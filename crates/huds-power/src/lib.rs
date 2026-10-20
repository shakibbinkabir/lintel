// Levels and battery — a Lintel feature. The contract is docs/ARCHITECTURE.md §2:
// nothing runs before `start`, everything is gone after `stop`, and every
// command of the feature arrives through `call`.
//
// This side only listens and tells the page: the volume and microphone of the
// default devices (svc-audio), the panel's brightness (WMI), power and the
// window in front (svc-sysevents). The page turns them into notices.
//
// `quiet` is news for the page's state and not a notice: our own write, a device
// switch, and the level the new device says by itself right after one.
//
// Events: `volume` and `mic` { level, mute, quiet }, `brightness` { level, displays },
// `power` { ac, percent, charging, saver }, `fullscreen` { on }, `micUse` { apps: [name], camera: [name] }
// (apps with the microphone open now), `chargeLimit` { percent }.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notch_core::{Ctx, Feature, Group, Manifest};
use serde_json::{json, Value};
use svc_audio::{Change, Flow, Volume};
use svc_sysevents::Foreground;
use tauri::Manager;
use windows::Devices::Power::{Battery, BatteryReport};
use windows::Foundation::IReference;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP};

pub mod brightness;
pub mod mic;

const ID: &str = "huds-power";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Levels and battery",
    group: Group::System,
    default_on: true,
    uses: &[],
    shortcuts: &[],
    tray: &[],
    defaults: r#"{ "volume": true, "mic": true, "brightness": true, "battery": true, "lowBattery": 20, "restOnBattery": false, "fullscreen": true, "micUse": true, "quietMic": false, "quietCamera": false, "chargeLimit": 0 }"#,
};

#[derive(Default)]
pub struct HudsPower {
    running: Mutex<Option<Running>>,
    shared: Arc<Shared>,
    /// The microphone watcher: there only while the setting is on.
    mic: Mutex<Option<mic::Watch>>,
}

/// The charge-limit reminder: one notice per plug-in session. An unplug, or a drop of 5 points
/// below the limit, arms it again. A reading without a charge changes nothing.
#[derive(Default)]
pub struct ChargeLimit {
    told: bool,
}

impl ChargeLimit {
    /// `limit` 0 is off. True when the reminder is due now.
    pub fn feed(&mut self, limit: u8, ac: bool, percent: Option<u8>) -> bool {
        if limit == 0 || !ac {
            self.told = false;
            return false;
        }
        let Some(p) = percent else { return false };
        if self.told && p + 5 <= limit {
            self.told = false;
        }
        if !self.told && p >= limit {
            self.told = true;
            return true;
        }
        false
    }
}

/// How long after the devices changed a level that moves is the new device's doing.
pub const SWITCH_QUIET: Duration = Duration::from_secs(3);

/// An output that has just become the default says its own level a moment later
/// (earbuds that connect report theirs): nobody touched the volume, so it is
/// news for the page's state and not a notice. A volume key pressed since the
/// change makes the level the user's again.
// debt: the Windows slider and a headset button within that time are taken for the device too.
#[derive(Default)]
pub struct Switch {
    at: Option<Instant>,
}

impl Switch {
    /// The devices changed at `now`.
    pub fn devices(&mut self, now: Instant) {
        self.at = Some(now);
    }

    /// True when a level told at `now` is the switch's own. `key`: a volume key went down since the switch.
    pub fn quiet(&mut self, now: Instant, key: bool) -> bool {
        if key {
            self.at = None;
        }
        self.at.is_some_and(|at| now.duration_since(at) < SWITCH_QUIET)
    }
}

/// A volume key is down, or went down since this was last asked. Every key is
/// asked each time: asking is what makes Windows forget the press.
fn volume_key() -> bool {
    [VK_VOLUME_MUTE, VK_VOLUME_DOWN, VK_VOLUME_UP].map(|vk| unsafe { GetAsyncKeyState(i32::from(vk.0)) } != 0).contains(&true)
}

/// What the event threads read and write.
#[derive(Default)]
struct Shared {
    /// The setting: the island stays away while a full-screen app is in front.
    hide: AtomicBool,
    /// What the page was last told.
    fullscreen: AtomicBool,
    /// The island's window, to know which monitor it is on.
    island: AtomicIsize,
    /// Counts power events, so that only the last of a burst is told.
    power_turn: AtomicU64,
    limit: Mutex<ChargeLimit>,
}

/// Dropping it ends the audio thread and the events window.
struct Running {
    _audio: svc_audio::Sub,
    _events: [svc_sysevents::Sub; 2],
    brightness_stop: Arc<AtomicBool>,
}

