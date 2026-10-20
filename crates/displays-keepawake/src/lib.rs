// Displays and keep awake — a Lintel feature. The contract is docs/ARCHITECTURE.md §2:
// nothing runs before `start`, everything is gone after `stop`, and every
// command of the feature arrives through `call`.
//
// Idle, this does nothing: no keep-awake request, no thread, no window. What it
// changes it puts back: the request is released when a session ends, when the
// feature is switched off and at quit (it dies with the process anyway); a
// dimming overlay is a click-through window that is closed when the level is
// back to full, when the feature stops and at quit; a radio is switched on again
// at wake only if this feature switched it off.
//
// Events: `session` { active, endsAt, startedAt, auto }, `ended` { reason },
// `brightness` { id, name, level, failed?, shortcut? }, `dim` { label, alpha },
// `displays` {} (the set of displays changed).

pub mod display;
pub mod power;
pub mod radio;
pub mod wall;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use display::{Display, Method};
use notch_core::{Ctx, Feature, Group, Manifest, Monitor, ShortcutDef, WindowSpec};
use radio::Kind;
use serde_json::{json, Value};
use svc_sysevents::Session as Sleep;
use tauri::{PhysicalPosition, PhysicalSize};

const ID: &str = "displays-keepawake";

/// Where the slider turns from the backlight into the overlay (extra dimming).
const SPLIT: f32 = 0.15;
/// The overlay never gets blacker than this: at least 15 % of the picture stays.
const MAX_ALPHA: f32 = 0.85;
/// Brightness keys: one step is 1/16 of the range, the system's own.
const STEP: f32 = 1.0 / 16.0;

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Displays and keep awake",
    group: Group::System,
    default_on: true,
    uses: &[],
    shortcuts: &[
        ShortcutDef { id: "toggle", label: "Start or stop keep awake", default: None },
        ShortcutDef { id: "brighter", label: "Brighter (display under the pointer)", default: Some("Ctrl+Alt+PageUp") },
        ShortcutDef { id: "dimmer", label: "Dimmer (display under the pointer)", default: Some("Ctrl+Alt+PageDown") },
    ],
    tray: &[],
    // minutes: the last duration picked (0 = until stopped); batteryLimit: a session ends on battery at or below it (0 = never)
    defaults: r#"{ "minutes": 60, "allowDisplaySleep": false, "batteryLimit": 10, "jiggle": false, "jiggleMinutes": 5,
        "autoExternal": false, "autoPower": false, "autoAll": false, "autoAgents": false, "agentsMinutes": 5,
        "linkKeys": false,
        "extraDimming": true, "linkDisplays": false,
        "bluetoothSleep": false, "wifiSleep": false, "wallpaperFolder": "" }"#,
};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn flag(s: &Value, key: &str) -> bool {
    s[key].as_bool().unwrap_or(false)
}

/// Runs `f` on a thread of its own and waits at most `limit` for it.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok()
}

/// Only the last of a burst runs, `ms` after it.
fn later(turn: &Arc<AtomicU64>, ms: u64, f: impl FnOnce() + Send + 'static) {
    let mine = turn.fetch_add(1, Relaxed) + 1;
    let turn = turn.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        if turn.load(Relaxed) == mine {
            f();
        }
    });
}

// ── Keep awake ────────────────────────────────────────────────────────────────

struct Session {
    _request: power::Request,
    /// Dropping it ends the session's thread.
    _alive: mpsc::Sender<()>,
    started: u64,
    ends: Option<u64>,
    auto: bool,
}

/// One real pointer movement of one pixel and back, so that the idle timer restarts.
fn nudge() {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut at = POINT::default();
        if GetCursorPos(&mut at).is_err() {
            return;
        }
        let send = |dx: i32| {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        };
        send(1);
        std::thread::sleep(Duration::from_millis(80));
        // Back only if the pointer is where the nudge left it: never fight the user.
        let mut now = POINT::default();
        if GetCursorPos(&mut now).is_ok() && now.x == at.x + 1 && now.y == at.y {
            send(-1);
        }
    }
}

// ── Brightness ────────────────────────────────────────────────────────────────

/// What the backlight (percent) and the overlay (alpha) should be for a slider value.
fn plan(method: Method, internal: bool, extra: bool, s: f32) -> (Option<u8>, f32) {
    let s = s.clamp(0.0, 1.0);
    if method == Method::None {
        return (None, (1.0 - s) * MAX_ALPHA);
    }
    if !extra {
        return (Some((s * 100.0).round() as u8), 0.0);
    }
    // The built-in backlight is off at zero: its lowest step is 1.
    let floor = if internal { 1 } else { 0 };
    if s >= SPLIT {
        (Some((((s - SPLIT) / (1.0 - SPLIT) * 100.0).round() as u8).max(floor)), 0.0)
    } else {
        (Some(floor), (1.0 - s / SPLIT) * MAX_ALPHA)
    }
}

