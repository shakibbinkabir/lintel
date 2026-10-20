// Radial menu — a Lintel feature. The contract is docs/ARCHITECTURE.md §2: nothing
// runs before `start`, everything is gone after `stop`, and every command of
// the feature arrives through `call`.
//
// The wheel is one transparent, topmost window that never takes the keyboard
// (`no_activate`) and is click-through. It is created on the first summons and
// then only hidden, so the next one opens at once; it is destroyed with the
// feature. While off, and while on but closed, nothing runs: no hook, no timer,
// no thread. A summons (a shortcut) starts one worker thread and, for as long
// as the wheel is open, the two low-level hooks of svc-input (the pointer, and
// the keys that are released or answered: Esc, Enter, arrows, digits). The
// window only draws; this side decides what is under the pointer
// (`geometry.rs`), what a click or a key does, and runs the slice.
//
// Behaviour follows the upstream utility's radial menu (RadialMenuService.swift,
// RadialMenuSupport.swift): the pointer has to travel before anything
// highlights, highlighting is by angle at any distance, hold the shortcut and
// release to run, a quick tap leaves the wheel open to click, a submenu opens
// when the pointer rests on it.

pub mod geometry;

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine;
use geometry::Zone;
use notch_core::{Ctx, Feature, Group, Manifest, Monitor, ShortcutDef, Use, WindowSpec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use svc_input::{Button, MouseKind, Verdict};
use tauri::{Manager, PhysicalPosition, PhysicalSize};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

const ID: &str = "radial-menu";
const LABEL: &str = "radial-wheel";
/// Longest wheel, and the rule of upstream: a wheel and one level of submenus.
const MAX_SLICES: usize = 12;
/// A pointer resting on a submenu this long opens it.
const DWELL: Duration = Duration::from_millis(350);
/// Between the close and the slice running: the wheel's exit animation (--d-fast).
const EXIT: Duration = Duration::from_millis(150);
const NONE: usize = usize::MAX;

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Radial menu",
    group: Group::Tools,
    default_on: false,
    uses: &[Use::InputHook],
    // Default Ctrl+Alt+R: not in the list of ARCHITECTURE §2.1, nor Ctrl+Space, Ctrl+Alt+L, Ctrl+Alt+Shift+3 or +4.
    // The second and third wheel stay unbound until the user picks keys.
    shortcuts: &[
        ShortcutDef { id: "wheel", label: "Open the first wheel", default: Some("Ctrl+Alt+R") },
        ShortcutDef { id: "wheel2", label: "Open the second wheel", default: None },
        ShortcutDef { id: "wheel3", label: "Open the third wheel", default: None },
    ],
    tray: &[],
    defaults: r#"{
  "mode": "pressOrHold",
  "atPointer": true,
  "profiles": [
    { "name": "General", "items": [
      { "kind": "media", "target": "playPause" },
      { "kind": "media", "target": "next" },
      { "kind": "file", "target": "shell:Downloads", "name": "Downloads", "icon": "download" },
      { "kind": "link", "target": "ms-settings:", "name": "Settings", "icon": "gear" },
      { "kind": "nowplaying" },
      { "kind": "media", "target": "prev" }
    ] }
  ]
}"#,
};

// ── Slices ────────────────────────────────────────────────────────────────────

/// One slice. `kind`: app (target = AppUserModelID), file (a path, a folder or a
/// shell: location), link, media (target = playPause | next | prev), nowplaying,
/// shortcut (target = "Ctrl+Shift+S"), command (target = a command line or script path, run hidden), submenu (items).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Item {
    pub kind: String,
    pub name: String,
    pub target: String,
    pub icon: String,
    pub items: Vec<Item>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Profile {
    name: String,
    items: Vec<Item>,
}

/// What can run is kept, item by item: a slice of a kind this version does not
/// know, or without its target, drops out; the rest of the wheel stays.
pub fn clean(items: Vec<Item>, depth: u8) -> Vec<Item> {
    items
        .into_iter()
        .filter_map(|mut it| {
            it.name = it.name.trim().chars().take(60).collect();
            it.icon = it.icon.trim().chars().take(40).collect();
            it.target = it.target.trim().to_string();
            let keep = match it.kind.as_str() {
                "app" | "file" | "link" | "shortcut" | "command" => !it.target.is_empty(),
                "media" => matches!(it.target.as_str(), "playPause" | "next" | "prev"),
                "nowplaying" => true,
                "submenu" if depth == 0 => {
                    it.items = clean(std::mem::take(&mut it.items), 1);
                    !it.items.is_empty()
                }
                _ => false,
            };
            if it.kind != "submenu" {
                it.items.clear();
            }
            keep.then_some(it)
        })
        .take(MAX_SLICES)
        .collect()
}