impl Shared {
    /// Tells the page when the answer changed.
    fn front(&self, cx: &Ctx, front: &Foreground) {
        let on = self.hide.load(Relaxed) && front.fullscreen && same_monitor(front.hwnd, self.island.load(Relaxed));
        if self.fullscreen.swap(on, Relaxed) != on {
            cx.emit(ID, "fullscreen", json!({ "on": on }));
        }
    }
}

impl Shared {
    /// Tells the page when the battery has reached the limit the user set.
    fn charge_limit(&self, cx: &Ctx) {
        let limit = cx.settings(ID)["chargeLimit"].as_u64().unwrap_or(0).min(100) as u8;
        let p = svc_sysevents::power_status();
        if self.limit.lock().unwrap().feed(limit, p.ac, p.percent) {
            cx.emit(ID, "chargeLimit", json!({ "percent": p.percent }));
        }
    }
}

/// The microphone and camera watcher follows the settings: any of them on loads it, all off drops it.
fn sync_mic(cx: &Ctx, slot: &Mutex<Option<mic::Watch>>, settings: &Value) {
    let on = ["micUse", "quietMic", "quietCamera"].iter().any(|k| settings[*k].as_bool().unwrap_or(k == &"micUse"));
    let mut slot = slot.lock().unwrap();
    if on && slot.is_none() {
        let cx = cx.clone();
        *slot = mic::watch(move |apps, camera| cx.emit(ID, "micUse", json!({ "apps": apps, "camera": camera })));
    } else if !on && slot.take().is_some() {
        cx.emit(ID, "micUse", json!({ "apps": Vec::<String>::new(), "camera": Vec::<String>::new() }));
    }
}

/// How many displays are on, so that the brightness HUD can say which one it is for.
fn monitors() -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
    unsafe { GetSystemMetrics(SM_CMONITORS) }
}

/// A full-screen app on another monitor leaves the island alone.
fn same_monitor(a: isize, b: isize) -> bool {
    let monitor = |hwnd: isize| unsafe { MonitorFromWindow(HWND(hwnd as *mut _), MONITOR_DEFAULTTONEAREST) };
    b == 0 || monitor(a) == monitor(b)
}

fn event_of(flow: Flow) -> &'static str {
    if flow == Flow::Render { "volume" } else { "mic" }
}

/// `quiet`: news for the page's state, not for the user (a device switch, our own write).
fn level_json(volume: &Volume, quiet: bool) -> Value {
    json!({ "level": volume.scalar, "mute": volume.mute, "quiet": quiet })
}

fn power_json() -> Value {
    let p = svc_sysevents::power_status();
    json!({ "ac": p.ac, "percent": p.percent, "charging": p.charging, "saver": p.saver })
}

/// The power status with what the battery itself reports: minutes at the
/// present rate (to full on the charger, to empty on battery) and how much of
/// its design capacity it still holds. Both are null where the firmware gives
/// no figures.
pub fn battery_json() -> Value {
    let report = Battery::AggregateBattery().and_then(|b| b.GetReport()).ok();
    let read = |get: fn(&BatteryReport) -> windows::core::Result<IReference<i32>>| {
        report.as_ref().and_then(|r| get(r).ok()).and_then(|v| v.Value().ok()).map(i64::from)
    };
    let rate = read(BatteryReport::ChargeRateInMilliwatts);
    let left = read(BatteryReport::RemainingCapacityInMilliwattHours);
    let full = read(BatteryReport::FullChargeCapacityInMilliwattHours);
    let design = read(BatteryReport::DesignCapacityInMilliwattHours);
    let minutes = match (rate, left, full) {
        (Some(rate), Some(left), Some(full)) if rate > 0 && full > left => Some((full - left) * 60 / rate),
        (Some(rate), Some(left), _) if rate < 0 => Some(left * 60 / -rate),
        _ => None,
    };
    let mut out = power_json();
    // A rate near zero gives days: not worth showing.
    out["minutes"] = json!(minutes.filter(|m| (1..=48 * 60).contains(m)));
    out["health"] = json!(match (full, design) {
        (Some(full), Some(design)) if design > 0 => Some((full * 100 / design).min(100)),
        _ => None,
    });
    out
}