/// The slider value a backlight reading stands for (no overlay).
fn level_of(extra: bool, internal: bool, hw: u8) -> f32 {
    let hw = f32::from(hw) / 100.0;
    if extra && internal && hw <= 0.01 {
        SPLIT
    } else if extra {
        SPLIT + hw * (1.0 - SPLIT)
    } else {
        hw
    }
}

struct Job {
    method: Method,
    percent: u8,
    /// The slider value this write stands for; it becomes the last good one when it succeeds.
    level: f32,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<HashMap<String, Job>>,
    wake: Condvar,
    quit: AtomicBool,
}

#[derive(Default)]
struct Subs {
    display: Option<svc_sysevents::Sub>,
    power: Option<svc_sysevents::Sub>,
    sleep: Option<svc_sysevents::Sub>,
}

#[derive(Default)]
struct Inner {
    session: Mutex<Option<Session>>,
    /// A session started by a condition was stopped by hand: no new one until the conditions clear.
    suppressed: AtomicBool,
    /// How each display can be dimmed, as the last read found out.
    methods: Mutex<HashMap<String, Method>>,
    /// The slider value that last took (or was last shown), per display.
    levels: Mutex<HashMap<String, f32>>,
    /// The last backlight percent queued, to not write the same twice.
    queued: Mutex<HashMap<String, u8>>,
    fails: Mutex<HashMap<String, u8>>,
    /// display id -> window label of its overlay
    opened: Mutex<HashMap<String, String>>,
    alphas: Mutex<HashMap<String, f32>>,
    queue: Arc<Queue>,
    worker: Mutex<bool>,
    subs: Mutex<Subs>,
    display_turn: Arc<AtomicU64>,
    power_turn: Arc<AtomicU64>,
    agents_turn: Arc<AtomicU64>,
    /// An agent works now (told by the page), and when one last did (ms); the quiet minutes count from it.
    agents_busy: AtomicBool,
    agents_last: AtomicU64,
}

impl Inner {
    // ── sessions ──────────────────────────────────────────────────────────────

    fn session_json(&self) -> Value {
        match &*self.session.lock().unwrap() {
            Some(s) => json!({ "active": true, "startedAt": s.started, "endsAt": s.ends, "auto": s.auto }),
            None => json!({ "active": false }),
        }
    }

    fn battery_low(limit: u64) -> bool {
        let p = svc_sysevents::power_status();
        limit > 0 && !p.ac && p.percent.is_some_and(|c| u64::from(c) <= limit)
    }

    fn begin(self: &Arc<Self>, cx: &Ctx, minutes: u64, auto: bool) -> Result<(), String> {
        let s = cx.settings(ID);
        if auto && Self::battery_low(s["batteryLimit"].as_u64().unwrap_or(10)) {
            return Err("The battery is low.".into());
        }
        let request = power::Request::new(flag(&s, "allowDisplaySleep"))?;
        let (alive, gone) = mpsc::channel::<()>();
        let started = now_ms();
        // Replacing a session drops the old request after the new one exists: the PC never gets a gap.
        *self.session.lock().unwrap() =
            Some(Session { _request: request, _alive: alive, started, ends: (minutes > 0).then(|| started + minutes * 60_000), auto });
        cx.emit(ID, "session", self.session_json());
        let (me, cx) = (self.clone(), cx.clone());
        std::thread::spawn(move || me.watch(&cx, gone));
        Ok(())
    }

    /// The session's own thread: wakes for the end and every 30 s for the battery and the pointer.
    fn watch(self: &Arc<Self>, cx: &Ctx, gone: mpsc::Receiver<()>) {
        let mut jiggled = Instant::now();
        loop {
            let ends = self.session.lock().unwrap().as_ref().and_then(|s| s.ends);
            let wait = ends.map_or(30_000, |e| e.saturating_sub(now_ms()).clamp(250, 30_000));
            if !matches!(gone.recv_timeout(Duration::from_millis(wait)), Err(mpsc::RecvTimeoutError::Timeout)) {
                return;
            }
            let s = cx.settings(ID);
            let ends = self.session.lock().unwrap().as_ref().and_then(|s| s.ends);
            if ends.is_some_and(|e| now_ms() >= e) {
                self.end(cx, "done");
                return;
            }
            if Self::battery_low(s["batteryLimit"].as_u64().unwrap_or(10)) {
                self.end(cx, "battery");
                return;
            }
            let every = Duration::from_secs(s["jiggleMinutes"].as_u64().unwrap_or(5).clamp(1, 15) * 60);
            if flag(&s, "jiggle") && jiggled.elapsed() >= every {
                jiggled = Instant::now();
                nudge();
            }
        }
    }