fn wants_media(items: &[Item]) -> bool {
    items.iter().any(|i| matches!(i.kind.as_str(), "media" | "nowplaying") || wants_media(&i.items))
}

// ── Where the wheel stands ────────────────────────────────────────────────────

fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    let _ = unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

/// The work area of the display under a point, and its scale.
fn screen(at: (i32, i32)) -> (geometry::Rect, f64) {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: at.0, y: at.1 }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let w = info.rcWork;
        (geometry::Rect { left: w.left, top: w.top, right: w.right, bottom: w.bottom }, dx.max(96) as f64 / 96.0)
    }
}

// ── What the hooks share with the worker ──────────────────────────────────────

/// 256 bits, one per virtual key.
#[derive(Default)]
struct Bits([AtomicU64; 4]);

impl Bits {
    fn set(&self, vk: u16) {
        self.0[(vk >> 6 & 3) as usize].fetch_or(1 << (vk & 63), Relaxed);
    }
    fn clear(&self, vk: u16) -> bool {
        self.0[(vk >> 6 & 3) as usize].fetch_and(!(1 << (vk & 63)), Relaxed) >> (vk & 63) & 1 == 1
    }
    fn has(&self, vk: u16) -> bool {
        self.0[(vk >> 6 & 3) as usize].load(Relaxed) >> (vk & 63) & 1 == 1
    }
    fn any_down(&self) -> bool {
        (8u16..=254).any(|vk| self.has(vk) && unsafe { GetAsyncKeyState(vk as i32) } < 0)
    }
}

/// The wheel as the pointer hook sees it: read on the hook's thread, so atomics only.
struct Hot {
    cx: AtomicI32,
    cy: AtomicI32,
    scale: AtomicU64,
    n: AtomicUsize,
    /// The slice last reported, so that a move that changes nothing sends nothing.
    last: AtomicUsize,
    armed: std::sync::atomic::AtomicBool,
    ox: AtomicI32,
    oy: AtomicI32,
    /// Buttons whose press was swallowed: their release is too.
    swallowed: AtomicU32,
}

impl Hot {
    fn scale(&self) -> f64 {
        f64::from_bits(self.scale.load(Relaxed))
    }
    fn offset(&self, x: i32, y: i32) -> (f64, f64) {
        let s = self.scale();
        ((x - self.cx.load(Relaxed)) as f64 / s, (y - self.cy.load(Relaxed)) as f64 / s)
    }
    /// A new level: nothing highlights until the pointer has moved again.
    fn rearm(&self, n: usize) {
        let (x, y) = cursor();
        self.ox.store(x, Relaxed);
        self.oy.store(y, Relaxed);
        self.n.store(n, Relaxed);
        self.last.store(NONE, Relaxed);
        self.armed.store(false, Relaxed);
    }
}

fn button_bit(b: Button) -> u32 {
    match b {
        Button::Left => 1,
        Button::Right => 2,
        Button::Middle => 4,
        Button::X1 => 8,
        Button::X2 => 16,
    }
}

enum Msg {
    Hover(Option<usize>),
    Click(Zone),
    Release,
    Esc,
    Enter,
    Pick(usize),
    Rotate(i32),
    Quit,
}

// ── The session ───────────────────────────────────────────────────────────────

struct Active {
    tx: Sender<Msg>,
    profile: usize,
    held: Arc<Bits>,
    join: Option<JoinHandle<()>>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
/// The last `show` payload while the wheel is open, for a page that mounts after the event.
static SHOWN: Mutex<Option<Value>> = Mutex::new(None);

fn lock<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    PressOrHold,
    Press,
    Hold,
}

