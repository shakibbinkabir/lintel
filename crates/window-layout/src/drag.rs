// Snapping by dragging. A window dragged by its title bar, or moved with the
// modifier drag (hold the chosen keys and drag it from anywhere), snaps when it
// is let go in a zone: at a screen edge or corner, or, while the zone key is
// held, in the column of the display it is over (two, three on an ultrawide).
// A window that Lintel tiled gets the size it had before back when it is
// dragged away from its place.
//
// The mouse hook only forwards the events of a drag to a thread of this module,
// which sleeps until one comes and ends with the subscription. Nothing runs
// between drags.
//
// debt: no preview of the zone while dragging; the window lands where the pointer is let go.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;

use svc_input::{Button, MouseKind, Verdict};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetWindowRect, IsZoomed, SendMessageTimeoutW, SetWindowPos, WindowFromPoint, GA_ROOT, HTCAPTION, SMTO_ABORTIFHUNG,
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WM_NCHITTEST,
};

use crate::layout::{displays, pid_of, placeable, set_visible, visible, Display};

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// A title bar drag snaps.
    pub edge: bool,
    /// Keys that, held, make a drag from anywhere move the window (empty: off).
    pub modifier: Vec<i32>,
    /// A key that, held while dragging, makes the zones the columns of the display (0: none).
    pub zones: i32,
}

/// What the module asks of the feature.
pub struct Hooks {
    /// The size a tiled window had before, when it is dragged away from its place.
    pub unsnap: Box<dyn Fn(isize) -> Option<(i32, i32)> + Send>,
    /// Places the window in a zone (the id of a tile or "maximize").
    pub snap: Box<dyn Fn(isize, &str) + Send>,
    /// Apps to leave alone.
    pub skip: Box<dyn Fn(isize) -> bool + Send>,
}

enum Ev {
    Down(i32, i32, bool),
    Move(i32, i32),
    Up,
}

pub struct Drag {
    _sub: svc_input::Sub,
}

fn held(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) < 0 }
}

fn win(hwnd: isize) -> HWND {
    HWND(hwnd as *mut _)
}

/// Where a drop at `at` places a window: an edge (half), a corner (quarter) or the top (the screen) of a
/// side with no display beyond it, or with `columns` the column of the display it is over.
pub fn zone(at: POINT, all: &[Display], columns: bool) -> Option<&'static str> {
    let has = |x: i32, y: i32| all.iter().any(|d| x >= d.rect.left && x < d.rect.right && y >= d.rect.top && y < d.rect.bottom);
    let d = all.iter().find(|d| at.x >= d.rect.left && at.x < d.rect.right && at.y >= d.rect.top && at.y < d.rect.bottom)?;
    let (w, h) = (d.rect.right - d.rect.left, d.rect.bottom - d.rect.top);
    if columns {
        let n = if w * 9 >= h * 21 { 3 } else { 2 };
        let col = ((at.x - d.rect.left) * n / w).clamp(0, n - 1);
        return Some(if n == 3 { ["left-third", "center-third", "right-third"][col as usize] } else { ["left-half", "right-half"][col as usize] });
    }
    let band = 12 * d.dpi as i32 / 96;
    let corner = band * 6;
    let left = at.x < d.rect.left + band && !has(d.rect.left - 1, at.y);
    let right = at.x >= d.rect.right - band && !has(d.rect.right, at.y);
    let top = at.y < d.rect.top + band && !has(at.x, d.rect.top - 1);
    let bottom = at.y >= d.rect.bottom - band && !has(at.x, d.rect.bottom);
    let (near_top, near_bottom) = (at.y < d.rect.top + corner, at.y >= d.rect.bottom - corner);
    let (near_left, near_right) = (at.x < d.rect.left + corner, at.x >= d.rect.right - corner);
    if left || right {
        let side = if left { "left" } else { "right" };
        return Some(match (near_top, near_bottom, left) {
            (true, _, true) => "top-left",
            (true, _, false) => "top-right",
            (_, true, true) => "bottom-left",
            (_, true, false) => "bottom-right",
            _ if side == "left" => "left-half",
            _ => "right-half",
        });
    }
    if top {
        return Some(if near_left { "top-left" } else if near_right { "top-right" } else { "maximize" });
    }
    if bottom {
        return if near_left { Some("bottom-left") } else if near_right { Some("bottom-right") } else { None };
    }
    None
}

struct Grab {
    hwnd: isize,
    at: (i32, i32),
    modifier: bool,
    moved: bool,
    /// The pointer from the window's top left corner, and the border that is not drawn.
    off: (i32, i32),
    border: (i32, i32),
    all: Vec<Display>,
    zone: Option<&'static str>,
}

