// Island window: placement on the chosen display, the two window sizes
// (full panel / invisible wake strip), click-through and the cursor poll.
//
// There is no notch on a PC, so the island is a black shape drawn at the top
// centre of the main display inside a borderless, transparent, always-on-top
// window that never takes focus.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::platform::{self, cursor_physical, left_button_down};

/// Logical size of the full window: the largest island (640×320) and room around it.
/// Same constants in src/core/layout.ts.
pub const PANEL_W: f64 = 720.0;
pub const PANEL_H: f64 = 360.0;
/// Logical size of the invisible strip that wakes the island when it is hidden.
pub const STRIP_W: f64 = 240.0;
pub const STRIP_H: f64 = 6.0;
/// The window of a floating island that is hidden: its 72×14 pill, 12 px above
/// it and 4 px beside and below (FLOAT_TOP, PILL_W and PILL_H in src/island/place.ts).
pub const PILL_WIN_W: f64 = 80.0;
pub const PILL_WIN_H: f64 = 30.0;

pub const WINDOW_LABEL: &str = "island";

/// Margin around the island that still counts as "on the island", in logical px.
/// Wider than the macOS 6 pt because a click must never be swallowed.
const HIT_MARGIN: f64 = 14.0;

#[derive(Serialize, Clone)]
pub struct CursorPayload {
    pub x: f64,
    pub y: f64,
    /// The left button is held: a drag may be going on.
    pub down: bool,
}

#[derive(Serialize, Clone)]
pub struct ScreenInfo {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

/// The island shape in window-logical coordinates, pushed by the front end.
/// The poll thread owns the click-through decision so it lands in the same 16 ms
/// tick as the cursor read — an IPC round trip here loses clicks.
#[derive(Clone, Copy, Default)]
pub struct IslandRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where the page put the island after a drag (src/island/place.ts decides):
/// the top-centre point of its window in screen pixels. None: the top centre
/// of the chosen display, as before there was any dragging.
#[derive(Deserialize, Clone, Copy)]
pub struct Place {
    pub x: i32,
    pub y: i32,
    pub floating: bool,
}

static PLACE: Mutex<Option<Place>> = Mutex::new(None);

pub fn set_place(place: Option<Place>) {
    *PLACE.lock().unwrap() = place;
}

/// A display as the page's placement rules want it: screen pixels, the work
/// area, and the key it is remembered by (`DisplayId::key`).
#[derive(Serialize)]
pub struct DisplayInfo {
    key: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    wx: i32,
    wy: i32,
    ww: u32,
    wh: u32,
    scale: f64,
}

pub fn displays(app: &AppHandle) -> Vec<DisplayInfo> {
    let Ok(monitors) = app.available_monitors() else { return Vec::new() };
    monitors
        .iter()
        .map(|m| {
            let (p, s, work) = (m.position(), m.size(), m.work_area());
            DisplayInfo {
                key: describe(m).key(),
                x: p.x,
                y: p.y,
                w: s.width,
                h: s.height,
                wx: work.position.x,
                wy: work.position.y,
                ww: work.size.width,
                wh: work.size.height,
                scale: m.scale_factor(),
            }
        })
        .collect()
}

/// Wakes / parks the cursor poll thread so a hidden island costs literally nothing.
pub struct PollGate {
    active: Mutex<bool>,
    cv: Condvar,
    pub collapsed: AtomicBool,
    pub rect: Mutex<IslandRect>,
    /// Mirrors the window flag so we only call into the OS when it changes.
    ignoring: AtomicBool,
    /// The strip rests: the setting keeps it at peek, the pointer is away and
    /// nothing moves (the page says so). The poll then only looks, ten times a
    /// second, for the pointer coming back, and ends the rest itself when it does.
    pub resting: AtomicBool,
    /// The island is being dragged: where the pointer holds its window (pointer
    /// minus window origin, screen pixels). The poll carries the window until
    /// the button is let go.
    pub drag: Mutex<Option<(i32, i32)>>,
}

impl PollGate {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(false),
            cv: Condvar::new(),
            collapsed: AtomicBool::new(true),
            rect: Mutex::new(IslandRect::default()),
            ignoring: AtomicBool::new(false),
            resting: AtomicBool::new(false),
            drag: Mutex::new(None),
        }
    }

    pub fn set_rect(&self, rect: IslandRect) {
        *self.rect.lock().unwrap() = rect;
    }

    /// Forces the next poll tick to re-apply the flag (after a window resize).
    pub fn forget_ignore_state(&self) {
        self.ignoring.store(false, Ordering::Relaxed);
    }

    pub fn set_active(&self, on: bool) {
        let mut guard = self.active.lock().unwrap();
        *guard = on;
        self.cv.notify_all();
    }

    pub(crate) fn wait_until_active(&self) {
        let mut guard = self.active.lock().unwrap();
        while !*guard {
            guard = self.cv.wait(guard).unwrap();
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        *self.active.lock().unwrap()
    }
}

