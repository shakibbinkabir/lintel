// Capture — the screen, taken. A selector over a frozen picture of every
// display (a region, a window, the whole display), a colour picker, text and
// QR codes read off the screen, a ruler, captures pinned above other windows,
// and the last twelve captures kept for the island.
//
// The contract is docs/ARCHITECTURE.md §2: nothing runs before `start`,
// everything is gone after `stop`, every command arrives through `call`. At
// rest there is nothing at all: no hook, timer, thread or window exists until
// a shortcut or a tile starts a session, and the session's windows are
// destroyed when it ends.
//
// Pixels are svc-capture's and physical from end to end: the page selects in
// pixels of the frozen picture and exactly those are cut out here. Share and
// drag-out are svc-shell's, the clipboard and the keyboard hand-over svc-inject's.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use notch_core::{log, storage, win, Ctx, Feature, Group, Manifest, Monitor as Where, ShortcutDef, Use, WindowSpec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use svc_capture::{Image, Monitor, Rect, Win};
use svc_inject::{clip, ForegroundGuard};
use svc_shell::{DropEffect, Item};
use tauri::{Manager, PhysicalPosition, PhysicalSize};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetSystemMetrics, SetCursorPos, SM_SWAPBUTTON};

const ID: &str = "capture";

/// What the selector can do; the first is what an unknown name means. "record"
/// is the recorder's: the page offers it only while that feature runs.
const TOOLS: [&str; 5] = ["shot", "color", "text", "ruler", "record"];

/// How many captures the island keeps.
const KEPT: usize = 12;

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Capture",
    group: Group::Capture,
    default_on: true,
    uses: &[Use::Capture, Use::Clipboard],
    shortcuts: &[
        ShortcutDef { id: "shot", label: "Take a screenshot", default: Some("Ctrl+Alt+Shift+4") },
        ShortcutDef { id: "full", label: "Capture the whole screen", default: Some("Ctrl+Alt+Shift+3") },
        ShortcutDef { id: "color", label: "Pick a colour from the screen", default: None },
        ShortcutDef { id: "text", label: "Copy text from the screen", default: None },
        ShortcutDef { id: "ruler", label: "Measure on the screen", default: None },
    ],
    tray: &[],
    defaults: r#"{ "copy": true, "save": true, "folder": "", "loupe": true, "colorFormat": "hex" }"#,
};

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn cursor() -> (i32, i32) {
    let mut at = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut at);
    }
    (at.x, at.y)
}

fn copy_text(text: &str) -> Result<(), String> {
    clip::open()?.empty_then(|clipboard| clipboard.set_text(text))
}

/// Windows can read text here: asked once, it starts a recogniser to find out.
fn reads_text() -> bool {
    static CAN: OnceLock<bool> = OnceLock::new();
    *CAN.get_or_init(svc_capture::ocr_available)
}

/// The only kind of code the app offers to open: a plain web address.
fn web_link(text: &str) -> bool {
    let rest = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")).unwrap_or_default();
    !rest.is_empty() && !rest.starts_with('/') && !text.contains(|c: char| c.is_whitespace() || c.is_control() || c == '"')
}

/// One colour all over: a window that drew nothing into its own buffer.
fn blank(image: &Image) -> bool {
    let first = image.pixel(0, 0);
    (0..image.w * image.h).step_by(997).all(|i| image.pixel(i % image.w, i / image.w) == first)
}

fn rect_arg(value: &Value) -> Option<Rect> {
    let n = |key: &str| value[key].as_f64().map(|v| v.round() as i32);
    Some(Rect { x: n("x")?, y: n("y")?, w: n("w")?, h: n("h")? })
}

// ── The captures that are kept ────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct Shot {
    id: String,
    /// The file's name in the captures folder.
    name: String,
    w: u32,
    h: u32,
    /// Where on the desktop it was taken, in physical pixels: a pin opens there.
    x: i32,
    y: i32,
    /// Seconds since 1970.
    at: u64,
    /// The copy "Save" made in the user's folder.
    #[serde(default)]
    saved: Option<String>,
}

