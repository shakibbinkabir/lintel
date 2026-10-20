// Window layout — placement shortcuts with gaps and a way back, the pointer to
// the next display, focus that follows the pointer and close protection.
//
// The contract is docs/ARCHITECTURE.md §2: nothing runs before `start`,
// everything is gone after `stop`, every command arrives through `call`.
//
// What runs: nothing for the placements (their shortcuts are the host's hot
// keys, and a press arrives here as `call("shortcut")`). Focus follows the
// pointer is one mouse subscription and a thread asleep until the pointer
// moves (follow.rs); close protection is one keyboard subscription (guard.rs).
// Both exist only while their switch is on, so with every tool off there is no
// hook, no timer and no thread.

pub mod drag;
pub mod follow;
pub mod guard;
pub mod layout;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use guard::Which;
use notch_core::{Ctx, Feature, Group, Manifest, ShortcutDef, Use};
use serde_json::{json, Value};

const ID: &str = "window-layout";

const fn key(id: &'static str, label: &'static str, default: Option<&'static str>) -> ShortcutDef {
    ShortcutDef { id, label, default }
}

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Window layout",
    group: Group::Windows,
    // Opt-in: it takes shortcuts with the Windows key, and two of its tools read the keyboard and the mouse.
    default_on: false,
    uses: &[Use::InputHook],
    // The ids are the actions of layout.rs. Ctrl+Win+Left and Right belong to
    // Windows (the virtual desktops), so the arrows sit one modifier further.
    shortcuts: &[
        key("left", "Snap left", Some("Ctrl+Alt+Super+Left")),
        key("right", "Snap right", Some("Ctrl+Alt+Super+Right")),
        key("up", "Snap up", Some("Ctrl+Alt+Super+Up")),
        key("down", "Snap down", Some("Ctrl+Alt+Super+Down")),
        key("center", "Center", Some("Ctrl+Alt+Super+Home")),
        key("restore", "Put back where it was", Some("Ctrl+Alt+Super+Backspace")),
        key("next-display", "Move to the next display", Some("Ctrl+Alt+Super+PageDown")),
        key("previous-display", "Move to the previous display", Some("Ctrl+Alt+Super+PageUp")),
        key("maximize", "Maximize", None),
        key("almost-maximize", "Almost maximize", None),
        key("top-left", "Top left", None),
        key("top-right", "Top right", None),
        key("bottom-left", "Bottom left", None),
        key("bottom-right", "Bottom right", None),
        key("left-third", "Left third", None),
        key("center-third", "Center third", None),
        key("right-third", "Right third", None),
        key("left-two-thirds", "Left two thirds", None),
        key("right-two-thirds", "Right two thirds", None),
        key("pin", "Keep the window above the others (again: let it go)", Some("Ctrl+Alt+Super+P")),
        key("unpin-all", "Let every pinned window go", None),
        key("pointer-next-display", "Send the pointer to the next display", None),
    ],
    tray: &[],
    // Gaps in pixels at 100 %, margin in percent, delays in milliseconds.
    // ignoredApps: program names (game.exe). closeMode: "hold" | "double".
    defaults: r#"{ "cycleThirds": true, "windowGap": 0, "screenGap": 0, "margin": 5, "ignoredApps": [], "follow": false, "followDelay": 250, "closeAltF4": false, "closeCtrlW": false, "closeMode": "hold", "closeHoldMs": 800, "closeIntervalMs": 600, "screenGapLeft": -1, "screenGapTop": -1, "screenGapRight": -1, "screenGapBottom": -1, "edgeSnap": false, "dragModifier": "off", "zoneKey": "shift", "desktopStrip": false }"#,
};

// ── Settings into configuration ───────────────────────────────────────────────

/// A number of the settings, the default when it is out of range (as the upstream clamps).
fn number(s: &Value, key: &str, range: std::ops::RangeInclusive<u64>, fallback: u64) -> u64 {
    s[key].as_u64().filter(|v| range.contains(v)).unwrap_or(fallback)
}

fn options(s: &Value) -> layout::Options {
    layout::Options {
        window_gap: number(s, "windowGap", 0..=128, 0) as i32,
        screen_gap: number(s, "screenGap", 0..=128, 0) as i32,
        // -1: the same as the screen gap.
        edge_gaps: ["screenGapLeft", "screenGapTop", "screenGapRight", "screenGapBottom"].map(|k| s[k].as_i64().filter(|v| (0..=128).contains(v)).map(|v| v as i32)),
        margin: number(s, "margin", 0..=25, 5) as i32,
        cycle: s["cycleThirds"] != false,
    }
}

fn exe_name(path: &str) -> String {
    Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase()
}

/// An app of the list is in front: the placements leave it alone.
fn ignored(s: &Value) -> bool {
    svc_windows::front().is_some_and(|w| ignores(s, &w.exe))
}