/// A press outside the open island folds it (§4). The island never has the
/// focus to lose, so the press is heard through the shared mouse hook, which
/// exists only while this subscription does: while the page says the island is
/// open and may fold. The press is never swallowed, only noted: the cursor
/// poll, which knows where the island is, reads it on its next tick.
static OUTSIDE: Mutex<Option<svc_input::Sub>> = Mutex::new(None);
/// The last press not yet looked at: bit 63 set, then x and y (screen pixels) as two u31.
static PRESS: AtomicU64 = AtomicU64::new(0);

pub fn watch_outside(on: bool) {
    let mut slot = OUTSIDE.lock().unwrap();
    if on == slot.is_some() {
        return;
    }
    PRESS.store(0, Ordering::Relaxed);
    // Before every other stage (crates/svc-input): a debounced or remapped click is still a click outside.
    *slot = on.then(|| {
        svc_input::mouse(0, false, |m| {
            if matches!(m.kind, svc_input::MouseKind::Down(_)) {
                let pack = |v: i32| (v as i64 + (1 << 30)) as u64 & 0x7FFF_FFFF;
                PRESS.store(1 << 63 | pack(m.x) << 31 | pack(m.y), Ordering::Relaxed);
            }
            svc_input::Verdict::Pass
        })
    });
}

/// The press the mouse hook noted since the last tick, in screen pixels.
fn take_press() -> Option<(i32, i32)> {
    let p = PRESS.swap(0, Ordering::Relaxed);
    let unpack = |v: u64| ((v & 0x7FFF_FFFF) as i64 - (1 << 30)) as i32;
    (p != 0).then(|| (unpack(p >> 31), unpack(p)))
}

pub fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(WINDOW_LABEL)
}

fn monitor_contains(m: &Monitor, x: f64, y: f64) -> bool {
    let p = m.position();
    let s = m.size();
    x >= p.x as f64
        && x < (p.x + s.width as i32) as f64
        && y >= p.y as f64
        && y < (p.y + s.height as i32) as f64
}

/// A display's logical origin, the key `at:<x>,<y>` preferences are matched on.
/// Names are no good for that: two monitors of the same model share one.
fn logical_origin(m: &Monitor) -> (i32, i32) {
    let scale = m.scale_factor();
    let p = m.position();
    ((p.x as f64 / scale).round() as i32, (p.y as f64 / scale).round() as i32)
}

/// One entry of the "Island lives on" list in Settings.
#[derive(Serialize, Clone)]
pub struct MonitorChoice {
    pub key: String,
    pub label: String,
}

pub fn monitor_choices(app: &AppHandle) -> Vec<MonitorChoice> {
    let Ok(monitors) = app.available_monitors() else { return Vec::new() };
    monitors
        .iter()
        .map(|m| {
            let d = describe(m);
            MonitorChoice {
                key: d.key(),
                label: crate::i18n::tf(
                    "{name} — {width}×{height} at {x},{y}",
                    &[
                        ("name", &d.name),
                        ("width", &d.w.to_string()),
                        ("height", &d.h.to_string()),
                        ("x", &d.x.to_string()),
                        ("y", &d.y.to_string()),
                    ],
                ),
            }
        })
        .collect()
}

