// Audio mixer — a Lintel feature. The contract is docs/ARCHITECTURE.md §2: nothing
// runs before `start`, everything is gone after `stop`, and every command of
// the feature arrives through `call`.
//
// Over crates/svc-audio: the level of each app that plays (read on demand, and
// watched only while the Mixer section is on screen), the default output and
// microphone with their levels, the output picker and its shortcut. Three
// things are done on the user's behalf and always handed back when the feature
// stops: every microphone muted at once, the volume lowered when headphones
// disconnect, the preferred microphone kept as the default. The default
// *output* changes only on a click or the shortcut, never on its own, except
// with the setting `auto` on: then the highest output of `priority` that is
// present becomes the default, each time with an `auto` event the page answers
// with a notice whose button calls `revertOutput` (which also pauses it).
//
// Calls: `state`, `devices`, `watch { on }`, `icon { id }`, `appVolume { id, level }`,
// `appMute { id, mute }`, `volume { flow, level }`, `mute { mute }`, `output { id }`,
// `calls { id }`, `nextOutput`, `muteMics { on? }`, `revertOutput { id }` (flow: "out" | "in").
// Events: `apps` [App], `devices` { … }, `level { flow, level, mute }`,
// `switched { name, ok, headphones }`, `auto { name, headphones, from: id|null }`, `mics { muted, partial, level }`, `guard { level }`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use notch_core::{Ctx, Feature, Group, Manifest, ShortcutDef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use svc_audio::{Change, Device, Flow, Role};

pub mod apps;

const ID: &str = "audio-mixer";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Audio mixer",
    group: Group::Media,
    default_on: true,
    uses: &[],
    shortcuts: &[
        ShortcutDef { id: "nextOutput", label: "Switch to the next sound output", default: Some("Ctrl+Alt+O") },
        ShortcutDef { id: "muteMics", label: "Mute or unmute all microphones", default: Some("Ctrl+Alt+U") },
    ],
    tray: &[],
    // pinned, order, hidden (id -> name), hideInactive: the page's own; switcher: the outputs the shortcut cycles
    // through, in order (none = all); guardPercent: 0–100; preferredMic: a device id, "" = whatever Windows picks
    defaults: r#"{ "pinned": [], "order": [], "hidden": {}, "hideInactive": false,
        "switcher": [], "switchNotice": true, "callsFollow": false,
        "guard": false, "guardPercent": 25, "preferredMic": "", "auto": false, "priority": [] }"#,
};

/// Two levels closer than this are the same level.
const NEAR: f32 = 0.005;
/// The preferred microphone takes the default back this many times in
/// `STEER_WINDOW`; past that another tool wants it more, and is left to win.
const STEER_MAX: usize = 3;
const STEER_WINDOW: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct AudioMixer {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The listener on the default devices: the life of the feature.
    endpoint: Mutex<Option<svc_audio::Sub>>,
    /// The sessions watcher: only while the Mixer section is on screen.
    watch: Mutex<Option<svc_audio::SessionSub>>,
    st: Mutex<St>,
}

#[derive(Default)]
struct St {
    /// False once `stop` has handed everything back: a late callback does nothing.
    on: bool,
    cfg: Cfg,
    known: apps::Known,
    /// The rows as last read: what `appVolume` and `icon` look an id up in.
    apps: Vec<apps::App>,
    /// Every microphone is muted, by us.
    muted: bool,
    owed_file: PathBuf,
    owed: Owed,
    /// The default output as last seen, to tell headphones that went away.
    last_out: Option<Device>,
    lowered: Option<Lowered>,
    /// The default microphone before the preferred one first took its place, and the one we set.
    mic_before: Option<String>,
    mic_set: Option<String>,
    steered: Vec<Instant>,
    /// Automatic output priority: the outputs present at the last look, whether
    /// the default was moved by hand since the set last changed, whether the
    /// revert button paused it, and the output it last switched to.
    out_ids: Vec<String>,
    manual: bool,
    paused: bool,
    auto_to: Option<String>,
}

/// The settings Rust acts on (the rest are the page's).
#[derive(Default, Clone, PartialEq)]
struct Cfg {
    switcher: Vec<String>,
    calls_follow: bool,
    guard: bool,
    guard_level: f32,
    preferred_mic: String,
    auto: bool,
    priority: Vec<String>,
}