fn ignores(s: &Value, exe: &str) -> bool {
    let front = exe_name(exe);
    !front.is_empty() && s["ignoredApps"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|app| app.trim().eq_ignore_ascii_case(&front))
}

// ── The feature ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct Running {
    layout: layout::Layout,
    /// Focus follows the pointer, with the delay it runs at.
    follow: Mutex<Option<(u64, follow::Follow)>>,
    /// Close protection, with what it guards.
    guard: Mutex<Option<(guard::Config, guard::Guard)>>,
    /// Snapping by dragging, with what it runs on.
    drag: Mutex<Option<(drag::Config, drag::Drag)>>,
    /// The windows kept above the others from here, let go when the feature stops.
    pinned: Mutex<Vec<isize>>,
    /// A switch of virtual desktop is under way.
    switching: Arc<AtomicBool>,
}

impl Running {
    fn unpin_all(&self) {
        for hwnd in lock(&self.pinned).drain(..) {
            if layout::is_topmost(hwnd) {
                layout::set_topmost(hwnd, false);
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.unpin_all();
    }
}

#[derive(Default)]
pub struct WindowLayout {
    run: Mutex<Option<Arc<Running>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A guarded shortcut was held back, let go or confirmed: the page says so, and a confirmed one is carried out.
fn closing(cx: &Ctx, event: guard::Event, hold: bool) {
    let (state, which) = match event {
        guard::Event::Asked(which) => ("asked", which),
        guard::Event::Cancelled(which) => ("cancelled", which),
        guard::Event::Confirmed(which) => ("confirmed", which),
    };
    if state == "confirmed" {
        match which {
            // As the window's own × does: it may still ask to save.
            Which::AltF4 => {
                if let Some(front) = svc_windows::front() {
                    svc_windows::close(front.hwnd);
                }
            }
            // Ctrl is still held: the W that was held back, sent as the app's own.
            Which::CtrlW => {
                svc_input::inject_keys(&[(0x57, true), (0x57, false)]);
            }
        }
    }
    let keys = if which == Which::AltF4 { "Alt+F4" } else { "Ctrl+W" };
    cx.emit(ID, "guard", json!({ "state": state, "keys": keys, "hold": hold }));
}

/// Starts, stops or restarts the two tools that run, to match the settings.
fn tools(cx: &Ctx, run: &Arc<Running>, s: &Value) {
    let delay = (s["follow"] == true).then(|| number(s, "followDelay", 100..=3000, 250));
    let mut follow = lock(&run.follow);
    if follow.as_ref().map(|f| f.0) != delay {
        // The old one first: its hook goes before the new one comes.
        *follow = None;
        *follow = delay.map(|ms| (ms, follow::start(ms, |_| true)));
    }

    let modifier: Vec<i32> = match s["dragModifier"].as_str() {
        Some("alt") => vec![0x12],
        Some("ctrl-alt") => vec![0x11, 0x12],
        Some("shift-alt") => vec![0x10, 0x12],
        _ => vec![],
    };
    let zones = match s["zoneKey"].as_str() {
        Some("shift") => 0x10,
        Some("ctrl") => 0x11,
        Some("alt") => 0x12,
        _ => 0,
    };
    let config = (s["edgeSnap"] == true || !modifier.is_empty()).then(|| drag::Config { edge: s["edgeSnap"] == true, modifier, zones });
    let mut dragging = lock(&run.drag);
    if dragging.as_ref().map(|d| &d.0) != config.as_ref() {
        *dragging = None;
        *dragging = config.map(|config| {
            let (a, b) = (Arc::downgrade(run), Arc::downgrade(run));
            let (cx_snap, cx_skip) = (cx.clone(), cx.clone());
            let hooks = drag::Hooks {
                unsnap: Box::new(move |hwnd| Weak::upgrade(&a).and_then(|r| r.layout.unsnap(hwnd))),
                snap: Box::new(move |hwnd, id| {
                    if let Some(r) = Weak::upgrade(&b) {
                        r.layout.apply(hwnd, id, &options(&cx_snap.settings(ID)));
                    }
                }),
                skip: Box::new(move |hwnd| {
                    let s = cx_skip.settings(ID);
                    svc_windows::windows().into_iter().find(|w| w.hwnd == hwnd).is_some_and(|w| ignores(&s, &w.exe))
                }),
            };
            (config.clone(), drag::start(config, hooks))
        });
    }
    drop(dragging);

    let keys: Vec<(u16, Which)> = [("closeAltF4", 0x73, Which::AltF4), ("closeCtrlW", 0x57, Which::CtrlW)]
        .into_iter()
        .filter(|k| s[k.0] == true)
        .map(|k| (k.1, k.2))
        .collect();
    let hold = s["closeMode"] != "double";
    let ms = if hold { number(s, "closeHoldMs", 250..=2000, 800) } else { number(s, "closeIntervalMs", 200..=1500, 600) };
    let config = (!keys.is_empty()).then(|| guard::Config { keys, hold, ms, injected: false });
    let mut guard = lock(&run.guard);
    if guard.as_ref().map(|g| &g.0) != config.as_ref() {
        *guard = None;
        *guard = config.map(|config| {
            let cx = cx.clone();
            (config.clone(), guard::start(config, guard::held, move |event| closing(&cx, event, hold)))
        });
    }
}

impl WindowLayout {
    fn running(&self) -> Result<Arc<Running>, String> {
        lock(&self.run).clone().ok_or_else(|| "Window layout is turned off.".into())
    }
}

impl Feature for WindowLayout {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let mut run = lock(&self.run);
        if run.is_none() {
            let running = Arc::new(Running::default());
            tools(cx, &running, &cx.settings(ID));
            *run = Some(running);
        }
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        // Dropping the tools drops their subscriptions: the hooks go with the last one.
        lock(&self.run).take();
    }

    fn settings_changed(&self, cx: &Ctx, settings: &Value) {
        if let Ok(run) = self.running() {
            tools(cx, &run, settings);
        }
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let run = self.running()?;
        match method {
            // A shortcut (`id`) or a tile of the island's section (`action`).
            "shortcut" | "apply" => {
                let pressed = method == "shortcut";
                let action = args[if pressed { "id" } else { "action" }].as_str().unwrap_or_default();
                let s = cx.settings(ID);
                let result = if pressed && ignored(&s) {
                    "ignored"
                } else if action == "pin" {
                    match layout::target(!pressed) {
                        Some(hwnd) => {
                            let on = !layout::is_topmost(hwnd);
                            if !layout::set_topmost(hwnd, on) {
                                "failed"
                            } else {
                                let mut pinned = lock(&run.pinned);
                                pinned.retain(|h| *h != hwnd);
                                if on {
                                    pinned.push(hwnd);
                                }
                                if on { "pinned" } else { "unpinned" }
                            }
                        }
                        None => "no-window",
                    }
                } else if action == "unpin-all" {
                    run.unpin_all();
                    "unpinned"
                } else if action == "pointer-next-display" {
                    if layout::pointer_to_next_display() { "done" } else { "one-display" }
                } else {
                    // A tile was clicked on the island: never the app's own window.
                    match layout::target(!pressed) {
                        Some(hwnd) => run.layout.apply(hwnd, action, &options(&s)),
                        None => "no-window",
                    }
                };
                let out = json!({ "action": action, "result": result });
                if pressed {
                    // The section shows what its own click came to; a shortcut has nowhere else to say it.
                    cx.emit(ID, "result", out.clone());
                }
                Ok(out)
            }
            // The virtual desktops: names (null: not named), the one in front.
            "desktops" => {
                let (names, current) = svc_windows::desktops();
                Ok(json!({ "names": names, "current": current, "on": cx.settings(ID)["desktopStrip"] == true }))
            }
            // Switches to desktop `to` with the system's own keys (Win+Ctrl+Left/Right), one step at a time.
            "desktop" => {
                let (names, current) = svc_windows::desktops();
                let to = args["to"].as_u64().unwrap_or(0) as usize;
                if to >= names.len() || to == current || run.switching.swap(true, Relaxed) {
                    return Ok(json!(false));
                }
                let arrow = if to > current { 0x27 } else { 0x25 };
                let (cx, switching) = (cx.clone(), run.switching.clone());
                std::thread::spawn(move || {
                    for _ in 0..to.abs_diff(current) {
                        svc_input::inject_keys(&[(0x5B, true), (0xA2, true), (arrow, true), (arrow, false), (0xA2, false), (0x5B, false)]);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let (_, now) = svc_windows::desktops();
                    switching.store(false, Relaxed);
                    cx.emit(ID, "desktop", json!({ "current": now }));
                });
                Ok(json!(true))
            }
            // The tiles, for the section's grid: one table for what is drawn and what is done.
            "tiles" => Ok(json!(layout::TILES.iter().map(|t| json!({ "id": t.0, "name": t.1, "share": t.2 })).collect::<Vec<_>>())),
            // The programs with a window, for the list of apps to leave alone.
            "apps" => {
                let me = std::process::id();
                let mut apps: Vec<String> =
                    svc_windows::windows().into_iter().filter(|w| w.pid != me).map(|w| exe_name(&w.exe)).filter(|exe| !exe.is_empty()).collect();
                apps.sort();
                apps.dedup();
                Ok(json!(apps))
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/window-layout/strings.json")
    }
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(WindowLayout::default())
}