/// What a display is remembered by: its logical origin, plus its name and
/// logical size, so it is still found after the layout is rearranged or the
/// resolution changes (the Mac keeps the display's UUID for the same reason;
/// Tauri has no stable ID).
#[derive(Debug, Clone, PartialEq)]
struct DisplayId {
    name: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl DisplayId {
    /// `at:<x>,<y>` stays first, so a preference saved before still matches.
    fn key(&self) -> String {
        format!("at:{},{}|{}|{}x{}", self.x, self.y, self.name.replace('|', " "), self.w, self.h)
    }
}

fn describe(m: &Monitor) -> DisplayId {
    let (x, y) = logical_origin(m);
    let scale = m.scale_factor();
    let s = m.size();
    DisplayId {
        name: m.name().cloned().unwrap_or_else(|| "Display".into()),
        x,
        y,
        w: (s.width as f64 / scale).round() as i32,
        h: (s.height as f64 / scale).round() as i32,
    }
}

/// Which display a saved `at:` preference points at, best match first: same
/// place and name; the same name and size elsewhere (layout rearranged); the
/// same name alone when unique (resolution changed); the same place. None
/// means unplugged, and the caller falls back to the primary display.
fn pick_display(pref: &str, displays: &[DisplayId]) -> Option<usize> {
    let rest = pref.strip_prefix("at:")?;
    let mut parts = rest.split('|');
    let (x, y) = parts.next()?.split_once(',')?;
    let (x, y) = (x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?);
    let name = parts.next();
    let size = parts.next().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?))
    });
    let at = |d: &DisplayId| d.x == x && d.y == y;
    if let Some(name) = name {
        if let Some(i) = displays.iter().position(|d| at(d) && d.name == name) {
            return Some(i);
        }
        if let Some((w, h)) = size {
            if let Some(i) = displays.iter().position(|d| d.name == name && d.w == w && d.h == h) {
                return Some(i);
            }
        }
        let mut same_name = displays.iter().enumerate().filter(|(_, d)| d.name == name);
        if let (Some((i, _)), None) = (same_name.next(), same_name.next()) {
            return Some(i);
        }
    }
    displays.iter().position(at)
}

/// The display the island lives on: a chosen one, the primary one, or the one
/// under the cursor.
pub(crate) fn target_monitor(app: &AppHandle, pref: &str) -> Option<Monitor> {
    let monitors = app.available_monitors().ok()?;
    let ids: Vec<DisplayId> = monitors.iter().map(describe).collect();
    if let Some(i) = pick_display(pref, &ids) {
        return Some(monitors[i].clone());
    }
    if pref == "cursor" {
        if let Some((cx, cy)) = cursor_physical() {
            if let Some(m) = monitors.iter().find(|m| monitor_contains(m, cx, cy)) {
                return Some(m.clone());
            }
        }
    }
    app.primary_monitor()
        .ok()
        .flatten()
        .or_else(|| monitors.into_iter().next())
}

#[cfg(test)]
mod display_tests {
    use super::*;

    fn d(name: &str, x: i32, y: i32, w: i32, h: i32) -> DisplayId {
        DisplayId { name: name.into(), x, y, w, h }
    }

    #[test]
    fn a_display_is_found_again_after_changes() {
        let dell = d("DELL U2720Q", 1920, 0, 2560, 1440);
        let lap = d("eDP-1", 0, 0, 1920, 1200);
        let key = dell.key();
        assert_eq!(pick_display(&key, &[lap.clone(), dell.clone()]), Some(1));
        // Rearranged: the Dell moved to the left of the laptop.
        let moved = [d("eDP-1", 2560, 0, 1920, 1200), d("DELL U2720Q", 0, 0, 2560, 1440)];
        assert_eq!(pick_display(&key, &moved), Some(1));
        // Resolution changed, still the only Dell.
        let rescaled = [lap.clone(), d("DELL U2720Q", 1920, 0, 1920, 1080)];
        assert_eq!(pick_display(&key, &rescaled), Some(1));
        // Unplugged: nothing, so the caller falls back to the primary display.
        assert_eq!(pick_display(&key, &[lap.clone()]), None);
    }

    #[test]
    fn two_identical_monitors_are_told_apart_by_place() {
        let a = d("LG 27UL500", 0, 0, 1920, 1080);
        let b = d("LG 27UL500", 1920, 0, 1920, 1080);
        assert_eq!(pick_display(&b.key(), &[a.clone(), b.clone()]), Some(1));
        assert_eq!(pick_display(&a.key(), &[a, b]), Some(0));
    }

