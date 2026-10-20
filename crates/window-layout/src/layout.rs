// Where a window goes, and putting it there.
//
// The geometry is ported from the upstream WindowLayoutSupport.swift
// (`WindowLayoutGeometry`: the shares of the screen, the two gaps, the edge a
// window that kept a size of its own is held against, the move to another
// display, the order of the displays; `WindowLayoutHistory`). The upstream
// measures from the bottom of the screen and has one case per placement; here
// everything is physical pixels from the top and a placement is a share of the
// work area in twelfths, which says the same in a table.
//
// What Windows adds: a window that can be resized is wider than what is seen
// of it (its resize border is not drawn), so every rectangle here is the frame
// DWM reports, and the border is added back when the window is moved.
//
// The caller's thread must see physical pixels (per-monitor DPI awareness):
// the app does, and the probe asks for it.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsHungAppWindow,
    IsIconic, IsWindowVisible, IsZoomed, SetCursorPos, SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE, SET_WINDOW_POS_FLAGS,
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_SHOWNOACTIVATE, WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_MAXIMIZEBOX, WS_THICKFRAME,
};

// ── Shares of the work area ───────────────────────────────────────────────────

/// Left, right, top and bottom of a placement, in twelfths of the work area.
pub type Share = [i32; 4];

const FULL: (i32, i32) = (0, 12);

/// The placements that tile: id, English name, share.
pub const TILES: &[(&str, &str, Share)] = &[
    ("left-half", "Left half", [0, 6, 0, 12]),
    ("right-half", "Right half", [6, 12, 0, 12]),
    ("top-half", "Top half", [0, 12, 0, 6]),
    ("bottom-half", "Bottom half", [0, 12, 6, 12]),
    ("top-left", "Top left", [0, 6, 0, 6]),
    ("top-right", "Top right", [6, 12, 0, 6]),
    ("bottom-left", "Bottom left", [0, 6, 6, 12]),
    ("bottom-right", "Bottom right", [6, 12, 6, 12]),
    ("left-third", "Left third", [0, 4, 0, 12]),
    ("center-third", "Center third", [4, 8, 0, 12]),
    ("right-third", "Right third", [8, 12, 0, 12]),
    ("left-two-thirds", "Left two thirds", [0, 8, 0, 12]),
    ("right-two-thirds", "Right two thirds", [4, 12, 0, 12]),
];

/// Pixels a window may miss a frame by and still count as sitting there.
pub const TOLERANCE: i32 = 8;

pub fn width(r: RECT) -> i32 {
    r.right - r.left
}

pub fn height(r: RECT) -> i32 {
    r.bottom - r.top
}

pub fn close(a: RECT, b: RECT, by: i32) -> bool {
    (a.left - b.left).abs() <= by && (a.top - b.top).abs() <= by && (a.right - b.right).abs() <= by && (a.bottom - b.bottom).abs() <= by
}

/// The frame of a share. The screen gap, one per edge (left, top, right,
/// bottom), pulls the work area in (80 pixels of each axis always stay); the window gap is the whole distance
/// between two neighbours, so each gives up half of it on every edge it shares,
/// which is every edge that is not on the screen's.
pub fn tile_rect(share: Share, work: RECT, window_gap: i32, screen_gap: [i32; 4]) -> RECT {
    // Two gaps on one axis share the room that is left, in proportion.
    let pair = |a: i32, b: i32, len: i32| {
        let (a, b, room) = (a.max(0), b.max(0), (len - 80).max(0));
        if a + b <= room { (a, b) } else { (room * a / (a + b), room * b / (a + b)) }
    };
    let (dl, dr) = pair(screen_gap[0], screen_gap[2], width(work));
    let (dt, db) = pair(screen_gap[1], screen_gap[3], height(work));
    let (left, top, w, h) = (work.left + dl, work.top + dt, width(work) - dl - dr, height(work) - dt - db);
    let half = window_gap / 2;
    let inner = |at: i32| if at > 0 && at < 12 { half } else { 0 };
    RECT {
        left: left + w * share[0] / 12 + inner(share[0]),
        right: left + w * share[1] / 12 - inner(share[1]),
        top: top + h * share[2] / 12 + inner(share[2]),
        bottom: top + h * share[3] / 12 - inner(share[3]),
    }
}