impl Cfg {
    fn of(v: &Value) -> Self {
        let ids = |key: &str| v[key].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect());
        Self {
            switcher: ids("switcher").unwrap_or_default(),
            calls_follow: v["callsFollow"].as_bool().unwrap_or(false),
            guard: v["guard"].as_bool().unwrap_or(false),
            guard_level: (v["guardPercent"].as_f64().unwrap_or(25.0) as f32 / 100.0).clamp(0.0, 1.0),
            preferred_mic: v["preferredMic"].as_str().unwrap_or_default().to_owned(),
            auto: v["auto"].as_bool().unwrap_or(false),
            priority: ids("priority").unwrap_or_default(),
        }
    }
}

/// What this app owes the user, kept on disk so that a crash cannot lose it:
/// the microphones it muted. One that is unplugged while muted keeps its place
/// here and is opened when it returns, in this run or a later one.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Owed {
    claims: Vec<String>,
}

/// An output whose level this app lowered when headphones went away.
struct Lowered {
    device: String,
    set: f32,
    before: f32,
}

fn default_of(list: &[Device]) -> Option<&Device> {
    list.iter().find(|d| d.default_for.contains(&Role::Console))
}

/// The output to make the default: the first of `priority` that is `present`,
/// if the current default (none, or one not listed) ranks below it.
pub fn auto_target(priority: &[String], present: &[String], current: Option<&str>) -> Option<String> {
    let rank = |id: &str| priority.iter().position(|p| p == id).unwrap_or(usize::MAX);
    let best = priority.iter().find(|p| present.contains(p))?;
    current.is_none_or(|c| rank(c) > rank(best)).then(|| best.clone())
}

/// By what the device says it is (3 headphones, 5 headset); a USB dongle says
/// "speakers", so its name is asked too.
fn headphones(d: &Device) -> bool {
    const NAMES: [&str; 14] = [
        "headphone", "headset", "earphone", "earbud", "airpod", "earpod", "galaxy buds", "pixel buds", "beats",
        "bose qc", "sony wh", "sony wf", "jabra", "soundcore",
    ];
    let name = d.name.to_lowercase();
    matches!(d.form_factor, 3 | 5) || NAMES.iter().any(|n| name.contains(n))
}

/// The output everything plays on; with `calls`, call apps too.
fn set_output(id: &str, calls: bool) -> Result<(), String> {
    svc_audio::set_default(id, Role::Console)?;
    svc_audio::set_default(id, Role::Multimedia)?;
    if calls {
        svc_audio::set_default(id, Role::Communications)?;
    }
    Ok(())
}

fn set_mic(id: &str) -> Result<(), String> {
    [Role::Console, Role::Multimedia, Role::Communications].into_iter().try_for_each(|role| svc_audio::set_default(id, role))
}

fn flow_name(flow: Flow) -> &'static str {
    if flow == Flow::Render { "out" } else { "in" }
}

fn can_switch() -> bool {
    static CAN: OnceLock<bool> = OnceLock::new();
    *CAN.get_or_init(svc_audio::can_set_default)
}

// ── What is done on the user's behalf, and handed back ────────────────────────

impl St {
    fn save(&self) {
        if let Err(err) = notch_core::storage::json_save_atomic(&self.owed_file, &self.owed) {
            notch_core::log::line(format!("{ID}: the muted microphones could not be written down: {err}"));
        }
    }