    /// Releases the request. `reason`: "done", "battery", "auto" (conditions cleared), "manual", "off".
    fn end(self: &Arc<Self>, cx: &Ctx, reason: &str) {
        let Some(old) = self.session.lock().unwrap().take() else { return };
        drop(old);
        cx.emit(ID, "session", json!({ "active": false }));
        if matches!(reason, "done" | "battery") {
            cx.emit(ID, "ended", json!({ "reason": reason }));
        }
    }

    /// Whether the automatic conditions hold now; `None` when none is enabled.
    fn conditions(&self, cx: &Ctx) -> Option<bool> {
        let s = cx.settings(ID);
        let mut all = Vec::new();
        if flag(&s, "autoExternal") {
            // A failed read of the displays counts as "not connected", never as the last answer.
            all.push(display::list().iter().any(|d| !d.internal));
        }
        if flag(&s, "autoPower") {
            all.push(svc_sysevents::power_status().ac);
        }
        if flag(&s, "autoAgents") {
            let quiet = s["agentsMinutes"].as_u64().unwrap_or(5).min(120) * 60_000;
            all.push(self.agents_busy.load(Relaxed) || now_ms() < self.agents_last.load(Relaxed) + quiet);
        }
        if all.is_empty() {
            None
        } else if flag(&s, "autoAll") {
            Some(all.iter().all(|c| *c))
        } else {
            Some(all.iter().any(|c| *c))
        }
    }

    fn automate(self: &Arc<Self>, cx: &Ctx) {
        let ok = self.conditions(cx);
        let auto = self.session.lock().unwrap().as_ref().map(|s| s.auto);
        match (ok, auto) {
            (Some(true), None) => {
                if !self.suppressed.load(Relaxed) {
                    let _ = self.begin(cx, 0, true);
                }
            }
            (Some(true), _) => {}
            (Some(false), a) => {
                self.suppressed.store(false, Relaxed);
                if a == Some(true) {
                    self.end(cx, "auto");
                }
            }
            (None, a) => {
                self.suppressed.store(false, Relaxed);
                if a == Some(true) {
                    self.end(cx, "auto");
                }
            }
        }
    }

    // ── brightness ────────────────────────────────────────────────────────────

    fn extra(cx: &Ctx) -> bool {
        cx.settings(ID)["extraDimming"].as_bool().unwrap_or(true)
    }

    fn display_of(id: &str) -> Option<Display> {
        display::list().into_iter().find(|d| d.id == id)
    }

    fn method_of(&self, d: &Display) -> Method {
        let known = self.methods.lock().unwrap().get(&d.id).copied();
        known.unwrap_or_else(|| {
            let (m, _) = display::probe(d);
            self.methods.lock().unwrap().insert(d.id.clone(), m);
            m
        })
    }

    fn worker(self: &Arc<Self>, cx: &Ctx) {
        let mut running = self.worker.lock().unwrap();
        if *running {
            return;
        }
        *running = true;
        let (me, cx) = (self.clone(), cx.clone());
        std::thread::spawn(move || {
            let q = me.queue.clone();
            let mut last: HashMap<String, Instant> = HashMap::new();
            loop {
                let (id, job) = {
                    let mut jobs = q.jobs.lock().unwrap();
                    loop {
                        if q.quit.load(Relaxed) {
                            return;
                        }
                        if let Some(id) = jobs.keys().next().cloned() {
                            let job = jobs.remove(&id).unwrap();
                            break (id, job);
                        }
                        jobs = q.wake.wait(jobs).unwrap();
                    }
                };
                // 50 ms between two commands to one display: some monitors drop out when streamed faster.
                if let Some(t) = last.get(&id) {
                    std::thread::sleep(Duration::from_millis(50).saturating_sub(t.elapsed()));
                }
                let ok = Self::display_of(&id).is_some_and(|d| display::write(&d, job.method, job.percent));
                last.insert(id.clone(), Instant::now());
                me.written(&cx, &id, &job, ok);
            }
        });
    }