/// Where a window that kept the size `size` goes instead of `to`: against the
/// right or the bottom when its share ends there, in the middle when its share
/// touches neither end, from the left or the top otherwise, and always inside
/// the work area. Without a share (centre, almost maximise): the middle.
pub fn anchored(share: Option<Share>, to: RECT, size: (i32, i32), work: RECT) -> RECT {
    let along = |lo: i32, hi: i32, from: i32, until: i32, len: i32, first: i32, last: i32| {
        let at = if until == 12 && from != 0 {
            hi - len
        } else if from == 0 {
            lo
        } else {
            (lo + hi - len) / 2
        };
        if len <= last - first { at.clamp(first, last - len) } else { first }
    };
    let s = share.unwrap_or([1, 11, 1, 11]);
    let x = along(to.left, to.right, s[0], s[1], size.0, work.left, work.right);
    let y = along(to.top, to.bottom, s[2], s[3], size.1, work.top, work.bottom);
    RECT { left: x, top: y, right: x + size.0, bottom: y + size.1 }
}

// ── The arrows ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Tile(Share),
    Maximize,
    /// Through the screen's edge onto the display on that side, against the edge it came in by.
    Cross(Share),
}

/// What an arrow does to a window that sits on `now` (None: anywhere else).
/// The half on that side first. Then, sideways: two thirds, one third and the
/// half again, or with `cycle` off on to the next display. The top half again
/// fills the screen, the bottom half again goes to the display below. An arrow
/// across a half makes a quarter, an arrow along a quarter moves it over, and
/// an arrow into the corner it is in grows it back to a half.
pub fn arrow(dir: Dir, now: Option<Share>, cycle: bool) -> Step {
    let start = matches!(dir, Dir::Left | Dir::Up);
    let sideways = matches!(dir, Dir::Left | Dir::Right);
    let (near, far) = if start { ((0, 6), (6, 12)) } else { ((6, 12), (0, 6)) };
    let (wide, narrow) = if start { ((0, 8), (0, 4)) } else { ((4, 12), (8, 12)) };
    // `a` runs along the arrow, `o` across it.
    let share = |a: (i32, i32), o: (i32, i32)| if sideways { [a.0, a.1, o.0, o.1] } else { [o.0, o.1, a.0, a.1] };
    let half = Step::Tile(share(near, FULL));
    let Some(s) = now else { return half };
    let (a, o) = if sideways { ((s[0], s[1]), (s[2], s[3])) } else { ((s[2], s[3]), (s[0], s[1])) };
    if o == FULL {
        return match a {
            a if a == near && sideways && cycle => Step::Tile(share(wide, FULL)),
            a if a == wide && sideways && cycle => Step::Tile(share(narrow, FULL)),
            a if a == near && dir == Dir::Up => Step::Maximize,
            a if a == near => Step::Cross(share(far, FULL)),
            _ => half,
        };
    }
    if (o == (0, 6) || o == (6, 12)) && (a == FULL || a == far) {
        return Step::Tile(share(near, o));
    }
    half
}

// ── Displays ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Display {
    /// Windows' handle for it.
    pub handle: isize,
    /// The whole display.
    pub rect: RECT,
    /// Without the taskbar.
    pub work: RECT,
    /// 96 at 100 %, 120 at 125 %.
    pub dpi: u32,
}

pub fn displays() -> Vec<Display> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, lparam: LPARAM) -> BOOL {
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (mut dpi, mut dpi_y) = (96u32, 96u32);
        unsafe {
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
                let display = Display { handle: monitor.0 as isize, rect: info.rcMonitor, work: info.rcWork, dpi: dpi.max(1) };
                (*(lparam.0 as *mut Vec<Display>)).push(display);
            }
        }
        true.into()
    }
    let mut all: Vec<Display> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    all
}

fn index_of(all: &[Display], monitor: HMONITOR) -> Option<usize> {
    all.iter().position(|d| d.handle == monitor.0 as isize)
}