struct Wheel {
    cx: Ctx,
    title: String,
    root: Vec<Item>,
    path: Vec<usize>,
    hover: Option<usize>,
    sticky: bool,
    mode: Mode,
    hot: Arc<Hot>,
    dwell: Option<(usize, Instant)>,
}

impl Wheel {
    fn level(&self) -> &[Item] {
        let mut items = &self.root[..];
        for &p in &self.path {
            items = &items[p].items;
        }
        items
    }

    fn show(&self) {
        let title = match self.path.last() {
            None => self.title.clone(),
            Some(_) => {
                // The submenu's own name: the slice that was opened.
                let mut items = &self.root[..];
                let mut name = String::new();
                for &p in &self.path {
                    name = items[p].name.clone();
                    items = &items[p].items;
                }
                name
            }
        };
        // The song already playing: a subscriber only hears of changes.
        let payload = json!({ "title": title, "back": !self.path.is_empty(), "items": self.level(), "sticky": self.sticky, "np": media_event(svc_media::state().as_ref()) });
        *lock(&SHOWN) = Some(payload.clone());
        self.cx.emit(ID, "show", payload);
    }

    fn set_hover(&mut self, to: Option<usize>) {
        self.hover = to;
        self.dwell = to.filter(|i| self.level()[*i].kind == "submenu").map(|i| (i, Instant::now() + DWELL));
        self.cx.emit(ID, "hover", json!({ "index": to }));
    }

    fn enter(&mut self, i: usize) {
        self.path.push(i);
        self.sticky = true;
        self.hover = None;
        self.dwell = None;
        self.hot.rearm(self.level().len());
        self.show();
    }

    fn back(&mut self) -> bool {
        if self.path.pop().is_none() {
            return false;
        }
        self.hover = None;
        self.dwell = None;
        self.hot.rearm(self.level().len());
        self.show();
        true
    }

    /// A slice is chosen: a submenu opens, anything else ends the wheel with it.
    fn activate(&mut self, i: usize) -> Option<Option<Item>> {
        let item = self.level().get(i)?.clone();
        if item.kind == "submenu" {
            self.enter(i);
            None
        } else {
            Some(Some(item))
        }
    }

    /// `Some(pick)` ends the wheel.
    fn handle(&mut self, msg: Msg) -> Option<Option<Item>> {
        match msg {
            Msg::Quit => Some(None),
            Msg::Hover(to) => {
                self.set_hover(to);
                None
            }
            Msg::Click(Zone::Slice(i)) => self.activate(i),
            Msg::Click(Zone::Hub) | Msg::Esc => {
                if self.back() { None } else { Some(None) }
            }
            Msg::Click(Zone::Outside) => Some(None),
            Msg::Release => {
                if self.sticky || self.mode == Mode::Press {
                    return None;
                }
                match self.hover {
                    Some(i) => self.activate(i),
                    // Released in the middle: a tap leaves the wheel open to click; holding ends it.
                    None if self.mode == Mode::Hold => Some(None),
                    None => {
                        self.sticky = true;
                        None
                    }
                }
            }
            Msg::Enter => self.hover.and_then(|i| self.activate(i)),
            Msg::Pick(n) => {
                self.sticky = true;
                self.activate(n)
            }
            Msg::Rotate(step) => {
                let n = self.level().len() as i32;
                if n == 0 {
                    return None;
                }
                let from = self.hover.map_or(if step > 0 { -1 } else { 0 }, |i| i as i32);
                let to = (from + step).rem_euclid(n) as usize;
                if self.mode != Mode::Hold {
                    self.sticky = true;
                }
                self.hot.last.store(to, Relaxed);
                self.set_hover(Some(to));
                None
            }
        }
    }
}

fn mode_of(settings: &Value) -> Mode {
    match settings["mode"].as_str() {
        Some("press") => Mode::Press,
        Some("hold") => Mode::Hold,
        _ => Mode::PressOrHold,
    }
}

fn media_event(p: Option<&svc_media::Playback>) -> Value {
    match p {
        Some(p) => json!({ "title": p.title, "artist": p.artist, "playing": p.playing, "rev": p.art_rev, "player": p.source_name }),
        None => Value::Null,
    }
}