fn border(hwnd: isize) -> (i32, i32) {
    let (seen, mut outer) = (visible(hwnd), RECT::default());
    unsafe {
        let _ = GetWindowRect(win(hwnd), &mut outer);
    }
    (seen.left - outer.left, seen.top - outer.top)
}

/// The window under a press, if it may be moved: a title bar for the plain drag, anywhere for the modifier drag.
fn begin(x: i32, y: i32, modifier: bool, hooks: &Hooks) -> Option<Grab> {
    let (hit, root) = unsafe {
        let hit = WindowFromPoint(POINT { x, y });
        (hit, GetAncestor(hit, GA_ROOT))
    };
    let hwnd = root.0 as isize;
    if hwnd == 0 || pid_of(hwnd) == std::process::id() || !placeable(hwnd) || unsafe { IsZoomed(root).as_bool() } || (hooks.skip)(hwnd) {
        return None;
    }
    if !modifier {
        let mut r = 0usize;
        let sent = unsafe {
            SendMessageTimeoutW(hit, WM_NCHITTEST, WPARAM(0), LPARAM(((y & 0xFFFF) << 16 | (x & 0xFFFF)) as isize), SMTO_ABORTIFHUNG, 50, Some(&mut r))
        };
        if sent.0 == 0 || r != HTCAPTION as usize {
            return None;
        }
    } else {
        svc_windows::activate(hwnd);
    }
    let seen = visible(hwnd);
    Some(Grab { hwnd, at: (x, y), modifier, moved: false, off: (x - seen.left, y - seen.top), border: border(hwnd), all: displays(), zone: None })
}

fn moving(g: &mut Grab, x: i32, y: i32, hooks: &Hooks, zones: i32) {
    if !g.moved {
        if (x - g.at.0).abs().max((y - g.at.1).abs()) < 6 {
            return;
        }
        g.moved = true;
        // Dragged away from where it was tiled: the size it had, the grab point kept inside it.
        if let Some((w, h)) = (hooks.unsnap)(g.hwnd) {
            g.off = (g.off.0.min(w - 1), g.off.1.min(h - 1));
            let to = RECT { left: x - g.off.0, top: y - g.off.1, right: x - g.off.0 + w, bottom: y - g.off.1 + h };
            set_visible(g.hwnd, to, true);
            g.border = border(g.hwnd);
        }
    }
    if g.modifier {
        unsafe {
            let _ = SetWindowPos(
                win(g.hwnd),
                None,
                x - g.off.0 - g.border.0,
                y - g.off.1 - g.border.1,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
    }
    g.zone = zone(POINT { x, y }, &g.all, zones != 0 && held(zones));
}

pub fn start(config: Config, hooks: Hooks) -> Drag {
    let (tx, rx) = mpsc::channel::<Ev>();
    let (tracking, swallowing) = (AtomicBool::new(false), AtomicBool::new(false));
    let c = config.clone();
    let sub = svc_input::mouse(20, false, move |m| {
        if m.injected {
            return Verdict::Pass;
        }
        match m.kind {
            MouseKind::Down(Button::Left) => {
                let modifier = !c.modifier.is_empty() && c.modifier.iter().all(|k| held(*k));
                if modifier || c.edge {
                    tracking.store(true, Relaxed);
                    swallowing.store(modifier, Relaxed);
                    let _ = tx.send(Ev::Down(m.x, m.y, modifier));
                }
                if modifier { Verdict::Swallow } else { Verdict::Pass }
            }
            MouseKind::Move if tracking.load(Relaxed) => {
                let _ = tx.send(Ev::Move(m.x, m.y));
                Verdict::Pass
            }
            MouseKind::Up(Button::Left) if tracking.swap(false, Relaxed) => {
                let _ = tx.send(Ev::Up);
                if swallowing.swap(false, Relaxed) { Verdict::Swallow } else { Verdict::Pass }
            }
            _ => Verdict::Pass,
        }
    });
    let zones = config.zones;
    std::thread::spawn(move || {
        let mut grab: Option<Grab> = None;
        // Over when the subscription, which holds the sender, is gone.
        while let Ok(ev) = rx.recv() {
            match ev {
                Ev::Down(x, y, modifier) => grab = begin(x, y, modifier, &hooks),
                Ev::Move(x, y) => {
                    if let Some(g) = grab.as_mut() {
                        moving(g, x, y, &hooks, zones);
                    }
                }
                Ev::Up => {
                    if let Some(g) = grab.take().filter(|g| g.moved) {
                        if let Some(z) = g.zone {
                            // The system's own move loop ends a moment after the button went up.
                            std::thread::sleep(Duration::from_millis(60));
                            (hooks.snap)(g.hwnd, z);
                        }
                    }
                }
            }
        }
    });
    Drag { _sub: sub }
}