    /// Mutes every microphone that is open. One that is muted already is not
    /// claimed, so that unmuting never opens what the user closed. True when
    /// some microphone would not be muted.
    fn mute_mics(&mut self) -> bool {
        let open: Vec<String> = svc_audio::devices(Flow::Capture)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| svc_audio::device_volume(&d.id).is_ok_and(|v| !v.mute))
            .map(|d| d.id)
            .collect();
        // Written down before the microphones are touched.
        for id in &open {
            if !self.owed.claims.contains(id) {
                self.owed.claims.push(id.clone());
            }
        }
        self.muted = true;
        self.save();
        let refused: Vec<&String> = open.iter().filter(|id| svc_audio::set_device_mute(id, true).is_err()).collect();
        if !refused.is_empty() {
            self.owed.claims.retain(|id| !refused.contains(&id));
            self.save();
        }
        !refused.is_empty()
    }

    /// Opens the microphones this app muted. One that will not open (it is
    /// unplugged) keeps its claim and is tried again when the devices change.
    /// True when a microphone that is here stays muted.
    fn release_mics(&mut self) -> bool {
        self.muted = false;
        if self.owed.claims.is_empty() {
            return false;
        }
        let here: Vec<String> = svc_audio::devices(Flow::Capture).unwrap_or_default().into_iter().map(|d| d.id).collect();
        let mut stuck = false;
        self.owed.claims.retain(|id| {
            let kept = svc_audio::set_device_mute(id, false).is_err();
            stuck |= kept && here.contains(id);
            kept
        });
        self.save();
        stuck
    }

    /// Automatic output priority. A change of the set of outputs (plugged in,
    /// gone) ends a pause and a hand-made choice; a default moved with the set
    /// unchanged, by anyone but us, is a hand-made choice. Returns the outputs
    /// as they stand after.
    fn auto(&mut self, cx: &Ctx, outs: Vec<Device>) -> Vec<Device> {
        let mut ids: Vec<String> = outs.iter().map(|d| d.id.clone()).collect();
        ids.sort();
        let now = default_of(&outs).map(|d| d.id.clone());
        if ids != self.out_ids {
            self.out_ids = ids;
            (self.manual, self.paused, self.auto_to) = (false, false, None);
        } else if self.last_out.as_ref().is_some_and(|l| Some(&l.id) != now.as_ref()) && now != self.auto_to {
            self.manual = true;
        }
        if !self.cfg.auto || self.manual || self.paused || !can_switch() {
            return outs;
        }
        let Some(to) = auto_target(&self.cfg.priority, &self.out_ids, now.as_deref()) else { return outs };
        let Some(target) = outs.iter().find(|d| d.id == to) else { return outs };
        if set_output(&to, self.cfg.calls_follow).is_err() {
            return outs;
        }
        self.auto_to = Some(to);
        cx.emit(ID, "auto", json!({ "name": target.name, "headphones": headphones(target), "from": now }));
        svc_audio::devices(Flow::Render).unwrap_or(outs)
    }

    /// Headphones that were the output are gone and something else plays now:
    /// its level comes down, once. When headphones are the output again, it
    /// goes back up.
    fn guard(&mut self, cx: &Ctx, outs: &[Device]) {
        let now = default_of(outs).cloned();
        let before = std::mem::replace(&mut self.last_out, now.clone());
        let (Some(before), Some(now)) = (before, now) else { return };
        if !self.cfg.guard || before.id == now.id {
            return;
        }
        if headphones(&now) {
            self.restore_volume();
        } else if headphones(&before)
            && !outs.iter().any(|d| d.id == before.id)
            && self.lowered.as_ref().is_none_or(|l| l.device != now.id)
        {
            self.restore_volume();
            let level = self.cfg.guard_level;
            let Ok(was) = svc_audio::device_volume(&now.id) else { return };
            // Only ever down.
            if was.scalar > level + NEAR && svc_audio::set_device_volume(&now.id, level).is_ok() {
                self.lowered = Some(Lowered { device: now.id, set: level, before: was.scalar });
                cx.emit(ID, "guard", json!({ "level": level }));
            }
        }
    }

    /// A level this app lowered goes back up only while it is still the level
    /// the app set: anything else is the user's choice since, and it wins.
    fn restore_volume(&mut self) {
        let Some(l) = self.lowered.take() else { return };
        if svc_audio::device_volume(&l.device).is_ok_and(|v| (v.scalar - l.set).abs() < NEAR) {
            let _ = svc_audio::set_device_volume(&l.device, l.before);
        }
    }

    /// The preferred microphone, when it is here, takes the default back from
    /// whatever took it.
    fn steer(&mut self, ins: &[Device]) {
        let want = self.cfg.preferred_mic.clone();
        if want.is_empty() || !ins.iter().any(|d| d.id == want) {
            return;
        }
        let now = default_of(ins).map(|d| d.id.clone());
        if now.as_deref() == Some(want.as_str()) {
            return;
        }
        self.steered.retain(|at| at.elapsed() < STEER_WINDOW);
        if self.steered.len() >= STEER_MAX {
            return;
        }
        self.steered.push(Instant::now());
        if set_mic(&want).is_ok() {
            if self.mic_before.is_none() {
                self.mic_before = now;
            }
            self.mic_set = Some(want);
        }
    }

    /// The microphone that was the default before the preferred one took its
    /// place comes back, if ours is still the default and the other is here.
    fn restore_mic(&mut self) {
        let (Some(before), Some(set)) = (self.mic_before.take(), self.mic_set.take()) else { return };
        let ins = svc_audio::devices(Flow::Capture).unwrap_or_default();
        if before != set && default_of(&ins).is_some_and(|d| d.id == set) && ins.iter().any(|d| d.id == before) {
            let _ = set_mic(&before);
        }
    }
}