    /// A hardware write finished. A failure puts the slider back to the last level that took.
    fn written(self: &Arc<Self>, cx: &Ctx, id: &str, job: &Job, ok: bool) {
        if ok {
            self.fails.lock().unwrap().remove(id);
            self.levels.lock().unwrap().insert(id.into(), job.level);
            return;
        }
        let strikes = {
            let mut f = self.fails.lock().unwrap();
            let n = f.entry(id.into()).or_default();
            *n += 1;
            *n
        };
        self.queued.lock().unwrap().remove(id);
        // Two failures: the display's backlight is not ours to move; the overlay still is.
        if strikes >= 2 {
            self.methods.lock().unwrap().insert(id.into(), Method::None);
        }
        let Some(d) = Self::display_of(id) else { return };
        let good = self.levels.lock().unwrap().get(id).copied().unwrap_or(1.0);
        let (_, alpha) = plan(self.method_of(&d), d.internal, Self::extra(cx), good);
        self.overlay(cx, &d, alpha);
        cx.emit(ID, "brightness", json!({ "id": id, "name": d.name, "level": good, "failed": true, "method": self.method_of(&d).name() }));
        if strikes >= 2 {
            cx.emit(ID, "displays", json!({}));
        }
    }

    /// Moves one display to a slider value. The overlay follows at once; the backlight is queued.
    fn apply(self: &Arc<Self>, cx: &Ctx, d: &Display, s: f32) {
        let method = self.method_of(d);
        let (hw, alpha) = plan(method, d.internal, Self::extra(cx), s);
        // Brighter: the overlay goes first, so that a late backlight write never leaves the screen black.
        self.overlay(cx, d, alpha);
        match hw {
            Some(percent) => {
                if self.queued.lock().unwrap().insert(d.id.clone(), percent) == Some(percent) {
                    // Same backlight as already asked for: only the overlay moved.
                    self.levels.lock().unwrap().insert(d.id.clone(), s);
                    return;
                }
                self.worker(cx);
                self.queue.jobs.lock().unwrap().insert(d.id.clone(), Job { method, percent, level: s });
                self.queue.wake.notify_one();
            }
            None => {
                self.levels.lock().unwrap().insert(d.id.clone(), s);
            }
        }
    }

    /// The dimming overlay of a display: a click-through window, there only while alpha > 0.
    fn overlay(self: &Arc<Self>, cx: &Ctx, d: &Display, alpha: f32) {
        let label = format!("dim-{}", d.id.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>());
        let was = self.opened.lock().unwrap().contains_key(&d.id);
        if alpha < 0.004 {
            self.alphas.lock().unwrap().remove(&d.id);
            if was {
                cx.emit(ID, "dim", json!({ "label": label, "alpha": 0.0 }));
                cx.window().close(&label);
                self.opened.lock().unwrap().remove(&d.id);
                self.sync_subs(cx);
            }
            return;
        }
        self.alphas.lock().unwrap().insert(d.id.clone(), alpha);
        if !was {
            let spec = WindowSpec {
                label: label.clone(),
                feature: ID.into(),
                page: "dim".into(),
                w: 200.0,
                h: 200.0,
                frameless: true,
                topmost: true,
                no_activate: true,
                exclude_from_capture: true,
                transparent: true,
                monitor: Monitor::Primary,
            };
            match cx.window().open(spec) {
                Ok(win) => {
                    // Click-through before it is anywhere: it can never take the pointer or a key.
                    let _ = win.set_ignore_cursor_events(true);
                    let _ = win.set_position(PhysicalPosition::new(d.x, d.y));
                    let _ = win.set_size(PhysicalSize::new(d.w as u32, d.h as u32));
                }
                Err(e) => {
                    notch_core::log::line(format!("{ID}: dimming overlay: {e}"));
                    self.alphas.lock().unwrap().remove(&d.id);
                    return;
                }
            }
            self.opened.lock().unwrap().insert(d.id.clone(), label.clone());
            self.sync_subs(cx);
        }
        cx.emit(ID, "dim", json!({ "label": label, "alpha": alpha }));
    }

    fn close_overlays(&self, cx: &Ctx) {
        let all: Vec<String> = self.opened.lock().unwrap().drain().map(|(_, l)| l).collect();
        for label in all {
            cx.emit(ID, "dim", json!({ "label": label, "alpha": 0.0 }));
            cx.window().close(&label);
        }
        self.alphas.lock().unwrap().clear();
    }