impl Feature for HudsPower {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let mut running = self.running.lock().unwrap();
        if running.is_some() {
            return Ok(());
        }
        let shared = &self.shared;
        shared.hide.store(cx.settings(ID)["fullscreen"].as_bool().unwrap_or(true), Relaxed);
        // Asked once, here. From an event thread the question waits for the
        // main thread, which at quit is waiting for that very thread to end.
        let island = cx.app().get_webview_window("island").and_then(|w| w.hwnd().ok());
        shared.island.store(island.map_or(0, |hwnd| hwnd.0 as isize), Relaxed);

        let audio = {
            let cx = cx.clone();
            let switch = Mutex::new(Switch::default());
            svc_audio::on_endpoint_change(move |change| match change {
                Change::Volume { flow, volume, own } => {
                    let quiet = *own || switch.lock().unwrap().quiet(Instant::now(), volume_key());
                    cx.emit(ID, event_of(*flow), level_json(volume, quiet));
                }
                Change::Devices => {
                    // A press from before the switch says nothing about a level after it.
                    volume_key();
                    switch.lock().unwrap().devices(Instant::now());
                    for flow in [Flow::Render, Flow::Capture] {
                        if let Ok(volume) = svc_audio::endpoint_volume(flow) {
                            cx.emit(ID, event_of(flow), level_json(&volume, true));
                        }
                    }
                }
            })
        };
        let power = {
            let (cx, shared) = (cx.clone(), shared.clone());
            svc_sysevents::on_power(move |_| {
                // A dock flaps between the charger and the battery, and
                // "charging" follows the plug by a moment: one event, a second
                // after the last of them.
                let turn = shared.power_turn.fetch_add(1, Relaxed) + 1;
                let (cx, shared) = (cx.clone(), shared.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(1));
                    if shared.power_turn.load(Relaxed) == turn {
                        cx.emit(ID, "power", power_json());
                        shared.charge_limit(&cx);
                    }
                });
            })
        };
        let front = {
            let (cx, shared) = (cx.clone(), shared.clone());
            svc_sysevents::on_foreground(move |front| shared.front(&cx, front))
        };
        shared.front(cx, &svc_sysevents::foreground());

        let brightness_stop = Arc::new(AtomicBool::new(false));
        {
            let (cx, stop) = (cx.clone(), brightness_stop.clone());
            std::thread::spawn(move || {
                let told = brightness::listen(&stop, |percent| {
                    cx.emit(ID, "brightness", json!({ "level": f32::from(percent) / 100.0, "displays": monitors() }));
                });
                // A PC without a built-in panel: no brightness notices, quietly.
                if let Err(err) = told {
                    notch_core::log::line(format!("{ID}: no brightness events: {}", err.message()));
                }
            });
        }

        sync_mic(cx, &self.mic, &cx.settings(ID));
        shared.charge_limit(cx);

        *running = Some(Running { _audio: audio, _events: [power, front], brightness_stop });
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        let Some(running) = self.running.lock().unwrap().take() else { return };
        self.mic.lock().unwrap().take();
        running.brightness_stop.store(true, Relaxed);
        self.shared.power_turn.fetch_add(1, Relaxed);
        drop(running);
        if self.shared.fullscreen.swap(false, Relaxed) {
            cx.emit(ID, "fullscreen", json!({ "on": false }));
        }
    }

    fn settings_changed(&self, cx: &Ctx, settings: &Value) {
        self.shared.hide.store(settings["fullscreen"].as_bool().unwrap_or(true), Relaxed);
        self.shared.front(cx, &svc_sysevents::foreground());
        if self.running.lock().unwrap().is_some() {
            sync_mic(cx, &self.mic, settings);
            self.shared.charge_limit(cx);
        }
    }

    fn call(&self, cx: &Ctx, method: &str, _args: Value) -> Result<Value, String> {
        match method {
            // The rule, asked again: the island does before an activity brings it out
            // (playback starting in a window that went full screen without a word).
            "fullscreen" => {
                self.shared.front(cx, &svc_sysevents::foreground());
                Ok(json!(self.shared.fullscreen.load(Relaxed)))
            }
            // Events only tell of changes: this is what the page starts from.
            "state" => Ok(json!({
                "power": power_json(),
                "micMute": svc_audio::endpoint_volume(Flow::Capture).ok().map(|v| v.mute),
                "fullscreen": self.shared.fullscreen.load(Relaxed),
                "micApps": self.mic.lock().unwrap().is_some().then(mic::in_use),
                "cameraApps": self.mic.lock().unwrap().is_some().then(mic::camera_in_use),
            })),
            "battery" => Ok(battery_json()),
            "batterySettings" => {
                notch_core::win::open_url("ms-settings:batterysaver");
                Ok(Value::Null)
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/huds-power/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(HudsPower::default())
}