// ── The feature at work ───────────────────────────────────────────────────────

impl Inner {
    /// After the devices changed, at start and after a setting changed.
    fn settle(&self, cx: &Ctx) {
        let mut st = self.st.lock().unwrap();
        if !st.on {
            return;
        }
        // A microphone that arrives while all are muted is muted too; one that
        // comes back still muted from before is opened.
        if st.muted {
            st.mute_mics();
        } else {
            st.release_mics();
        }
        if let Ok(outs) = svc_audio::devices(Flow::Render) {
            let outs = st.auto(cx, outs);
            st.guard(cx, &outs);
        }
        if let Ok(ins) = svc_audio::devices(Flow::Capture) {
            st.steer(&ins);
        }
    }

    /// On the audio thread.
    fn changed(&self, cx: &Ctx, change: &Change) {
        match change {
            Change::Volume { flow, volume, own } => {
                // The default microphone was opened from outside (a headset
                // button, Windows Settings): the user is done with the mute.
                // Read again first: a level an app set a moment before the mute
                // is told after it, and says "open" of a microphone that is not.
                let opened = *flow == Flow::Capture && !own && !volume.mute && self.st.lock().unwrap().muted;
                if opened && svc_audio::endpoint_volume(Flow::Capture).is_ok_and(|v| !v.mute) {
                    self.set_mics(cx, Some(false));
                }
                cx.emit(ID, "level", json!({ "flow": flow_name(*flow), "level": volume.scalar, "mute": volume.mute }));
            }
            Change::Devices => {
                self.settle(cx);
                cx.emit(ID, "devices", self.devices_json());
            }
        }
    }

    /// Mutes every microphone or opens them again; `None` flips.
    fn set_mics(&self, cx: &Ctx, on: Option<bool>) -> Value {
        let (muted, partial) = {
            let mut st = self.st.lock().unwrap();
            let on = on.unwrap_or(!st.muted);
            if !st.on || on == st.muted {
                return json!({ "muted": st.muted, "partial": false });
            }
            let partial = if on { st.mute_mics() } else { st.release_mics() };
            (st.muted, partial)
        };
        // `level`: what the notice draws, the microphone's own once it is open again.
        let level = if muted { 0.0 } else { svc_audio::endpoint_volume(Flow::Capture).map_or(1.0, |v| v.scalar) };
        let said = json!({ "muted": muted, "partial": partial, "level": level });
        cx.emit(ID, "mics", said.clone());
        said
    }

    /// The shortcut: on to the next output of the ring, and the page names it.
    fn next_output(&self, cx: &Ctx) -> Result<Value, String> {
        let cfg = self.st.lock().unwrap().cfg.clone();
        let outs = svc_audio::devices(Flow::Render)?;
        let now = default_of(&outs);
        let ring: Vec<&Device> = if cfg.switcher.is_empty() {
            // `devices` lists the default first, an order that moves with every switch.
            let mut all: Vec<&Device> = outs.iter().collect();
            all.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
            all
        } else {
            let mut seen = HashSet::new();
            let chosen = cfg.switcher.iter().filter(|id| seen.insert(id.as_str()));
            chosen.filter_map(|id| outs.iter().find(|d| &d.id == id)).collect()
        };
        let next = match now.and_then(|now| ring.iter().position(|d| d.id == now.id)) {
            // Alone in the ring: it stays, and is named all the same.
            Some(at) => ring.get((at + 1) % ring.len()),
            None => ring.first(),
        };
        let said = match next {
            Some(next) => {
                let ok = now.is_some_and(|now| now.id == next.id) || set_output(&next.id, cfg.calls_follow).is_ok();
                json!({ "name": next.name, "ok": ok, "headphones": headphones(next) })
            }
            None => json!({ "name": "", "ok": false, "headphones": false }),
        };
        cx.emit(ID, "switched", said.clone());
        Ok(said)
    }