    /// Displays came or went: overlays follow their display or go, the page reads again.
    fn displays_changed(self: &Arc<Self>, cx: &Ctx) {
        self.methods.lock().unwrap().clear();
        let now = display::list();
        let open: Vec<(String, String)> = self.opened.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (id, label) in open {
            match now.iter().find(|d| d.id == id) {
                Some(d) => {
                    if let Some(win) = tauri::Manager::get_webview_window(cx.app(), &label) {
                        let _ = win.set_position(PhysicalPosition::new(d.x, d.y));
                        let _ = win.set_size(PhysicalSize::new(d.w as u32, d.h as u32));
                    }
                }
                None => {
                    cx.emit(ID, "dim", json!({ "label": label, "alpha": 0.0 }));
                    cx.window().close(&label);
                    self.opened.lock().unwrap().remove(&id);
                    self.alphas.lock().unwrap().remove(&id);
                }
            }
        }
        // A write queued for a display that went away is never sent.
        self.queue.jobs.lock().unwrap().retain(|id, _| now.iter().any(|d| &d.id == id));
        self.queued.lock().unwrap().retain(|id, _| now.iter().any(|d| &d.id == id));
        self.sync_subs(cx);
        cx.emit(ID, "displays", json!({}));
        self.automate(cx);
    }

    /// The display under the pointer, one step brighter or dimmer.
    fn step(self: &Arc<Self>, cx: &Ctx, up: bool) {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut at = POINT::default();
        if unsafe { GetCursorPos(&mut at) }.is_err() {
            return;
        }
        // Only a display that is there and holds the pointer: no guess when the pointer is on none.
        let all = display::list();
        let count = all.len();
        let Some(d) = all.into_iter().find(|d| at.x >= d.x && at.x < d.x + d.w && at.y >= d.y && at.y < d.y + d.h) else { return };
        let mut next = 0.0;
        // With "linkKeys" every display moves by the same step, so the gaps between them stay (until one hits an end).
        let linked = flag(&cx.settings(ID), "linkKeys");
        for o in display::list().into_iter().filter(|o| o.id == d.id || linked) {
            let known = self.levels.lock().unwrap().get(&o.id).copied();
            let now = known.unwrap_or_else(|| {
                let (m, hw) = display::probe(&o);
                self.methods.lock().unwrap().insert(o.id.clone(), m);
                hw.map_or(1.0, |hw| level_of(Self::extra(cx), o.internal, hw))
            });
            let to = (now + if up { STEP } else { -STEP }).clamp(0.0, 1.0);
            self.apply(cx, &o, to);
            if o.id == d.id {
                next = to;
            }
        }
        cx.emit(ID, "brightness", json!({ "id": d.id, "name": d.name, "level": next, "shortcut": true, "count": count }));
    }