/// The next display (or the one before), all of them in a ring from left to
/// right, then from the top, then by size.
pub fn adjacent(all: &[Display], at: usize, forward: bool) -> Option<usize> {
    if all.len() < 2 || at >= all.len() {
        return None;
    }
    let mut order: Vec<usize> = (0..all.len()).collect();
    order.sort_by_key(|&i| (all[i].rect.left, all[i].rect.top, width(all[i].rect), height(all[i].rect), i));
    let place = order.iter().position(|&i| i == at)?;
    Some(order[(place + if forward { 1 } else { order.len() - 1 }) % order.len()])
}

/// The display on one side. Sideways, one that starts further that way; up and
/// down, one that lies wholly beyond the edge. The nearest wins, then the one
/// most in line.
pub fn neighbour(all: &[Display], at: usize, dir: Dir) -> Option<usize> {
    let here = all.get(at)?.rect;
    let sideways = matches!(dir, Dir::Left | Dir::Right);
    let middle = |r: RECT| if sideways { (r.top + r.bottom) / 2 } else { (r.left + r.right) / 2 };
    (0..all.len())
        .filter(|&i| {
            let r = all[i].rect;
            match dir {
                Dir::Left => r.left < here.left,
                Dir::Right => r.left > here.left,
                Dir::Up => r.bottom <= here.top,
                Dir::Down => r.top >= here.bottom,
            }
        })
        .min_by_key(|&i| {
            let r = all[i].rect;
            let away = match dir {
                Dir::Left => -r.left,
                Dir::Right => r.left,
                Dir::Up => -r.bottom,
                Dir::Down => r.top,
            };
            (away, (middle(r) - middle(here)).abs(), i)
        })
}

/// The frame a window takes on another display. It keeps the size it has to
/// the eye (the two displays' scaling counted in), shrinking only to fit, and
/// its distance from the left and the top, or from the right or the bottom when
/// it sat against that edge. A window that filled its display fills the next.
pub fn rect_for_display(current: RECT, from: &Display, to: &Display) -> RECT {
    let (src, dst) = (from.work, to.work);
    if width(current) >= width(src) - TOLERANCE && height(current) >= height(src) - TOLERANCE {
        return dst;
    }
    let scaled = |v: i32| (v as i64 * to.dpi as i64 / from.dpi as i64) as i32;
    let axis = |s: (i32, i32), c: (i32, i32), d: (i32, i32)| {
        let len = scaled(c.1 - c.0).min(d.1 - d.0).max(1);
        let (lead, trail) = (c.0 - s.0, s.1 - c.1);
        let slack = (s.1 - s.0) - (c.1 - c.0);
        let at = if slack > TOLERANCE && trail <= TOLERANCE && trail < lead { d.1 - scaled(trail) - len } else { d.0 + scaled(lead) };
        let at = at.max(d.0).min(d.1 - len);
        (at, at + len)
    };
    let (left, right) = axis((src.left, src.right), (current.left, current.right), (dst.left, dst.right));
    let (top, bottom) = axis((src.top, src.bottom), (current.top, current.bottom), (dst.top, dst.bottom));
    RECT { left, top, right, bottom }
}

/// Sends the pointer to the middle of the next display. False with one display.
pub fn pointer_to_next_display() -> bool {
    let all = displays();
    let mut at = POINT::default();
    unsafe {
        if GetCursorPos(&mut at).is_err() {
            return false;
        }
        let Some(next) = index_of(&all, MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST)).and_then(|i| adjacent(&all, i, true)) else {
            return false;
        };
        let r = all[next].rect;
        SetCursorPos((r.left + r.right) / 2, (r.top + r.bottom) / 2).is_ok()
    }
}

// ── One window ────────────────────────────────────────────────────────────────

fn handle(hwnd: isize) -> HWND {
    HWND(hwnd as *mut _)
}

/// 0 for a window that is gone.
pub fn pid_of(hwnd: isize) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(handle(hwnd), Some(&mut pid)) };
    pid
}

pub fn class_of(hwnd: isize) -> String {
    let mut name = [0u16; 128];
    let len = unsafe { GetClassNameW(handle(hwnd), &mut name) }.max(0) as usize;
    String::from_utf16_lossy(&name[..len])
}

/// The desktop and the taskbars: never arranged, never brought to the front.
pub fn is_shell(hwnd: isize) -> bool {
    matches!(class_of(hwnd).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

/// What is seen of a window: its frame without the resize border that is not drawn.
pub fn visible(hwnd: isize) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let size = std::mem::size_of::<RECT>() as u32;
        if DwmGetWindowAttribute(handle(hwnd), DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut _, size).is_err() {
            let _ = GetWindowRect(handle(hwnd), &mut r);
        }
    }
    r
}