    fn watch(self: &Arc<Self>, cx: &Ctx, on: bool) {
        let mut watch = self.watch.lock().unwrap();
        if on && watch.is_none() {
            let (cx, inner) = (cx.clone(), self.clone());
            *watch = Some(svc_audio::on_sessions(move || cx.emit(ID, "apps", inner.apps())));
        } else if !on {
            let ended = watch.take();
            drop(watch);
            drop(ended);
        }
    }

    fn apps(&self) -> Vec<apps::App> {
        let sessions = svc_audio::sessions(None).unwrap_or_default();
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        st.apps = apps::group(&sessions, &mut st.known);
        st.apps.clone()
    }

    fn pids(&self, id: &str) -> Result<Vec<u32>, String> {
        let st = self.st.lock().unwrap();
        st.apps.iter().find(|a| a.id == id).map(|a| a.pids.clone()).ok_or_else(|| "The app is no longer playing.".into())
    }

    /// The app's icon as a data URL; null when it has none.
    fn icon(&self, id: &str) -> Value {
        let exe = self.st.lock().unwrap().apps.iter().find(|a| a.id == id).map(|a| a.exe.clone());
        let png = exe.filter(|exe| !exe.is_empty()).and_then(|exe| svc_shell::icon(&exe, 32).ok());
        json!(png.map(|png| format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))))
    }

    fn devices_json(&self) -> Value {
        let level = |flow| svc_audio::endpoint_volume(flow).ok().map(|v| json!({ "level": v.scalar, "mute": v.mute }));
        let list = |flow| -> Vec<Value> {
            let devices = svc_audio::devices(flow).unwrap_or_default();
            let row = |d: &Device| {
                json!({
                    "id": d.id,
                    "name": d.name,
                    "default": d.default_for.contains(&Role::Console),
                    "calls": d.default_for.contains(&Role::Communications),
                    "headphones": headphones(d),
                })
            };
            devices.iter().map(row).collect()
        };
        json!({
            "canSwitch": can_switch(),
            "out": level(Flow::Render),
            "mic": level(Flow::Capture),
            "outputs": list(Flow::Render),
            "inputs": list(Flow::Capture),
            "micsMuted": self.st.lock().unwrap().muted,
        })
    }
}