    /// One step now; then, while the keys stay down, a step every 100 ms. The shortcut registry
    /// registers hot keys without repeat, so it reports one press however long they are held.
    /// The keys watched are the ones bound in settings.json (read only), the defaults if none.
    fn hold_step(self: &Arc<Self>, cx: &Ctx, up: bool) {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        static HELD: AtomicBool = AtomicBool::new(false);
        self.step(cx, up);
        // A binding that cannot be read, or has a key we cannot watch, steps once.
        let Some(keys) = bound_keys(if up { "brighter" } else { "dimmer" }) else { return };
        if HELD.swap(true, Relaxed) {
            return;
        }
        let (me, cx) = (self.clone(), cx.clone());
        std::thread::spawn(move || {
            let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
            std::thread::sleep(std::time::Duration::from_millis(400));
            while keys.iter().all(|&(a, b)| down(a) || down(b)) {
                me.step(&cx, up);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            HELD.store(false, Relaxed);
        });
    }

    // ── radios at sleep ───────────────────────────────────────────────────────

    fn journal(cx: &Ctx) -> PathBuf {
        cx.data_dir(ID).join("radios.json")
    }

    /// Before sleep: the radios that are on go off, and the debt is written down first.
    fn radios_off(cx: &Ctx) {
        let s = cx.settings(ID);
        let path = Self::journal(cx);
        for (kind, key, name) in [(Kind::Bluetooth, "bluetoothSleep", "bluetooth"), (Kind::Wifi, "wifiSleep", "wifi")] {
            if !flag(&s, key) {
                continue;
            }
            if radio::states(kind).is_ok_and(|on| on.contains(&true)) {
                let mut owed: HashMap<String, bool> = notch_core::storage::json_load(&path);
                owed.insert(name.into(), true);
                let _ = notch_core::storage::json_save_atomic(&path, &owed);
                if let Err(e) = radio::set(kind, false) {
                    notch_core::log::line(format!("{ID}: {name} off: {e}"));
                }
            }
        }
    }

    /// At wake, at start and at quit: a radio this feature switched off comes back on,
    /// unless the user already did it.
    fn radios_back(cx: &Ctx) {
        let path = Self::journal(cx);
        let mut owed: HashMap<String, bool> = notch_core::storage::json_load(&path);
        owed.retain(|_, v| *v);
        if owed.is_empty() {
            return;
        }
        for (kind, name) in [(Kind::Bluetooth, "bluetooth"), (Kind::Wifi, "wifi")] {
            if !owed.contains_key(name) {
                continue;
            }
            // The radios are slow to come back after a wake: a few tries.
            for _ in 0..4 {
                match radio::states(kind) {
                    Ok(on) if on.contains(&true) || on.is_empty() => break,
                    Ok(_) if radio::set(kind, true).is_ok() => break,
                    _ => std::thread::sleep(Duration::from_secs(2)),
                }
            }
            owed.remove(name);
        }
        let _ = notch_core::storage::json_save_atomic(&path, &owed);
    }

    // ── what listens ──────────────────────────────────────────────────────────

    /// Subscribes to exactly what the settings and the open overlays need.
    fn sync_subs(self: &Arc<Self>, cx: &Ctx) {
        let s = cx.settings(ID);
        let want_display = flag(&s, "autoExternal") || !self.opened.lock().unwrap().is_empty();
        let want_power = flag(&s, "autoPower");
        let want_sleep = flag(&s, "bluetoothSleep") || flag(&s, "wifiSleep");
        let mut subs = self.subs.lock().unwrap();
        if !want_display {
            subs.display = None;
        } else if subs.display.is_none() {
            let (me, cx) = (self.clone(), cx.clone());
            subs.display = Some(svc_sysevents::on_display_change(move || {
                // A burst of changes is read once, after it.
                let (me2, cx2) = (me.clone(), cx.clone());
                later(&me.display_turn, 1500, move || me2.displays_changed(&cx2));
            }));
        }
        if !want_power {
            subs.power = None;
        } else if subs.power.is_none() {
            let (me, cx) = (self.clone(), cx.clone());
            subs.power = Some(svc_sysevents::on_power(move |_| {
                let (me2, cx2) = (me.clone(), cx.clone());
                later(&me.power_turn, 1000, move || me2.automate(&cx2));
            }));
        }
        if !want_sleep {
            subs.sleep = None;
        } else if subs.sleep.is_none() {
            let cx = cx.clone();
            subs.sleep = Some(svc_sysevents::on_session(move |ev| match ev {
                // Held here for up to 2.5 s, so that the radios are off before Windows suspends.
                Sleep::Sleep => {
                    let cx = cx.clone();
                    let _ = within(Duration::from_millis(2500), move || Self::radios_off(&cx));
                }
                Sleep::Resume => {
                    let cx = cx.clone();
                    std::thread::spawn(move || Self::radios_back(&cx));
                }
                _ => {}
            }));
        }
    }
}

// ── The feature ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct DisplaysKeepawake {
    inner: Arc<Inner>,
    running: AtomicBool,
}

/// Virtual keys (a pair: left or right of a modifier) of an accelerator such as "Ctrl+Alt+PageUp".
pub fn vks_of(keys: &str) -> Option<Vec<(u16, u16)>> {
    let mut out = Vec::new();
    for t in keys.split('+').map(str::trim) {
        let up = t.to_ascii_uppercase();
        let vk = match up.as_str() {
            "CTRL" | "CONTROL" | "COMMANDORCONTROL" | "CMDORCTRL" => (0xA2, 0xA3),
            "ALT" | "OPTION" => (0xA4, 0xA5),
            "SHIFT" => (0xA0, 0xA1),
            "SUPER" | "META" | "WIN" | "CMD" | "COMMAND" => (0x5B, 0x5C),
            _ => {
                let k = match up.as_str() {
                    "SPACE" => 0x20, "ENTER" | "RETURN" => 0x0D, "TAB" => 0x09, "BACKSPACE" => 0x08,
                    "DELETE" | "DEL" => 0x2E, "INSERT" => 0x2D, "HOME" => 0x24, "END" => 0x23,
                    "PAGEUP" => 0x21, "PAGEDOWN" => 0x22, "LEFT" | "ARROWLEFT" => 0x25,
                    "UP" | "ARROWUP" => 0x26, "RIGHT" | "ARROWRIGHT" => 0x27, "DOWN" | "ARROWDOWN" => 0x28,
                    _ if up.len() == 1 && up.as_bytes()[0].is_ascii_alphanumeric() => u16::from(up.as_bytes()[0]),
                    _ if up.len() >= 2 && up.starts_with('F') => match up[1..].parse::<u16>() {
                        Ok(n @ 1..=24) => 0x6F + n,
                        _ => return None,
                    },
                    _ => return None,
                };
                (k, k)
            }
        };
        out.push(vk);
    }
    (out.len() >= 2).then_some(out)
}