/// One writer of captures.json at a time.
static WRITING: Mutex<()> = Mutex::new(());

fn store(cx: &Ctx) -> PathBuf {
    cx.data_dir(ID).join("captures.json")
}

fn kept_dir(cx: &Ctx) -> PathBuf {
    let dir = cx.data_dir(ID).join("captures");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn load(cx: &Ctx) -> Vec<Shot> {
    storage::json_load(&store(cx))
}

/// Loads, changes, saves; the pages hear `capture:changed` when something did change.
fn edit<T>(cx: &Ctx, change: impl FnOnce(&mut Vec<Shot>) -> T) -> T {
    let _one = locked(&WRITING);
    let mut shots = load(cx);
    let before = shots.clone();
    let out = change(&mut shots);
    if shots != before {
        if let Err(err) = storage::json_save_atomic(&store(cx), &shots) {
            log::line(format!("capture: cannot save the list: {err}"));
        }
        cx.emit(ID, "changed", ());
    }
    out
}

fn one(cx: &Ctx, args: &Value) -> Result<(Shot, PathBuf), String> {
    let id = args["id"].as_str().unwrap_or_default();
    let shot = load(cx).into_iter().find(|s| s.id == id).ok_or("That capture is not kept any more.")?;
    let path = kept_dir(cx).join(&shot.name);
    if path.is_file() { Ok((shot, path)) } else { Err("That capture is not kept any more.".into()) }
}

fn new_id() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("{now:x}-{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// "name.png" in `dir`, or "name (2).png"… when that is taken.
fn free_png(dir: &Path, name: &str) -> PathBuf {
    (1u32..)
        .map(|n| dir.join(if n == 1 { format!("{name}.png") } else { format!("{name} ({n}).png") }))
        .find(|path| !path.exists())
        .expect("a free name")
}

/// Where "Save" puts a capture: the folder of the settings, or Pictures\Screenshots.
fn folder(cx: &Ctx) -> PathBuf {
    let chosen = PathBuf::from(cx.settings(ID)["folder"].as_str().unwrap_or_default().trim());
    if chosen.is_absolute() {
        return chosen;
    }
    win::picture_dirs().into_iter().find(|dir| dir.is_dir()).unwrap_or_else(win::home_dir).join("Screenshots")
}

/// A copy of a kept capture in the user's folder, under the same name.
fn save_copy(cx: &Ctx, file: &Path) -> Result<String, String> {
    let dir = folder(cx);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} cannot be used: {e}", dir.display()))?;
    let out = free_png(&dir, &file.file_stem().unwrap_or_default().to_string_lossy());
    std::fs::copy(file, &out).map_err(|e| format!("The capture could not be saved: {e}"))?;
    Ok(out.to_string_lossy().into_owned())
}

/// A new capture: on the clipboard, in the list and in the user's folder, as the
/// settings say. `at` is where on the desktop it was taken.
fn keep(cx: &Ctx, image: &Image, at: (i32, i32)) -> Result<Value, String> {
    let png = svc_capture::encode_png(image)?;
    let settings = cx.settings(ID);
    let copied = settings["copy"].as_bool() != Some(false)
        && svc_capture::clipboard_image(image, &png).map_err(|err| log::line(format!("capture: not copied: {err}"))).is_ok();
    let t = win::local_time();
    let name = format!("Screenshot {:04}-{:02}-{:02} at {:02}.{:02}.{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
    let file = free_png(&kept_dir(cx), &name);
    std::fs::write(&file, &png).map_err(|e| format!("The capture could not be kept: {e}"))?;
    let mut shot = Shot {
        id: new_id(),
        name: file.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        w: image.w,
        h: image.h,
        x: at.0,
        y: at.1,
        at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        saved: None,
    };
    if settings["save"].as_bool() != Some(false) {
        shot.saved = save_copy(cx, &file).map_err(|err| log::line(format!("capture: {err}"))).ok();
    }
    let dir = kept_dir(cx);
    edit(cx, |shots| {
        shots.insert(0, shot.clone());
        // The oldest leave with their files: these are the app's own copies.
        for old in shots.drain(KEPT.min(shots.len())..) {
            let _ = std::fs::remove_file(dir.join(old.name));
        }
    });
    cx.emit(ID, "captured", json!({ "shot": shot, "copied": copied }));
    Ok(Value::Null)
}

// ── A session of the selector ─────────────────────────────────────────────────

/// One display, as it was when the session began, and the window that shows it.
struct Screen {
    label: String,
    monitor: Monitor,
    frame: Image,
}

struct Session {
    /// Which session this is, for the watchdog that began with it.
    id: u32,
    /// An overlay's page has asked for its picture: the selector is up.
    asked: bool,
    tool: &'static str,
    /// The window that had the keyboard: it gets it back.
    front: ForegroundGuard,
    screens: Vec<Screen>,
    /// Front to back, as they were.
    windows: Vec<Win>,
}

struct Pin {
    label: String,
    shot: String,
    /// Clicks go through it.
    through: bool,
}

/// The session that is up (one at a time) and the pins that float. Both are
/// empty at rest and emptied by `stop`.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static PINS: Mutex<Vec<Pin>> = Mutex::new(Vec::new());

#[derive(Default)]
pub struct Capture;

/// Ends the session that is up: the keyboard goes back to the window that had
/// it, then the overlays go. None when there was no session.
fn end(cx: &Ctx) -> Option<Session> {
    let session = locked(&SESSION).take()?;
    if session.front.hwnd() != 0 {
        if let Err(err) = session.front.restore() {
            log::line(format!("capture: the keyboard did not go back: {err}"));
        }
    }
    for screen in &session.screens {
        cx.window().close(&screen.label);
    }
    Some(session)
}

/// Every display, photographed now. From the island's own tiles the island is
/// hosting the capture controls: it is left out of the picture and put back as it was.
fn frozen(cx: &Ctx, hide_island: bool) -> Vec<Screen> {
    let island = cx
        .app()
        .get_webview_window("island")
        .filter(|_| hide_island)
        .and_then(|window| window.hwnd().ok())
        .map(|hwnd| hwnd.0 as isize)
        .filter(|hwnd| !svc_capture::excluded_from_capture(*hwnd));
    if let Some(hwnd) = island {
        svc_capture::exclude_from_capture(hwnd, true);
        // Until Windows has drawn the screen without it.
        std::thread::sleep(Duration::from_millis(80));
    }
    let screens = svc_capture::monitors()
        .into_iter()
        .enumerate()
        .filter_map(|(i, monitor)| Some(Screen { label: format!("capture-sel-{i}"), frame: svc_capture::grab(monitor.rect).ok()?, monitor }))
        .collect();
    if let Some(hwnd) = island {
        svc_capture::exclude_from_capture(hwnd, false);
    }
    screens
}

impl Capture {
    /// Freezes the screen and puts the selector over it, with the keyboard.
    fn begin(&self, cx: &Ctx, tool: &str, from_island: bool) -> Result<Value, String> {
        // The shortcut again: the session that is up ends.
        if end(cx).is_some() {
            return Ok(Value::Null);
        }
        let tool = TOOLS.iter().copied().find(|t| *t == tool).unwrap_or(TOOLS[0]);
        let front = ForegroundGuard::capture();
        let screens = frozen(cx, from_island);
        if screens.is_empty() {
            return Err("The screen could not be captured.".into());
        }
        static NEXT: AtomicU32 = AtomicU32::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let places: Vec<(String, Rect)> = screens.iter().map(|s| (s.label.clone(), s.monitor.rect)).collect();
        *locked(&SESSION) = Some(Session { id, asked: false, tool, front, screens, windows: svc_capture::windows() });
        // An overlay covers a whole display and takes every click: should its page
        // never come up, nobody is left behind it. One short-lived thread per session.
        let watch = cx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(6));
            if locked(&SESSION).as_ref().is_some_and(|s| s.id == id && !s.asked) {
                log::line("capture: the selector did not come up; the session is ended");
                end(&watch);
            }
        });

        let pointer = cursor();
        let mut keyboard: Option<tauri::WebviewWindow> = None;
        for (label, rect) in places {
            let spec = WindowSpec {
                label,
                feature: ID.into(),
                page: "selector".into(),
                w: 320.0,
                h: 200.0,
                frameless: true,
                topmost: true,
                no_activate: false,
                exclude_from_capture: true,
                // Nothing of it shows until the page has drawn the frozen picture.
                transparent: true,
                monitor: Where::Primary,
            };
            match cx.window().open(spec) {
                Ok(window) => {
                    let _ = window.set_skip_taskbar(true);
                    let _ = window.set_resizable(false);
                    // Exactly the display, in physical pixels: one pixel of the picture is one of the screen.
                    let _ = window.set_position(PhysicalPosition::new(rect.x, rect.y));
                    let _ = window.set_size(PhysicalSize::new(rect.w as u32, rect.h as u32));
                    if keyboard.is_none() || rect.contains(pointer.0, pointer.1) {
                        keyboard = Some(window);
                    }
                }
                Err(err) => log::line(format!("capture: {err}")),
            }
        }
        let Some(hwnd) = keyboard.as_ref().and_then(|w| w.hwnd().ok()).map(|h| h.0 as isize) else {
            end(cx);
            return Err("The selector could not be opened.".into());
        };
        // The overlay under the pointer takes the keyboard (Esc, Enter, the tool
        // keys); `end` gives it back. Windows may refuse, so the answer is
        // checked: without it the pointer still does everything, and a right click cancels.
        match ForegroundGuard::of(hwnd).restore() {
            // In front: the keys go on to the page inside the window.
            Ok(()) => {
                let _ = keyboard.map(|w| w.set_focus());
            }
            Err(err) => log::line(format!("capture: the selector did not get the keyboard: {err}")),
        }
        Ok(Value::Null)
    }

    /// The page made its choice: the session ends and the tool does its work.
    fn finish(&self, cx: &Ctx, args: &Value) -> Result<Value, String> {
        let session = end(cx).ok_or("No capture is in progress.")?;
        let label = args["label"].as_str().unwrap_or_default();
        let screen = session.screens.iter().find(|s| s.label == label).ok_or("That display is not part of the capture.")?;
        let on = screen.monitor.rect;
        let whole = Rect { x: 0, y: 0, w: on.w, h: on.h };
        // A rectangle in pixels of the frozen picture; none is the whole display.
        let area = rect_arg(&args["rect"]).map(|r| r.clip(whole)).filter(|r| r.w >= 2 && r.h >= 2).unwrap_or(whole);
        match args["tool"].as_str().unwrap_or(session.tool) {
            // The page read the pixel from the same bytes and wrote it in the user's format.
            "color" => {
                let text = args["text"].as_str().unwrap_or_default();
                copy_text(text)?;
                cx.emit(ID, "result", json!({ "kind": "color", "text": text, "hex": args["hex"] }));
                Ok(Value::Null)
            }
            "text" => {
                read(cx, &screen.frame.crop(area));
                Ok(Value::Null)
            }
            // The recorder films what was pointed at: a window, or the area in pixels of
            // the desktop. A feature cannot call another one, so the island's page hears
            // this and asks the recorder through the registry (`recorder` `start`).
            "record" => {
                let window = args["window"].as_i64().filter(|hwnd| session.windows.iter().any(|w| w.hwnd as i64 == *hwnd));
                let target = match window {
                    Some(hwnd) => json!({ "what": "window", "hwnd": hwnd }),
                    None => json!({ "what": "region", "rect": { "x": on.x + area.x, "y": on.y + area.y, "w": area.w, "h": area.h } }),
                };
                cx.emit(ID, "record", target);
                Ok(Value::Null)
            }
            _ => {
                let window = args["window"].as_i64().and_then(|hwnd| session.windows.iter().find(|w| w.hwnd as i64 == hwnd));
                match window {
                    // From its own buffer, so nothing that covers it is in the picture; some
                    // programs draw nothing that way, and then it is cut from the frozen screen.
                    Some(window) => {
                        let seen = window.rect.clip(on);
                        let image = svc_capture::grab_window(window.hwnd)
                            .ok()
                            .filter(|image| !blank(image))
                            .unwrap_or_else(|| screen.frame.crop(Rect { x: seen.x - on.x, y: seen.y - on.y, ..seen }));
                        keep(cx, &image, (window.rect.x, window.rect.y))
                    }
                    None => keep(cx, &screen.frame.crop(area), (on.x + area.x, on.y + area.y)),
                }
            }
        }
    }

    /// The display under the pointer, at once and without the selector.
    fn full(&self, cx: &Ctx, from_island: bool) -> Result<Value, String> {
        let pointer = cursor();
        let screens = frozen(cx, from_island);
        let screen = screens
            .iter()
            .find(|s| s.monitor.rect.contains(pointer.0, pointer.1))
            .or(screens.first())
            .ok_or("The screen could not be captured.")?;
        keep(cx, &screen.frame, (screen.monitor.rect.x, screen.monitor.rect.y))
    }

    // ── Pins ──────────────────────────────────────────────────────────────────

    fn pins_now(&self) -> Value {
        let pins = locked(&PINS);
        json!({ "count": pins.len(), "through": pins.iter().filter(|p| p.through).count() })
    }

    /// A kept capture in a window of its own, above everything.
    fn pin(&self, cx: &Ctx, shot: &Shot) -> Result<Value, String> {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        let label = format!("capture-pin-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let monitors = svc_capture::monitors();
        let m = monitors.iter().find(|m| m.rect.contains(shot.x, shot.y)).or(monitors.first()).ok_or("There is no display to pin it on.")?;
        // Pixel for pixel where it was taken while that leaves the screen usable
        // (55 % of its short side); a larger one is scaled down and centred.
        let k = (0.55 * m.rect.w.min(m.rect.h) as f64 / shot.w.max(shot.h).max(1) as f64).min(1.0);
        let (w, h) = (((shot.w as f64 * k).round() as i32).max(1), ((shot.h as f64 * k).round() as i32).max(1));
        let (x, y) = if k < 1.0 { (m.rect.x + (m.rect.w - w) / 2, m.rect.y + (m.rect.h - h) / 2) } else { (shot.x, shot.y) };
        let window = cx.window().open(WindowSpec {
            label: label.clone(),
            feature: ID.into(),
            page: "pin".into(),
            w: w as f64 / m.scale,
            h: h as f64 / m.scale,
            frameless: true,
            topmost: true,
            no_activate: true,
            // Like the rest of the capture chrome: a pin is never in the next capture.
            exclude_from_capture: true,
            transparent: true,
            monitor: Where::Primary,
        })?;
        let _ = window.set_resizable(false);
        let _ = window.set_size(PhysicalSize::new(w as u32, h as u32));
        let _ = window.set_position(PhysicalPosition::new(x, y));
        locked(&PINS).push(Pin { label, shot: shot.id.clone(), through: false });
        cx.emit(ID, "pins", self.pins_now());
        Ok(Value::Null)
    }

    /// Closes one pin, or all of them without a label.
    fn unpin(&self, cx: &Ctx, label: Option<&str>) {
        let gone: Vec<Pin> = {
            let mut pins = locked(&PINS);
            let (gone, stay) = std::mem::take(&mut *pins).into_iter().partition(|p| label.is_none_or(|l| l == p.label));
            *pins = stay;
            gone
        };
        for pin in &gone {
            cx.window().close(&pin.label);
        }
        if !gone.is_empty() {
            cx.emit(ID, "pins", self.pins_now());
        }
    }

    /// The window of a pin the page named: only labels this feature opened.
    fn pin_window(&self, cx: &Ctx, args: &Value) -> Result<(String, tauri::WebviewWindow), String> {
        let label = args["label"].as_str().unwrap_or_default();
        if !locked(&PINS).iter().any(|p| p.label == label) {
            return Err("That pin is closed.".into());
        }
        let window = cx.app().get_webview_window(label).ok_or("That pin is closed.")?;
        Ok((label.to_string(), window))
    }
}