    #[test]
    fn preferences_saved_before_still_match() {
        let lap = d("eDP-1", 0, 0, 1920, 1200);
        let ext = d("HDMI-1", 1920, 0, 1920, 1080);
        assert_eq!(pick_display("at:1920,0", &[lap.clone(), ext.clone()]), Some(1));
        assert_eq!(pick_display("primary", &[lap, ext]), None);
        assert_eq!(pick_display("at:nonsense", &[]), None);
    }
}

pub fn screen_info(app: &AppHandle, pref: &str) -> ScreenInfo {
    match target_monitor(app, pref) {
        Some(m) => {
            let scale = m.scale_factor();
            let p = m.position();
            let s = m.size();
            ScreenInfo {
                x: p.x as f64 / scale,
                y: p.y as f64 / scale,
                width: s.width as f64 / scale,
                height: s.height as f64 / scale,
                scale,
            }
        }
        None => ScreenInfo { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0, scale: 1.0 },
    }
}

/// Places and sizes the window. `collapsed` picks the wake strip instead of the panel.
pub fn apply_geometry(app: &AppHandle, pref: &str, collapsed: bool) {
    let Some(win) = window(app) else { return };
    // Dragged somewhere: the display that point is on gives the scale.
    let place = *PLACE.lock().unwrap();
    let there = place.and_then(|p| {
        let monitors = app.available_monitors().ok()?;
        monitors.into_iter().find(|m| monitor_contains(m, p.x as f64, p.y as f64 + 1.0))
    });
    let Some(m) = there.or_else(|| target_monitor(app, pref)) else { return };

    let scale = m.scale_factor();
    let mp = *m.position();
    let ms = *m.size();

    let (lw, lh) = match (collapsed, place) {
        (false, _) => (PANEL_W, PANEL_H),
        (true, Some(Place { floating: true, .. })) => (PILL_WIN_W, PILL_WIN_H),
        (true, _) => (STRIP_W, STRIP_H),
    };
    let pw = (lw * scale).round().max(1.0) as u32;
    let ph = (lh * scale).round().max(1.0) as u32;
    let (x, y) = match place {
        Some(p) => (p.x - pw as i32 / 2, p.y),
        None => (mp.x + (ms.width as i32 - pw as i32) / 2, mp.y),
    };

    let _ = win.set_size(PhysicalSize::new(pw, ph));
    let _ = win.set_position(PhysicalPosition::new(x, y));
    // Moving across displays can rescale the window: re-assert the physical size.
    let _ = win.set_size(PhysicalSize::new(pw, ph));
    let _ = win.set_always_on_top(true);
}

/// The zoom that makes one CSS pixel one window pixel again, when the web view's
/// pixel ratio differs from the window's scale (mixed-scale displays, text
/// scaling): the open island would otherwise be cut off. None = nothing to do.
pub fn zoom_correction(current: f64, scale: f64, dpr: f64) -> Option<f64> {
    let ok = |v: f64| v.is_finite() && v > 0.0;
    if !ok(dpr) || !ok(scale) || (dpr - scale).abs() / scale < 0.02 {
        return None;
    }
    Some((current * scale / dpr).clamp(0.25, 4.0))
}

/// Position, size and scale of the monitor the island lives on. Any change here
/// means the island has to be placed again.
/// With every other display after it: a dragged island may live on any of them.
fn current_screen_key(app: &AppHandle) -> Option<Vec<(i32, i32, u32, u32, u64)>> {
    let pref = app
        .try_state::<crate::Shared>()
        .map(|s| s.settings.lock().unwrap().screen.clone())
        .unwrap_or_else(|| "primary".into());
    let key = |m: &Monitor| {
        let (p, size) = (m.position(), m.size());
        (p.x, p.y, size.width, size.height, m.scale_factor().to_bits())
    };
    let mut keys = vec![key(&target_monitor(app, &pref)?)];
    keys.extend(app.available_monitors().ok()?.iter().map(key));
    Some(keys)
}