/// The keys the user bound to a shortcut of this feature, or its default; None if not watchable.
fn bound_keys(id: &str) -> Option<Vec<(u16, u16)>> {
    let stored: Value = std::fs::read_to_string(notch_core::win::config_dir().join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let bound = &stored["shortcuts"][format!("{ID}:{id}")];
    if bound["enabled"].as_bool() == Some(false) {
        return None;
    }
    let default = if id == "brighter" { "Ctrl+Alt+PageUp" } else { "Ctrl+Alt+PageDown" };
    vks_of(bound["keys"].as_str().unwrap_or(default))
}

fn name_of(p: &std::path::Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn radio_word(kind: Kind) -> &'static str {
    match radio::states(kind) {
        Ok(on) if on.is_empty() => "none",
        Ok(on) if on.contains(&true) => "on",
        Ok(_) => "off",
        Err(_) => "denied",
    }
}

impl Feature for DisplaysKeepawake {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        if self.running.swap(true, Relaxed) {
            return Ok(());
        }
        let inner = self.inner.clone();
        inner.queue.quit.store(false, Relaxed);
        inner.sync_subs(cx);
        let cx = cx.clone();
        // A radio switched off before a shutdown while asleep comes back first of all.
        std::thread::spawn(move || {
            DisplaysKeepawake::radios_debt(&cx);
            inner.automate(&cx);
        });
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        if !self.running.swap(false, Relaxed) {
            return;
        }
        let inner = &self.inner;
        inner.end(cx, "off");
        inner.close_overlays(cx);
        *inner.subs.lock().unwrap() = Subs::default();
        inner.queue.quit.store(true, Relaxed);
        inner.queue.jobs.lock().unwrap().clear();
        inner.queue.wake.notify_all();
        *inner.worker.lock().unwrap() = false;
        inner.queued.lock().unwrap().clear();
        let cx2 = cx.clone();
        let _ = within(Duration::from_secs(3), move || Inner::radios_back(&cx2));
    }

    fn settings_changed(&self, cx: &Ctx, settings: &Value) {
        let inner = &self.inner;
        inner.sync_subs(cx);
        if let Some(s) = inner.session.lock().unwrap().as_mut() {
            // The display request follows "allow display sleep" at once.
            if let Ok(r) = power::Request::new(flag(settings, "allowDisplaySleep")) {
                s._request = r;
            }
        }
        if !flag(settings, "extraDimming") {
            inner.close_overlays(cx);
            inner.queued.lock().unwrap().clear();
        }
        let inner = inner.clone();
        let cx = cx.clone();
        std::thread::spawn(move || inner.automate(&cx));
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let inner = &self.inner;
        match method {
            // ── keep awake
            "state" => Ok(inner.session_json()),
            "start" => {
                let minutes = args["minutes"].as_u64().unwrap_or(0).min(24 * 60);
                inner.suppressed.store(false, Relaxed);
                inner.begin(cx, minutes, false)?;
                cx.patch_settings(ID, json!({ "minutes": minutes }));
                Ok(inner.session_json())
            }
            "extend" => {
                let add = args["minutes"].as_u64().unwrap_or(15).min(24 * 60) * 60_000;
                if let Some(s) = inner.session.lock().unwrap().as_mut() {
                    // Open-ended sessions stay open-ended; the others end later.
                    if let Some(e) = s.ends {
                        s.ends = Some(e.max(now_ms()) + add);
                    }
                }
                cx.emit(ID, "session", inner.session_json());
                Ok(inner.session_json())
            }
            "stop" => {
                // Stopped by hand while the conditions still hold: no new automatic session until they clear.
                inner.suppressed.store(inner.conditions(cx) == Some(true), Relaxed);
                inner.end(cx, "manual");
                Ok(inner.session_json())
            }
            "toggle" => {
                if inner.session.lock().unwrap().is_some() {
                    self.call(cx, "stop", Value::Null)
                } else {
                    let minutes = cx.settings(ID)["minutes"].as_u64().unwrap_or(60);
                    self.call(cx, "start", json!({ "minutes": minutes }))
                }
            }
            // The page tells whether an agent works; the quiet minutes after the last one count from here.
            "agents" => {
                let busy = args["working"].as_bool().unwrap_or(false);
                if busy {
                    inner.agents_last.store(now_ms(), Relaxed);
                } else if inner.agents_busy.load(Relaxed) {
                    inner.agents_last.store(now_ms(), Relaxed);
                    // The condition lapses once the quiet time has passed.
                    let quiet = cx.settings(ID)["agentsMinutes"].as_u64().unwrap_or(5).min(120) * 60_000;
                    let (me, cx2) = (inner.clone(), cx.clone());
                    later(&inner.agents_turn, quiet + 500, move || me.automate(&cx2));
                }
                inner.agents_busy.store(busy, Relaxed);
                if flag(&cx.settings(ID), "autoAgents") {
                    let (me, cx2) = (inner.clone(), cx.clone());
                    std::thread::spawn(move || me.automate(&cx2));
                }
                Ok(Value::Null)
            }
            "shortcut" => {
                match args["id"].as_str() {
                    Some("toggle") => return self.call(cx, "toggle", Value::Null),
                    Some(id @ ("brighter" | "dimmer")) => inner.hold_step(cx, id == "brighter"),
                    _ => {}
                }
                Ok(Value::Null)
            }
            // ── displays
            "displays" => {
                let extra = Inner::extra(cx);
                let out: Vec<Value> = display::list()
                    .iter()
                    .map(|d| {
                        let (m, hw) = display::probe(d);
                        // Fresh reads: a display that was marked unsupported gets another chance on reopening.
                        inner.methods.lock().unwrap().insert(d.id.clone(), m);
                        inner.fails.lock().unwrap().remove(&d.id);
                        let remembered = inner.levels.lock().unwrap().get(&d.id).copied();
                        let level = match (remembered, hw) {
                            // What the user left, if the hardware still agrees.
                            (Some(r), Some(hw)) => match plan(m, d.internal, extra, r).0 {
                                Some(p) if p.abs_diff(hw) <= 3 => r,
                                _ => level_of(extra, d.internal, hw),
                            },
                            (Some(r), None) => r,
                            (None, Some(hw)) => level_of(extra, d.internal, hw),
                            (None, None) => 1.0,
                        };
                        inner.levels.lock().unwrap().insert(d.id.clone(), level);
                        json!({ "id": d.id, "name": d.name, "internal": d.internal, "primary": d.primary, "method": m.name(), "level": level })
                    })
                    .collect();
                Ok(json!(out))
            }
            "set" => {
                let id = args["id"].as_str().ok_or("no display")?;
                let level = args["level"].as_f64().ok_or("no level")? as f32;
                let d = Inner::display_of(id).ok_or("That display is not connected any more.")?;
                let before = inner.levels.lock().unwrap().get(id).copied().unwrap_or(1.0);
                inner.apply(cx, &d, level);
                inner.levels.lock().unwrap().entry(id.to_string()).or_insert(level);
                let mut moved = vec![json!({ "id": id, "level": level })];
                if flag(&cx.settings(ID), "linkDisplays") {
                    // The others move by the same amount.
                    let delta = level - before;
                    for o in display::list().iter().filter(|o| o.id != id) {
                        let was = inner.levels.lock().unwrap().get(&o.id).copied().unwrap_or(1.0);
                        let to = (was + delta).clamp(0.0, 1.0);
                        inner.apply(cx, o, to);
                        moved.push(json!({ "id": o.id, "level": to }));
                    }
                }
                Ok(json!(moved))
            }
            "dimAlpha" => {
                let label = args["label"].as_str().unwrap_or_default();
                let id = inner.opened.lock().unwrap().iter().find(|(_, l)| l.as_str() == label).map(|(k, _)| k.clone());
                Ok(json!(id.and_then(|id| inner.alphas.lock().unwrap().get(&id).copied()).unwrap_or(0.0)))
            }
            // ── wallpaper
            "wallpapers" => {
                let own = wall::own_dir(cx.settings(ID)["wallpaperFolder"].as_str().unwrap_or(""));
                let (sys, mine) = wall::list(&own);
                let row = |p: &PathBuf| json!({ "path": p.to_string_lossy(), "name": name_of(p) });
                Ok(json!({ "system": sys.iter().map(row).collect::<Vec<_>>(), "own": mine.iter().map(row).collect::<Vec<_>>(), "folder": own.to_string_lossy() }))
            }
            "wallThumb" | "wallSet" => {
                let own = wall::own_dir(cx.settings(ID)["wallpaperFolder"].as_str().unwrap_or(""));
                let path = PathBuf::from(args["path"].as_str().ok_or("no picture")?);
                if !wall::allowed(&path, &own) {
                    return Err("That is not one of the wallpaper pictures.".into());
                }
                if method == "wallSet" {
                    wall::set(&path)?;
                    return Ok(Value::Null);
                }
                let png = svc_shell::thumbnail(&path, 160)?;
                Ok(json!(format!("data:image/png;base64,{}", base64(&png))))
            }
            // ── radios
            "radios" => Ok(json!({ "bluetooth": radio_word(Kind::Bluetooth), "wifi": radio_word(Kind::Wifi) })),
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/displays-keepawake/strings.json")
    }
}

impl DisplaysKeepawake {
    fn radios_debt(cx: &Ctx) {
        Inner::radios_back(cx);
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(DisplaysKeepawake::default())
}