/// The text tool: a QR code if there is one, else the text Windows reads. A
/// code's content is only shown; nothing is opened or copied without a click.
fn read(cx: &Ctx, piece: &Image) {
    let codes = svc_capture::qr(piece);
    if !codes.is_empty() {
        let text = codes.join("\n");
        cx.emit(ID, "result", json!({ "kind": "qr", "link": codes.len() == 1 && web_link(&text), "text": text }));
        return;
    }
    let lines = if reads_text() { svc_capture::ocr(piece).map_err(|err| log::line(format!("capture: text recognition: {err}"))).unwrap_or_default() } else { Vec::new() };
    let text = lines.iter().map(|l| l.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
    if text.is_empty() || copy_text(&text).is_err() {
        cx.emit(ID, "result", json!({ "kind": "none" }));
    } else {
        cx.emit(ID, "result", json!({ "kind": "text", "text": text }));
    }
}

impl Feature for Capture {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        // A picture the list no longer knows (the app closed in between) goes too.
        let kept = load(cx);
        for file in std::fs::read_dir(kept_dir(cx)).into_iter().flatten().flatten() {
            if !kept.iter().any(|s| file.file_name() == *s.name) {
                let _ = std::fs::remove_file(file.path());
            }
        }
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        end(cx);
        self.unpin(cx, None);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        match method {
            // ── The selector ──────────────────────────────────────────────────
            "shortcut" | "begin" => match args["id"].as_str().or(args["tool"].as_str()).unwrap_or_default() {
                "full" => self.full(cx, method == "begin"),
                tool => self.begin(cx, tool, method == "begin"),
            },
            // An overlay asks for its display: the picture as it is in memory
            // (RGBA, top row first), and the windows a click could mean.
            "frame" => {
                let mut session = locked(&SESSION);
                let session = session.as_mut().ok_or("No capture is in progress.")?;
                session.asked = true;
                let screen = session.screens.iter().find(|s| args["label"] == s.label.as_str()).ok_or("That display is not part of the capture.")?;
                let on = screen.monitor.rect;
                let windows: Vec<Value> = session
                    .windows
                    .iter()
                    .map(|w| (w.hwnd, w.rect.clip(on)))
                    .filter(|(_, r)| r.w > 0 && r.h > 0)
                    .map(|(hwnd, r)| json!({ "id": hwnd, "x": r.x - on.x, "y": r.y - on.y, "w": r.w, "h": r.h }))
                    .collect();
                Ok(json!({
                    "tool": session.tool,
                    "w": screen.frame.w,
                    "h": screen.frame.h,
                    "rgba": base64(&screen.frame.rgba),
                    "windows": windows,
                    "ocr": reads_text(),
                    "settings": cx.settings(ID),
                }))
            }
            // The tool was switched on one display: the others follow.
            "tool" => {
                let tool = TOOLS.iter().copied().find(|t| args["tool"] == *t).ok_or("unknown tool")?;
                if let Some(session) = locked(&SESSION).as_mut() {
                    session.tool = tool;
                }
                cx.emit(ID, "tool", tool);
                Ok(Value::Null)
            }
            // An arrow key: the pointer moves by whole pixels of the screen.
            "nudge" => {
                if locked(&SESSION).is_some() {
                    let (x, y) = cursor();
                    let by = |key: &str| args[key].as_i64().unwrap_or(0).clamp(-100, 100) as i32;
                    unsafe {
                        let _ = SetCursorPos(x + by("dx"), y + by("dy"));
                    }
                }
                Ok(Value::Null)
            }
            "finish" => self.finish(cx, &args),
            "cancel" => {
                end(cx);
                Ok(Value::Null)
            }
            // A colour read with C, a measured size, a code's content: on the clipboard, by a click or a key.
            "copy_text" => {
                let text = args["text"].as_str().unwrap_or_default();
                if text.is_empty() {
                    return Err("There was nothing to copy.".into());
                }
                copy_text(text)?;
                Ok(Value::Null)
            }
            "caps" => Ok(json!({ "ocr": reads_text(), "folder": folder(cx).to_string_lossy() })),

            // ── The captures that are kept ────────────────────────────────────
            "list" => {
                let dir = kept_dir(cx);
                // A capture that was dragged away (moved) leaves the list.
                Ok(json!(edit(cx, |shots| {
                    shots.retain(|s| dir.join(&s.name).is_file());
                    shots.clone()
                })))
            }
            "thumb" => {
                let (_, path) = one(cx, &args)?;
                // Explorer's own preview of the PNG; the PNG itself when there is none.
                let png = svc_shell::thumbnail(&path, 256).or_else(|_| std::fs::read(&path).map_err(|e| e.to_string()))?;
                Ok(json!({ "url": format!("data:image/png;base64,{}", base64(&png)) }))
            }
            // The text Windows reads in the whole capture, to the clipboard; "" when there is none.
            "copy_ocr" => {
                let png = std::fs::read(one(cx, &args)?.1).map_err(|e| e.to_string())?;
                let lines = svc_capture::ocr(&svc_capture::decode_png(&png)?)?;
                let text = lines.iter().map(|l| l.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("
");
                if !text.is_empty() {
                    copy_text(&text)?;
                }
                Ok(json!(text.chars().count()))
            }
            "copy" => {
                let (_, path) = one(cx, &args)?;
                let png = std::fs::read(&path).map_err(|e| e.to_string())?;
                svc_capture::clipboard_image(&svc_capture::decode_png(&png)?, &png)?;
                Ok(Value::Null)
            }
            // A copy in the user's folder; where it went is the answer.
            "save" => {
                let (shot, path) = one(cx, &args)?;
                let saved = match shot.saved.filter(|p| Path::new(p).is_file()) {
                    Some(there) => there,
                    None => save_copy(cx, &path)?,
                };
                edit(cx, |shots| {
                    if let Some(s) = shots.iter_mut().find(|s| s.id == shot.id) {
                        s.saved = Some(saved.clone());
                    }
                });
                Ok(json!(saved))
            }
            // The saved copy in its folder when there is one.
            "reveal" => {
                let (shot, path) = one(cx, &args)?;
                let saved = shot.saved.map(PathBuf::from).filter(|p| p.is_file());
                svc_shell::reveal(saved.as_deref().unwrap_or(&path))?;
                Ok(Value::Null)
            }
            "open" => {
                svc_shell::open(&one(cx, &args)?.1.to_string_lossy())?;
                Ok(Value::Null)
            }
            "remove" | "clear" => {
                let id = args["id"].as_str().unwrap_or_default();
                let dir = kept_dir(cx);
                edit(cx, |shots| {
                    shots.retain(|s| {
                        let go = method == "clear" || s.id == id;
                        // The app's own copy; one saved to the user's folder stays there.
                        if go {
                            let _ = std::fs::remove_file(dir.join(&s.name));
                        }
                        !go
                    });
                });
                Ok(Value::Null)
            }
            "share" => {
                let items = [Item::File(one(cx, &args)?.1)];
                // The sheet gets a window of its own under the island (svc_shell::share_sheet says why).
                let package = svc_shell::share_package(&items).map_err(|e| e.message())?;
                let island = cx.app().get_webview_window("island").ok_or("The island is not there.")?;
                let (at, size) = (island.outer_position().map_err(|e| e.to_string())?, island.outer_size().map_err(|e| e.to_string())?);
                let under = (at.x + size.width as i32 / 2, at.y + (244.0 * island.scale_factor().unwrap_or(1.0)) as i32);
                let (done, shown) = mpsc::channel();
                cx.app()
                    .run_on_main_thread(move || {
                        let _ = done.send(svc_shell::share_sheet(&package, under));
                    })
                    .map_err(|e| e.to_string())?;
                shown.recv().map_err(|e| e.to_string())??;
                Ok(Value::Null)
            }
            // An OLE drag out of the island, on the UI thread, until the button is released.
            "drag" => {
                // A pin's file when the press was on a pin (`label`), else the capture by `id`.
                let pinned = args["label"].as_str().and_then(|l| locked(&PINS).iter().find(|p| p.label == l).map(|p| p.shot.clone()));
                let items = [Item::File(one(cx, &pinned.map_or(args.clone(), |id| json!({ "id": id })))?.1)];
                let (done, effect) = mpsc::channel();
                cx.app()
                    .run_on_main_thread(move || {
                        let _ = done.send(svc_shell::drag_out(&items));
                    })
                    .map_err(|e| e.to_string())?;
                Ok(json!(effect.recv().unwrap_or(DropEffect::None) != DropEffect::None))
            }

            // ── Pins ──────────────────────────────────────────────────────────
            "pin" => self.pin(cx, &one(cx, &args)?.0),
            "pins" => Ok(self.pins_now()),
            // A pin's page asks for its picture.
            "pin_image" => {
                let label = args["label"].as_str().unwrap_or_default();
                let shot = locked(&PINS).iter().find(|p| p.label == label).map(|p| p.shot.clone()).ok_or("That pin is closed.")?;
                let png = std::fs::read(one(cx, &json!({ "id": shot }))?.1).map_err(|e| e.to_string())?;
                Ok(json!({ "url": format!("data:image/png;base64,{}", base64(&png)) }))
            }
            // A pin is dragged: it follows the pointer until the button is let go.
            // A loop of its own because a window that never takes the keyboard is
            // not moved by Windows while another program is in front.
            "pin_move" => {
                let (_, window) = self.pin_window(cx, &args)?;
                let (from, origin) = (cursor(), window.outer_position().map_err(|e| e.to_string())?);
                let button = if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 { VK_RBUTTON } else { VK_LBUTTON };
                let until = Instant::now() + Duration::from_secs(60);
                while unsafe { GetAsyncKeyState(button.0 as i32) } < 0 && Instant::now() < until {
                    let now = cursor();
                    let _ = window.set_position(PhysicalPosition::new(origin.x + now.0 - from.0, origin.y + now.1 - from.1));
                    std::thread::sleep(Duration::from_millis(8));
                }
                Ok(Value::Null)
            }
            // Clicks go through one pin, or reach all of them again.
            "pin_through" | "pins_clickable" => {
                let through = method == "pin_through";
                let only = if through { Some(self.pin_window(cx, &args)?.0) } else { None };
                for pin in locked(&PINS).iter_mut().filter(|p| only.as_ref().is_none_or(|l| *l == p.label)) {
                    if let Some(window) = cx.app().get_webview_window(&pin.label) {
                        let _ = window.set_ignore_cursor_events(through);
                    }
                    pin.through = through;
                }
                cx.emit(ID, "pins", self.pins_now());
                Ok(Value::Null)
            }
            // The wheel over a pin: it grows or shrinks about its centre.
            "pin_zoom" => {
                let (_, window) = self.pin_window(cx, &args)?;
                let k = args["k"].as_f64().unwrap_or(1.0).clamp(0.5, 2.0);
                let (size, at) = (window.outer_size().map_err(|e| e.to_string())?, window.outer_position().map_err(|e| e.to_string())?);
                let (w, h) = (((size.width as f64 * k).round() as u32).clamp(60, 8000), ((size.height as f64 * k).round() as u32).clamp(60, 8000));
                let _ = window.set_size(PhysicalSize::new(w, h));
                let _ = window.set_position(PhysicalPosition::new(at.x + (size.width as i32 - w as i32) / 2, at.y + (size.height as i32 - h as i32) / 2));
                Ok(Value::Null)
            }
            "pin_close" => {
                let (label, _) = self.pin_window(cx, &args)?;
                self.unpin(cx, Some(&label));
                Ok(Value::Null)
            }
            "pins_close" => {
                self.unpin(cx, None);
                Ok(Value::Null)
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/capture/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(Capture)
}