fn style(hwnd: isize) -> u32 {
    unsafe { GetWindowLongW(handle(hwnd), GWL_STYLE) as u32 }
}

/// A window a placement may move: on screen, not minimised, not the island or
/// another panel that never takes the keyboard, not the desktop, and not one
/// that covers its whole display without a title bar (a game, a film).
pub fn placeable(hwnd: isize) -> bool {
    let win = handle(hwnd);
    unsafe {
        if hwnd == 0 || !IsWindowVisible(win).as_bool() || IsIconic(win).as_bool() || is_shell(hwnd) {
            return false;
        }
        if GetWindowLongW(win, GWL_EXSTYLE) as u32 & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return false;
        }
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(win, MONITOR_DEFAULTTONEAREST), &mut info);
        visible(hwnd) != info.rcMonitor || style(hwnd) & WS_CAPTION.0 == WS_CAPTION.0
    }
}

/// The window a placement is for: the one in front, or the first one behind it
/// when that is the island or the desktop. Of the app's own windows, the ones
/// that float are never it (the island while it has the keyboard, a panel),
/// and with `skip_own` none is (a click on the island asked).
pub fn target(skip_own: bool) -> Option<isize> {
    let me = std::process::id();
    let floats = |hwnd: isize| unsafe { GetWindowLongW(handle(hwnd), GWL_EXSTYLE) as u32 } & WS_EX_TOPMOST.0 != 0;
    let fits = |hwnd: isize| placeable(hwnd) && !(pid_of(hwnd) == me && (skip_own || floats(hwnd)));
    let front = unsafe { GetForegroundWindow() }.0 as isize;
    if fits(front) {
        return Some(front);
    }
    svc_windows::windows().into_iter().map(|w| w.hwnd).find(|hwnd| fits(*hwnd))
}

/// Moves the window so that what is seen of it is `to`. False when Windows
/// refuses (a window that runs as administrator).
pub fn set_visible(hwnd: isize, to: RECT, resize: bool) -> bool {
    let (mut outer, seen) = (RECT::default(), visible(hwnd));
    unsafe {
        if GetWindowRect(handle(hwnd), &mut outer).is_err() {
            return false;
        }
        // The border that is not drawn, side by side: none at the top, some pixels elsewhere, more on a scaled display.
        let (l, t, r, b) = (seen.left - outer.left, seen.top - outer.top, outer.right - seen.right, outer.bottom - seen.bottom);
        let flags = SWP_NOZORDER | SWP_NOACTIVATE | if resize { SET_WINDOW_POS_FLAGS(0) } else { SWP_NOSIZE };
        // debt: not SWP_ASYNCWINDOWPOS, so the frame can be read back at once; an app that hangs right now holds this call.
        SetWindowPos(handle(hwnd), None, to.left - l, to.top - t, width(to) + l + r, height(to) + t + b, flags).is_ok()
    }
}

/// Puts the window on `to` and says whether it sits there. A window that keeps
/// a size of its own (a minimum, or it cannot be resized at all) is held against
/// the edge its share touches instead.
fn put(hwnd: isize, to: RECT, share: Option<Share>, work: RECT) -> bool {
    unsafe {
        if IsZoomed(handle(hwnd)).as_bool() {
            // Back to a window, without taking the keyboard.
            let _ = ShowWindow(handle(hwnd), SW_SHOWNOACTIVATE);
        }
    }
    let size_of = |hwnd: isize| (width(visible(hwnd)), height(visible(hwnd)));
    let fixed = style(hwnd) & WS_THICKFRAME.0 == 0;
    // Twice at most: on a display with another scaling the border has another
    // width, and the app may resize itself as it arrives.
    for _ in 0..2 {
        let want = if fixed { anchored(share, to, size_of(hwnd), work) } else { to };
        if !set_visible(hwnd, want, !fixed) {
            return false;
        }
        if close(visible(hwnd), want, 1) {
            return true;
        }
    }
    // It kept a size of its own: against its edge, once.
    let want = anchored(share, to, size_of(hwnd), work);
    set_visible(hwnd, want, false) && close(visible(hwnd), want, 1)
}