fn worker(cx: Ctx, rx: mpsc::Receiver<Msg>, mut wheel: Wheel, subs: (svc_input::Sub, svc_input::Sub), media: Option<svc_media::Sub>) {
    wheel.show();
    let pick = loop {
        let msg = match wheel.dwell {
            Some((i, at)) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    wheel.dwell = None;
                    if wheel.hover == Some(i) {
                        wheel.enter(i);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break None,
            },
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => break None,
            },
        };
        let quit = matches!(msg, Msg::Quit);
        if let Some(done) = wheel.handle(msg) {
            if quit {
                drop(subs);
                drop(media);
                cx.emit(ID, "close", json!({}));
                *lock(&SHOWN) = None;
                return;
            }
            break done;
        }
    };
    // The keyboard and the pointer are free on the first frame of the exit.
    drop(subs);
    cx.emit(ID, "close", json!({}));
    *lock(&SHOWN) = None;
    {
        let mut active = lock(&ACTIVE);
        if active.as_ref().is_some_and(|a| a.join.as_ref().is_some_and(|j| j.thread().id() == std::thread::current().id())) {
            // The handle stays with the slot's owner (`stop` joins it); only the slot is released.
            active.take();
        }
    }
    std::thread::sleep(EXIT);
    if lock(&ACTIVE).is_none() {
        if let Some(win) = cx.app().get_webview_window(LABEL) {
            let _ = win.hide();
        }
    }
    if let Some(item) = pick {
        if let Err(message) = run(&item) {
            cx.emit(ID, "failed", json!({ "message": message }));
        }
    }
    drop(media);
}

// ── Running a slice ───────────────────────────────────────────────────────────

/// `%NAME%` replaced by the environment's value.
fn expand(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 && std::env::var(&after[..end]).is_ok() => {
                out.push_str(&std::env::var(&after[..end]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A bare host gets https; a scheme (https:, mailto:, ms-settings:) is kept.
fn link(target: &str) -> String {
    let has_scheme = target.split_once(':').is_some_and(|(s, _)| s.len() > 1 && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)));
    if has_scheme { target.to_string() } else { format!("https://{target}") }
}

fn run(item: &Item) -> Result<(), String> {
    match item.kind.as_str() {
        "app" => svc_shell::open(&format!("shell:AppsFolder\\{}", item.target)),
        "file" => {
            let path = expand(&item.target);
            if !path.starts_with("shell:") && !std::path::Path::new(&path).exists() {
                return Err(format!("{path} was not found."));
            }
            svc_shell::open(&path)
        }
        "link" => svc_shell::open(&link(&item.target)),
        "media" => {
            match item.target.as_str() {
                "playPause" => svc_media::play_pause(),
                "next" => svc_media::next(),
                _ => svc_media::prev(),
            }
            Ok(())
        }
        "nowplaying" => {
            svc_media::play_pause();
            Ok(())
        }
        "shortcut" => svc_inject::send_keys(&item.target),
        // Only ever from the person's click on this slice; the command line is theirs, as typed in the editor.
        "command" => {
            use std::os::windows::process::CommandExt;
            std::process::Command::new("cmd")
                .arg("/C")
                .raw_arg(format!("\"{}\"", item.target))
                .creation_flags(0x0800_0000)
                .spawn()
                .map(|_| ())
                .map_err(|e| format!("{} did not start: {e}", item.target))
        }
        _ => Err("This slice cannot be run.".into()),
    }
}

// ── Opening ───────────────────────────────────────────────────────────────────

fn profiles(settings: &Value) -> Vec<(String, Vec<Item>)> {
    serde_json::from_value::<Vec<Profile>>(settings["profiles"].clone())
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.name.trim().chars().take(60).collect::<String>(), clean(p.items, 0)))
        .collect()
}