impl Feature for AudioMixer {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let inner = &self.inner;
        let mut endpoint = inner.endpoint.lock().unwrap();
        if endpoint.is_some() {
            return Ok(());
        }
        {
            let mut st = inner.st.lock().unwrap();
            st.on = true;
            st.cfg = Cfg::of(&cx.settings(ID));
            st.owed_file = cx.data_dir(ID).join("owed.json");
            st.owed = notch_core::storage::json_load(&st.owed_file);
        }
        *endpoint = Some({
            let (cx, inner) = (cx.clone(), inner.clone());
            svc_audio::on_endpoint_change(move |change| inner.changed(&cx, change))
        });
        // Off the launch path: where the output stands, the preferred
        // microphone, and any microphone a crash left muted.
        let (cx, inner) = (cx.clone(), inner.clone());
        std::thread::spawn(move || inner.settle(&cx));
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        let inner = &self.inner;
        let subs = (inner.endpoint.lock().unwrap().take(), inner.watch.lock().unwrap().take());
        // No lock is held while the threads end: a callback under way may want one.
        drop(subs);
        let mut st = inner.st.lock().unwrap();
        if !st.on {
            return;
        }
        // Everything done on the user's behalf is handed back.
        st.release_mics();
        st.restore_volume();
        st.restore_mic();
        let owed_file = std::mem::take(&mut st.owed_file);
        let owed = std::mem::take(&mut st.owed);
        *st = St { owed_file, owed, ..St::default() };
    }

    fn settings_changed(&self, cx: &Ctx, settings: &Value) {
        {
            let mut st = self.inner.st.lock().unwrap();
            let cfg = Cfg::of(settings);
            // Pins, the order, the hide list: the page's own.
            if cfg == st.cfg {
                return;
            }
            if !cfg.guard {
                st.restore_volume();
            }
            if cfg.auto && !st.cfg.auto {
                (st.manual, st.paused) = (false, false);
            }
            if cfg.preferred_mic != st.cfg.preferred_mic {
                st.steered.clear();
                // "Whatever Windows picks": the user took over, nothing is put back later.
                if cfg.preferred_mic.is_empty() {
                    st.mic_before = None;
                    st.mic_set = None;
                }
            }
            st.cfg = cfg;
        }
        self.inner.settle(cx);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let inner = &self.inner;
        let id = || args["id"].as_str().unwrap_or_default().to_owned();
        let level = || (args["level"].as_f64().unwrap_or(0.0) as f32).clamp(0.0, 1.0);
        match method {
            // Events only tell of changes: this is what the page starts from.
            "state" => {
                let mut state = inner.devices_json();
                state["apps"] = json!(inner.apps());
                Ok(state)
            }
            "devices" => Ok(inner.devices_json()),
            "watch" => {
                inner.watch(cx, args["on"].as_bool().unwrap_or(false));
                Ok(Value::Null)
            }
            "icon" => Ok(inner.icon(&id())),
            "appVolume" => svc_audio::set_session_volume(&inner.pids(&id())?, level()).map(|()| Value::Null),
            "appMute" => {
                svc_audio::set_session_mute(&inner.pids(&id())?, args["mute"].as_bool().unwrap_or(false)).map(|()| Value::Null)
            }
            "volume" if args["flow"] == "in" => svc_audio::set_volume(Flow::Capture, level()).map(|()| Value::Null),
            // The service mutes at 0 and opens above it. (A microphone's level never opens one.)
            "volume" => svc_audio::set_volume(Flow::Render, level()).map(|()| Value::Null),
            // The button of the notice for an automatic switch: back, and no more automatic ones until the devices change.
            "revertOutput" => {
                let calls = {
                    let mut st = inner.st.lock().unwrap();
                    st.paused = true;
                    st.cfg.calls_follow
                };
                set_output(&id(), calls).map(|()| Value::Null)
            }
            "mute" => svc_audio::set_mute(Flow::Render, args["mute"].as_bool().unwrap_or(false)).map(|()| Value::Null),
            // The two places the default output changes: a click on the picker, and the shortcut.
            "output" => {
                let calls = inner.st.lock().unwrap().cfg.calls_follow;
                set_output(&id(), calls).map(|()| Value::Null)
            }
            "calls" => svc_audio::set_default(&id(), Role::Communications).map(|()| Value::Null),
            "nextOutput" => inner.next_output(cx),
            "muteMics" => Ok(inner.set_mics(cx, args["on"].as_bool())),
            "shortcut" => match args["id"].as_str() {
                Some("nextOutput") => inner.next_output(cx),
                Some("muteMics") => Ok(inner.set_mics(cx, None)),
                _ => Err("unknown shortcut".into()),
            },
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/audio-mixer/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(AudioMixer::default())
}

#[cfg(test)]
mod tests {
    use super::auto_target;

    #[test]
    fn priority() {
        let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let p = v(&["dock", "bt", "spk"]);
        assert_eq!(auto_target(&p, &v(&["spk", "bt"]), Some("spk")).as_deref(), Some("bt"));
        assert_eq!(auto_target(&p, &v(&["spk", "bt"]), Some("bt")), None);
        assert_eq!(auto_target(&p, &v(&["spk", "bt", "dock"]), Some("bt")).as_deref(), Some("dock"));
        assert_eq!(auto_target(&p, &v(&["spk", "bt"]), None).as_deref(), Some("bt"));
        assert_eq!(auto_target(&p, &v(&["hdmi"]), Some("hdmi")), None);
        assert_eq!(auto_target(&p, &v(&["hdmi", "spk"]), Some("hdmi")).as_deref(), Some("spk"));
    }
}