/// Maximised the way the title bar's button does it, so the button puts it back.
fn maximize(hwnd: isize, work: RECT) -> bool {
    if style(hwnd) & WS_MAXIMIZEBOX.0 == 0 {
        return put(hwnd, work, None, work);
    }
    unsafe {
        let _ = ShowWindow(handle(hwnd), SW_MAXIMIZE);
        IsZoomed(handle(hwnd)).as_bool()
    }
}

/// Keeps the window above the others (or lets it go back), and says whether it did.
pub fn set_topmost(hwnd: isize, on: bool) -> bool {
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe { SetWindowPos(handle(hwnd), Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE).is_ok() }
}

pub fn is_topmost(hwnd: isize) -> bool {
    (unsafe { GetWindowLongW(handle(hwnd), GWL_EXSTYLE) } as u32) & WS_EX_TOPMOST.0 != 0
}

// ── Placements, and the way back ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Between two windows side by side, in pixels at 100 %.
    pub window_gap: i32,
    /// Between a window and the edge of the screen, in pixels at 100 %.
    pub screen_gap: i32,
    /// Left, top, right and bottom edge of its own, in place of `screen_gap`.
    pub edge_gaps: [Option<i32>; 4],
    /// Almost maximise leaves this share of the screen free on every side, in percent.
    pub margin: i32,
    /// A repeated Left or Right changes the width instead of the display.
    pub cycle: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { window_gap: 0, screen_gap: 0, edge_gaps: [None; 4], margin: 5, cycle: true }
    }
}

impl Options {
    pub fn tile(&self, share: Share, on: &Display) -> RECT {
        let px = |v: i32| v * on.dpi as i32 / 96;
        tile_rect(share, on.work, px(self.window_gap), self.edge_gaps.map(|g| px(g.unwrap_or(self.screen_gap))))
    }

    /// The share a frame sits on, if it sits on one.
    pub fn occupies(&self, frame: RECT, on: &Display) -> Option<Share> {
        TILES.iter().map(|t| t.2).find(|s| close(self.tile(*s, on), frame, TOLERANCE))
    }
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    rect: RECT,
    zoomed: bool,
}

fn frame(hwnd: isize) -> Frame {
    Frame { rect: visible(hwnd), zoomed: unsafe { IsZoomed(handle(hwnd)).as_bool() } }
}

fn same(a: Frame, b: Frame) -> bool {
    a.zoomed == b.zoomed && close(a.rect, b.rect, 1)
}

/// The frames each window had before its placements, twenty at most, for the
/// session. A window is known by its handle and its process: Windows hands a
/// closed window's handle out again.
#[derive(Default)]
pub struct Layout {
    history: Mutex<HashMap<(isize, u32), Vec<Frame>>>,
    /// Where a placement left a window, and the size it had before the first one.
    placed: Mutex<HashMap<(isize, u32), (RECT, (i32, i32))>>,
}

impl Layout {
    /// One action on one window: "left", "right", "up", "down" (the arrows),
    /// the id of a tile, "maximize", "almost-maximize", "center",
    /// "next-display", "previous-display" or "restore". Says how it went:
    /// "done", "restored", "no-window", "nothing-to-restore", "one-display" or
    /// "failed".
    /// The size a window had before Lintel tiled it, when it still sits where the
    /// tiling left it (once: a second call says None until it is tiled again).
    pub fn unsnap(&self, hwnd: isize) -> Option<(i32, i32)> {
        let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
        let key = (hwnd, pid_of(hwnd));
        let (at, size) = *placed.get(&key)?;
        placed.remove(&key);
        (close(at, visible(hwnd), TOLERANCE) && !unsafe { IsZoomed(handle(hwnd)).as_bool() }).then_some(size)
    }