fn open_wheel(cx: &Ctx, index: usize, sticky: bool) -> Result<Value, String> {
    // A wheel is open: the same key again closes it (unless it is only the key repeating), another key switches.
    let previous = {
        let mut active = lock(&ACTIVE);
        if active.as_ref().is_some_and(|a| a.profile == index && a.held.any_down()) {
            return Ok(json!({}));
        }
        active.take()
    };
    if let Some(mut old) = previous {
        let _ = old.tx.send(Msg::Quit);
        if let Some(j) = old.join.take() {
            let _ = j.join();
        }
        if old.profile == index {
            if let Some(win) = cx.app().get_webview_window(LABEL) {
                let _ = win.hide();
            }
            return Ok(json!({}));
        }
    }

    let settings = cx.settings(ID);
    let all = profiles(&settings);
    let Some((name, items)) = all.into_iter().nth(index) else { return Ok(json!({})) };
    if items.is_empty() {
        cx.emit(ID, "failed", json!({ "message": "This wheel has no slices yet. Add some in Settings." }));
        return Ok(json!({}));
    }

    let pointer = cursor();
    let (area, scale) = screen(pointer);
    let centre = if settings["atPointer"].as_bool().unwrap_or(true) { geometry::centre(pointer, area, scale) } else { geometry::middle(area) };

    let hot = Arc::new(Hot {
        cx: AtomicI32::new(centre.0),
        cy: AtomicI32::new(centre.1),
        scale: AtomicU64::new(scale.to_bits()),
        n: AtomicUsize::new(items.len()),
        last: AtomicUsize::new(NONE),
        armed: std::sync::atomic::AtomicBool::new(false),
        ox: AtomicI32::new(pointer.0),
        oy: AtomicI32::new(pointer.1),
        swallowed: AtomicU32::new(0),
    });
    let held = Arc::new(Bits::default());
    let keys_swallowed = Arc::new(Bits::default());
    let (tx, rx) = mpsc::channel::<Msg>();

    let mouse = {
        let (hot, tx) = (hot.clone(), tx.clone());
        svc_input::mouse(5, false, move |m| {
            let (dx, dy) = hot.offset(m.x, m.y);
            let n = hot.n.load(Relaxed);
            match m.kind {
                MouseKind::Move => {
                    if !hot.armed.load(Relaxed) {
                        let s = hot.scale();
                        let moved = ((m.x - hot.ox.load(Relaxed)) as f64 / s).hypot((m.y - hot.oy.load(Relaxed)) as f64 / s);
                        if moved < geometry::WAKE {
                            return Verdict::Pass;
                        }
                        hot.armed.store(true, Relaxed);
                    }
                    let now = geometry::slice_at(dx, dy, n);
                    let code = now.unwrap_or(NONE);
                    if hot.last.swap(code, Relaxed) != code {
                        let _ = tx.send(Msg::Hover(now));
                    }
                    Verdict::Pass
                }
                MouseKind::Down(b) => {
                    hot.swallowed.fetch_or(button_bit(b), Relaxed);
                    let zone = if b == Button::Left { geometry::zone(dx, dy, n) } else { Zone::Outside };
                    let _ = tx.send(Msg::Click(zone));
                    Verdict::Swallow
                }
                MouseKind::Up(b) => {
                    if hot.swallowed.fetch_and(!button_bit(b), Relaxed) & button_bit(b) != 0 {
                        Verdict::Swallow
                    } else {
                        Verdict::Pass
                    }
                }
                _ => Verdict::Pass,
            }
        })
    };
    let keyboard = {
        let (held, swallowed, tx) = (held.clone(), keys_swallowed.clone(), tx.clone());
        svc_input::keyboard(5, false, move |k| {
            if !k.down {
                if held.has(k.vk) {
                    let _ = tx.send(Msg::Release);
                }
                return if swallowed.clear(k.vk) { Verdict::Swallow } else { Verdict::Pass };
            }
            // Keys of the shortcut still held (or repeating) are not for the wheel.
            if held.has(k.vk) {
                return Verdict::Pass;
            }
            let msg = match k.vk {
                0x1B => Msg::Esc,
                0x08 => Msg::Esc,
                0x0D => Msg::Enter,
                0x25 | 0x26 => Msg::Rotate(-1),
                0x27 | 0x28 => Msg::Rotate(1),
                v @ 0x31..=0x39 => Msg::Pick((v - 0x31) as usize),
                v @ 0x61..=0x69 => Msg::Pick((v - 0x61) as usize),
                _ => return Verdict::Pass,
            };
            swallowed.set(k.vk);
            let _ = tx.send(msg);
            Verdict::Swallow
        })
    };
    // After the hook: a key released between the two is simply not part of the chord.
    for vk in 8u16..=254 {
        if unsafe { GetAsyncKeyState(vk as i32) } < 0 {
            held.set(vk);
        }
    }
    let tap = !held.any_down();

    let media = wants_media(&items).then(|| {
        let cx = cx.clone();
        svc_media::subscribe(move |p| cx.emit(ID, "nowplaying", media_event(p)))
    });

    // The window: made on the first summons, then only moved, shown and hidden.
    place(cx, centre, scale)?;

    let mode = mode_of(&settings);
    let wheel = Wheel {
        cx: cx.clone(),
        title: name,
        root: items,
        path: Vec::new(),
        hover: None,
        sticky: sticky || tap || mode == Mode::Press,
        mode,
        hot,
        dwell: None,
    };
    let handle = {
        let cx = cx.clone();
        std::thread::Builder::new()
            .name("radial-menu".into())
            .spawn(move || worker(cx, rx, wheel, (mouse, keyboard), media))
            .map_err(|e| format!("The wheel could not start: {e}"))?
    };
    *lock(&ACTIVE) = Some(Active { tx, profile: index, held, join: Some(handle) });
    if let Some(win) = cx.app().get_webview_window(LABEL) {
        let _ = win.show();
    }
    Ok(json!({}))
}