/// Emits `cursor` (window-logical coordinates) at ~60 Hz while the island is
/// visible. Parked on a condvar the rest of the time, and slowed to 10 Hz with
/// no `cursor` at all while the strip rests (`PollGate::resting`).
pub fn spawn_cursor_poll(app: AppHandle, gate: Arc<PollGate>) {
    std::thread::spawn(move || {
        let mut was_down = false;
        // Remembered across wakes so a display change while hidden is noticed the
        // moment the island comes back.
        let mut last_screen: Option<Vec<(i32, i32, u32, u32, u64)>> = None;
        let (period, rest_period, screen_every) = (16, 100, 30);
        loop {
            gate.wait_until_active();
            let mut last = (f64::MIN, f64::MIN);
            let mut ticks: u32 = 0;
            // Ticks in a row on which the cursor could not be read, and when the last tick was.
            let mut blind: u32 = 0;
            let mut woke = SystemTime::now();
            while gate.is_active() {
                // The island is being dragged: this tick only carries its window
                // under the pointer, and says where it was let go (`island-drop`,
                // the top-centre point of the window; the page decides the rest).
                // Who moves the window is as in upstream pull request 428
                // (`windows/src-tauri/src/island.rs`); written anew.
                // debt: crossing to a display of another scale was never seen (one display here);
                // with the mouse buttons swapped in Windows the held button is not seen.
                let drag = *gate.drag.lock().unwrap();
                if let Some((ox, oy)) = drag {
                    std::thread::sleep(Duration::from_millis(8));
                    let Some(win) = window(&app) else {
                        *gate.drag.lock().unwrap() = None;
                        continue;
                    };
                    match cursor_physical().filter(|_| left_button_down()) {
                        Some((cx, cy)) => {
                            let _ = win.set_position(PhysicalPosition::new(cx as i32 - ox, cy as i32 - oy));
                        }
                        None => {
                            *gate.drag.lock().unwrap() = None;
                            if let (Ok(at), Ok(size)) = (win.outer_position(), win.inner_size()) {
                                let _ = win.emit("island-drop", (at.x + size.width as i32 / 2, at.y));
                            }
                        }
                    }
                    continue;
                }
                let resting = gate.resting.load(Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(if resting { rest_period } else { period }));

                // Monitors get plugged in, unplugged, rearranged and rescaled, and
                // an island pinned to coordinates that no longer exist is an island
                // nobody can reach. Checked about twice a second — the cursor poll
                // is already running, so this costs one monitor query.
                ticks = ticks.wrapping_add(1);
                if ticks % screen_every == 0 {
                    let now = current_screen_key(&app);
                    if now.is_some() && now != last_screen {
                        let first = last_screen.is_none();
                        last_screen = now;
                        if !first {
                            crate::log::line("display layout changed — repositioning".to_string());
                            let _ = app.emit_to(WINDOW_LABEL, "screen-changed", ());
                        }
                    }
                }

                // Lock and sleep put away a strip that would otherwise stay out
                // (Settings › Island "The small island stays"). Told from here, where
                // both show: a tick that took seconds is a computer that slept, and
                // the cursor cannot be read while the lock screen (or a UAC prompt)
                // has the desktop. Said again about twice a second while it lasts,
                // so an open island that folds meanwhile is put away too.
                // debt: svc-sysevents has the events themselves; the host does not link it.
                let slept = woke.elapsed().is_ok_and(|gap| gap > Duration::from_secs(5));
                woke = SystemTime::now();
                let cursor = cursor_physical();
                if slept || (cursor.is_none() && blind % 32 == 0) {
                    let _ = app.emit_to(WINDOW_LABEL, "away", ());
                }
                blind = if cursor.is_none() { blind.wrapping_add(1) } else { 0 };

                let Some(win) = window(&app) else { continue };
                let Ok(origin) = win.outer_position() else { continue };
                let scale = win.scale_factor().unwrap_or(1.0);
                let Some((cx, cy)) = cursor else { continue };
                let x = (cx - origin.x as f64) / scale;
                let y = (cy - origin.y as f64) / scale;
                let size = match win.inner_size() {
                    Ok(s) => (s.width as f64 / scale, s.height as f64 / scale),
                    Err(_) => (PANEL_W, PANEL_H),
                };
                // Over the island shape, a small margin included. Told to the
                // keyboard hook on every tick, moved or not: the island changes
                // shape under a pointer that rests.
                let r = *gate.rect.lock().unwrap();
                let on_island = r.w > 0.0
                    && x >= r.x - HIT_MARGIN
                    && x <= r.x + r.w + HIT_MARGIN
                    && y >= r.y - HIT_MARGIN
                    && y <= r.y + r.h + HIT_MARGIN;
                // Resting: nothing but the pointer coming back matters. What the
                // last full tick decided still stands (the pointer was off the
                // island: clicks pass through, no key is read), and the page is
                // not woken for a pointer that moves somewhere else.
                if resting {
                    if !on_island {
                        continue;
                    }
                    gate.resting.store(false, Ordering::Relaxed);
                }
                platform::keyhook::pointer(on_island && !gate.collapsed.load(Ordering::Relaxed));
                // The user gave the keyboard to another window: the page hears of it.
                if platform::keyboard_was_taken_away(&win) {
                    let _ = win.emit("keyboard-lost", ());
                }
                // A press somewhere else folds the open island. A key of an
                // on-screen keyboard is typing, not leaving.
                if let Some((px, py)) = take_press() {
                    let (lx, ly) = ((px as f64 - origin.x as f64) / scale, (py as f64 - origin.y as f64) / scale);
                    let inside = lx >= r.x - HIT_MARGIN
                        && lx <= r.x + r.w + HIT_MARGIN
                        && ly >= r.y - HIT_MARGIN
                        && ly <= r.y + r.h + HIT_MARGIN;
                    if !inside && !svc_windows::is_onscreen_keyboard_point(px, py) {
                        let _ = win.emit("outside-press", ());
                    }
                }

                if (x - last.0).abs() < 1.0 && (y - last.1).abs() < 1.0 {
                    continue;
                }
                last = (x, y);

                // Click-through: the window only takes the mouse over the island
                // shape. The entry margin means the flag is already off by the
                // time a moving cursor reaches a button.

                // A file being dragged has to be able to find us. WS_EX_TRANSPARENT
                // — what click-through is on Windows — hides the window from
                // WindowFromPoint, so OLE finds no drop target and shows the "no
                // drop" cursor. macOS has no such problem: AppKit delivers drags to
                // registered destinations whatever ignoresMouseEvents says. So while
                // a button is held anywhere over the panel, the whole panel takes
                // the mouse, which also makes the drop zone as forgiving as the Mac's.
                // A press may be the start of a drag: make sure the drop target is
                // ours before the file arrives.
                let down = left_button_down();
                if down && !was_down {
                    let handle = app.clone();
                    let _ = app.run_on_main_thread(move || platform::unblock_webview_drops(&handle));
                }
                was_down = down;

                let dragging = down
                    && x >= 0.0
                    && x <= size.0
                    && y >= 0.0
                    && y <= size.1;

                // The wake strip always takes the mouse. Asked here too, not only
                // in set_collapsed: a tick that was asleep while the island
                // collapsed would otherwise find the pointer off the island and
                // make the strip click-through, after set_collapsed cleared it.
                let accept = on_island || dragging || gate.collapsed.load(Ordering::Relaxed);
                if gate.ignoring.load(Ordering::Relaxed) == accept {
                    gate.ignoring.store(!accept, Ordering::Relaxed);
                    let _ = platform::set_click_through(&win, !accept);
                }

                let _ = win.emit("cursor", CursorPayload { x, y, down });
            }

            // Parked: the window is (or is about to be) the wake strip. A tick
            // that read "not collapsed" a moment before the collapse may still
            // have made it click-through; what this thread asks of the window
            // is done in order, so clearing it from here always lands last.
            // Without it the hidden island only woke for an agent's event.
            // (Both halves are ported from upstream `windows/src-tauri/src/island.rs`,
            // pull requests 114, 124 and 85.)
            if let Some(win) = window(&app) {
                let _ = platform::set_click_through(&win, false);
            }
            gate.forget_ignore_state();
            platform::keyhook::pointer(false);
        }
    });
}

/// Re-applies click-through after the window or the island changed shape: the
/// window takes the mouse again and the next poll tick decides from the cursor.
pub fn refresh_click_through(app: &AppHandle, gate: &PollGate) {
    set_ignore_cursor(app, false);
    gate.forget_ignore_state();
}

pub fn set_ignore_cursor(app: &AppHandle, ignore: bool) {
    if let Some(win) = window(app) {
        let _ = platform::set_click_through(&win, ignore);
    }
}