    pub fn apply(&self, hwnd: isize, action: &str, o: &Options) -> &'static str {
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        // Closed windows are forgotten here, on an action, never on a timer.
        history.retain(|(hwnd, pid), _| *pid != 0 && pid_of(*hwnd) == *pid);
        if !placeable(hwnd) {
            return "no-window";
        }
        if unsafe { IsHungAppWindow(handle(hwnd)).as_bool() } {
            return "failed";
        }
        let key = (hwnd, pid_of(hwnd));
        let before = frame(hwnd);
        let all = displays();
        let Some(at) = index_of(&all, unsafe { MonitorFromWindow(handle(hwnd), MONITOR_DEFAULTTONEAREST) }) else { return "failed" };
        let here = all[at];
        let dir = match action {
            "left" => Some(Dir::Left),
            "right" => Some(Dir::Right),
            "up" => Some(Dir::Up),
            "down" => Some(Dir::Down),
            _ => None,
        };

        let there = if action == "restore" {
            let mut previous = None;
            if let Some(frames) = history.get_mut(&key) {
                // The last frame that is not the one it has now.
                while let Some(f) = frames.pop() {
                    if !same(f, before) {
                        previous = Some(f);
                        break;
                    }
                }
            }
            let Some(f) = previous else { return "nothing-to-restore" };
            let back = if f.zoomed {
                // Maximised on the display it was maximised on.
                let on = unsafe { MonitorFromRect(&f.rect, MONITOR_DEFAULTTONEAREST) };
                (index_of(&all, on) == Some(at) || put(hwnd, f.rect, None, f.rect)) && maximize(hwnd, f.rect)
            } else {
                put(hwnd, f.rect, None, f.rect)
            };
            return if back { "restored" } else { "failed" };
        } else if let Some(dir) = dir {
            let now = if before.zoomed { None } else { o.occupies(before.rect, &here) };
            match arrow(dir, now, o.cycle) {
                Step::Tile(share) => put(hwnd, o.tile(share, &here), Some(share), here.work),
                Step::Maximize => maximize(hwnd, here.work),
                Step::Cross(share) => match neighbour(&all, at, dir) {
                    Some(next) => put(hwnd, o.tile(share, &all[next]), Some(share), all[next].work),
                    // No display on that side: it stays on its half.
                    None => true,
                },
            }
        } else if let Some(tile) = TILES.iter().find(|t| t.0 == action) {
            put(hwnd, o.tile(tile.2, &here), Some(tile.2), here.work)
        } else {
            let work = here.work;
            match action {
                "maximize" => maximize(hwnd, work),
                "almost-maximize" => {
                    let (dx, dy) = (width(work) * o.margin / 100, height(work) * o.margin / 100);
                    put(hwnd, RECT { left: work.left + dx, top: work.top + dy, right: work.right - dx, bottom: work.bottom - dy }, None, work)
                }
                "center" => {
                    let size = (width(before.rect).min(width(work)), height(before.rect).min(height(work)));
                    put(hwnd, anchored(None, work, size, work), None, work)
                }
                "next-display" | "previous-display" => {
                    let Some(next) = adjacent(&all, at, action == "next-display") else { return "one-display" };
                    if before.zoomed {
                        // As a window first: a maximised one does not leave its display.
                        unsafe {
                            let _ = ShowWindow(handle(hwnd), SW_SHOWNOACTIVATE);
                        }
                        let to = rect_for_display(visible(hwnd), &here, &all[next]);
                        put(hwnd, to, None, all[next].work) && maximize(hwnd, all[next].work)
                    } else {
                        put(hwnd, rect_for_display(before.rect, &here, &all[next]), None, all[next].work)
                    }
                }
                _ => return "failed",
            }
        };

        // Remember where a tiling left it, and the size it had before the first one.
        // Centre, display moves and maximising are not placements to come back from.
        let tiled = dir.is_some() || TILES.iter().any(|t| t.0 == action);
        {
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            let after = frame(hwnd);
            if there && tiled && !before.zoomed && !after.zoomed {
                let old = placed.get(&key).filter(|p| close(p.0, before.rect, TOLERANCE)).map(|p| p.1);
                placed.insert(key, (after.rect, old.unwrap_or((width(before.rect), height(before.rect)))));
            } else {
                placed.remove(&key);
            }
        }
        if !same(frame(hwnd), before) {
            let frames = history.entry(key).or_default();
            frames.push(before);
            if frames.len() > 20 {
                frames.remove(0);
            }
        }
        // debt: a window that would not go is left where it ended up; the upstream puts the first frame back.
        if there { "done" } else { "failed" }
    }
}