/// Opens the window the first time, and puts it around `centre`.
fn place(cx: &Ctx, centre: (i32, i32), scale: f64) -> Result<(), String> {
    let win = match cx.app().get_webview_window(LABEL) {
        Some(win) => win,
        None => cx.window().open(WindowSpec {
            label: LABEL.into(),
            feature: ID.into(),
            page: "wheel".into(),
            w: geometry::SIZE,
            h: geometry::SIZE,
            frameless: true,
            topmost: true,
            no_activate: true,
            exclude_from_capture: false,
            transparent: true,
            monitor: Monitor::Cursor,
        })?,
    };
    let (origin, side) = geometry::window(centre, scale);
    // Click-through: the pointer and the keys stay with the hooks and the window in front.
    let _ = win.set_ignore_cursor_events(true);
    // Position, size, position: a move across displays of another scale may resize once.
    let _ = win.set_position(PhysicalPosition::new(origin.0, origin.1));
    let _ = win.set_size(PhysicalSize::new(side, side));
    let _ = win.set_position(PhysicalPosition::new(origin.0, origin.1));
    Ok(())
}

// ── The feature ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct RadialMenu;

fn data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
}

impl Feature for RadialMenu {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    // Nothing runs until a shortcut is pressed.
    fn start(&self, _cx: &Ctx) -> Result<(), String> {
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        let taken = lock(&ACTIVE).take();
        if let Some(mut a) = taken {
            let _ = a.tx.send(Msg::Quit);
            if let Some(j) = a.join.take() {
                let _ = j.join();
            }
        }
        *lock(&SHOWN) = None;
        cx.window().close(LABEL);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        match method {
            "shortcut" => {
                let index = match args["id"].as_str() {
                    Some("wheel2") => 1,
                    Some("wheel3") => 2,
                    _ => 0,
                };
                open_wheel(cx, index, false)
            }
            // "Try it" in Settings: stays open for clicking.
            "try" => open_wheel(cx, args["profile"].as_u64().unwrap_or(0) as usize, true),
            // For the page that mounted after the wheel was shown.
            "session" => {
                let shown = lock(&SHOWN).clone();
                Ok(shown.map_or(Value::Null, |mut p| {
                    p["np"] = media_event(svc_media::state().as_ref());
                    p
                }))
            }
            "apps" => {
                let mut apps = svc_windows::apps();
                apps.sort_by_key(|a| a.name.to_lowercase());
                apps.dedup_by(|a, b| a.id == b.id);
                Ok(json!(apps.into_iter().map(|a| json!({ "id": a.id, "name": a.name })).collect::<Vec<_>>()))
            }
            "icon" => {
                let target = args["target"].as_str().unwrap_or_default();
                let target = if args["kind"] == "file" { expand(target) } else { target.to_string() };
                Ok(svc_shell::icon(&target, 64).map(|png| json!(data_url(&png))).unwrap_or(Value::Null))
            }
            "art" => {
                let rev = args["rev"].as_u64().unwrap_or(0);
                Ok(svc_media::art(rev).map(|(bytes, mime)| json!(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes.as_slice())))).unwrap_or(Value::Null))
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/radial-menu/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(RadialMenu)
}
