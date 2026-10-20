// Command bar — a window that opens over the app in front on a shortcut, finds
// apps, open windows, commands, settings, emoji and answers for what is typed,
// runs the row on Enter and gives the keyboard back to the window it took it from.
//
// The contract is docs/ARCHITECTURE.md §2: nothing runs before `start`,
// everything is gone after `stop`, every command arrives through `call`. With
// the bar closed nothing runs but the registered shortcut: the window is hidden
// (kept, so that it opens at once), no timer, no hook, no thread. Nothing is
// read from other apps while it is closed, and what is typed is never written
// anywhere.

pub mod dates;
pub mod emoji;
pub mod math;
pub mod rows;
pub mod search;
pub mod selection;
pub mod units;

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine;
use notch_core::{log, storage, Ctx, Feature, Group, Manifest, Monitor, ShortcutDef, Use, WindowSpec};
use rows::{Catalog, Query};
use search::Learned;
use serde_json::{json, Value};
use svc_inject::clip;
use tauri::{Manager, PhysicalPosition, WebviewWindow, WindowEvent};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::Shell::SHEmptyRecycleBinW;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, PostMessageW, WM_COMMAND};

const ID: &str = "command-bar";
/// The window's label and its page.
const LABEL: &str = "command-bar";
/// The window, in logical pixels: the panel with room around it for its shadow.
/// The page draws nothing else there, and a click on the empty part closes the bar.
const SIZE: (f64, f64) = (720.0, 540.0);

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Command bar",
    group: Group::Tools,
    // Its shortcut is taken from every other program while it is on: the user turns it on.
    default_on: false,
    // "Empty the recycle bin", after a confirmation in the bar.
    uses: &[Use::FileDelete],
    shortcuts: &[ShortcutDef { id: "toggle", label: "Open the command bar", default: Some("Ctrl+Space") }],
    tray: &[],
    // links: `name = address or folder`, one per line. fileFolders: folders to look for files in, one per line.
    // selectionBar: a small bar beside text selected in any app (off: it watches the mouse while on).
    defaults: r#"{ "links": "", "fileFolders": "", "selectionBar": false }"#,
};

/// The bar is on screen: who had the keyboard before it, and the bar's own window.
struct Open {
    before: svc_windows::Before,
    hwnd: isize,
}

#[derive(Default)]
struct State {
    open: Option<Open>,
    cat: Catalog,
    learned: Learned,
    /// Where `learned` is kept; None while the feature is off.
    file: Option<PathBuf>,
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn same_file(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// One more run of a row, or a change to the pins: kept on disk, ids and counts only.
fn learn(state: &Shared, change: impl FnOnce(&mut Learned)) {
    let mut s = lock(state);
    change(&mut s.learned);
    if let Some(file) = &s.file {
        if let Err(err) = storage::json_save_atomic(file, &s.learned) {
            log::line(format!("command bar: what is used most was not saved: {err}"));
        }
    }
}

// ── The window: opened over the app in front, closed with the keyboard handed back ──

/// Centred on the display the pointer is on.
fn place(win: &WebviewWindow) {
    let Ok(cursor) = win.cursor_position() else { return };
    let Ok(Some(monitor)) = win.monitor_from_point(cursor.x, cursor.y) else { return };
    let (origin, area, scale) = (monitor.position(), monitor.size(), monitor.scale_factor());
    let (w, h) = ((SIZE.0 * scale) as i32, (SIZE.1 * scale) as i32);
    let _ = win.set_position(PhysicalPosition::new(origin.x + (area.width as i32 - w) / 2, origin.y + (area.height as i32 - h) / 2));
}

fn create(state: &Shared, cx: &Ctx) -> Result<WebviewWindow, String> {
    let win = cx.window().open(WindowSpec {
        label: LABEL.into(),
        feature: ID.into(),
        page: "bar".into(),
        w: SIZE.0,
        h: SIZE.1,
        frameless: true,
        topmost: true,
        transparent: true,
        monitor: Monitor::Cursor,
        ..Default::default()
    })?;
    let _ = win.set_resizable(false);
    let (state, cx) = (state.clone(), cx.clone());
    win.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event {
            // Judged a moment later, off the UI thread: between two windows there is none in front.
            let (state, cx) = (state.clone(), cx.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(40));
                left(&state, &cx);
            });
        }
    });
    Ok(win)
}

/// The shortcut was pressed: the bar comes up over the window in front and takes the keyboard.
fn open(state: &Shared, cx: &Ctx) -> Result<(), String> {
    // Read before the bar shows: who has the keyboard, and the windows front to back.
    let before = svc_windows::remember();
    let windows = svc_windows::windows();
    // The menu of the app in front, unless it runs as administrator (its commands could not be sent anyway).
    let menu = if before.hwnd() != 0 && !svc_inject::is_elevated(before.hwnd()) { rows::menu_commands(before.hwnd()) } else { Vec::new() };
    let menu_app = windows.iter().find(|w| w.hwnd == before.hwnd()).map(|w| w.app().to_string()).unwrap_or_default();

    let win = match cx.app().get_webview_window(LABEL) {
        Some(win) => win,
        None => create(state, cx)?,
    };
    let hwnd = win.hwnd().map_err(|e| e.to_string())?.0 as isize;
    place(&win);
    let _ = win.show();
    // Said again on every opening: a window that is shown again gets its taskbar button back.
    let _ = win.set_skip_taskbar(true);
    // Asked and checked: Windows may refuse, and a bar that is shown without the keyboard is of no use.
    if !svc_windows::activate(hwnd) {
        let _ = win.hide();
        return Err("Windows did not let the command bar come to the front.".into());
    }
    // The window is in front; the keys have to reach the page inside it.
    let _ = AsRef::<tauri::Webview>::as_ref(&win).set_focus();

    let settings = cx.settings(ID);
    {
        let mut s = lock(state);
        s.cat.windows = windows;
        s.cat.menu = menu;
        s.cat.menu_app = menu_app;
        s.cat.links = rows::links(settings["links"].as_str().unwrap_or_default());
        s.cat.sep = dates::separators();
        s.cat.day_first = dates::day_first();
        s.open = Some(Open { before, hwnd });
    }
    cx.emit(ID, "open", json!({}));

    // The installed apps are read again on every opening, off this thread; the list of last time shows meanwhile.
    let (state, cx) = (state.clone(), cx.clone());
    std::thread::spawn(move || {
        let apps = svc_windows::apps();
        let changed = {
            let mut s = lock(&state);
            let changed = s.cat.apps != apps;
            s.cat.apps = apps;
            changed && s.open.is_some()
        };
        if changed {
            cx.emit(ID, "refresh", json!({}));
        }
    });
    Ok(())
}

/// The bar goes away. With `back`, the keyboard goes to the window it was taken
/// from first, while the bar is still the one in front (Windows lets only the
/// window in front pass the front on). Answers with that window.
fn close(state: &Shared, cx: &Ctx, back: bool) -> Option<isize> {
    let open = lock(state).open.take()?;
    if back && !open.before.give_back(open.hwnd) && svc_windows::front().is_some_and(|f| f.hwnd == open.hwnd) {
        // Hidden below, the bar gives the front to the window Windows picks next.
        log::line("command bar: the keyboard could not be handed back to the window it came from");
    }
    if let Some(win) = cx.app().get_webview_window(LABEL) {
        let _ = win.hide();
    }
    cx.emit(ID, "closed", json!({}));
    Some(open.before.hwnd())
}

/// The bar lost the keyboard. A click on another window closes it; a click on
/// the on-screen keyboard, which is how some people type, does not.
fn left(state: &Shared, cx: &Ctx) {
    let Some(hwnd) = lock(state).open.as_ref().map(|o| o.hwnd) else { return };
    match svc_windows::front() {
        // The focus only moved inside the bar's own window.
        Some(front) if front.hwnd == hwnd => return,
        Some(front) if svc_windows::is_keyboard_exe(&front.exe) => return,
        _ => {}
    }
    let mut at = POINT::default();
    if unsafe { GetCursorPos(&mut at) }.is_ok() && svc_windows::is_onscreen_keyboard_point(at.x, at.y) {
        return;
    }
    // The user chose another window: it keeps the keyboard.
    close(state, cx, false);
}

// ── What a row does ───────────────────────────────────────────────────────────

fn copy(text: &str) -> Result<(), String> {
    let clipboard = clip::open()?;
    clipboard.empty();
    clipboard.set_text(text)
}

fn shutdown(args: &[&str]) -> Result<(), String> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let mut command = std::process::Command::new(Path::new(&root).join("System32").join("shutdown.exe"));
    // CREATE_NO_WINDOW: no console flashes up.
    command.args(args).creation_flags(0x0800_0000).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// The commands that change the state of the PC. Only ever reached from a row
/// the user ran; the ones that end the session or delete need `sure` first.
fn system(what: &str) -> Result<(), String> {
    match what {
        "lock" => unsafe { LockWorkStation() }.map_err(|e| e.message()),
        "sleep" => {
            // Returns when the PC wakes: on a thread of its own.
            std::thread::spawn(|| unsafe { SetSuspendState(false, false, false) });
            Ok(())
        }
        "restart" => shutdown(&["/r", "/t", "0"]),
        "shutdown" => shutdown(&["/s", "/t", "0"]),
        "signout" => shutdown(&["/l"]),
        "bin" => {
            // SHERB_NOCONFIRMATION: the bar has asked already. An empty bin is an error to Windows, not to us.
            let _ = unsafe { SHEmptyRecycleBinW(None, PCWSTR::null(), 1) };
            Ok(())
        }
        _ => Err("unknown command".into()),
    }
}

/// The rows that are asked about first. The page asks; this refuses to be told otherwise.
const ASKED_FIRST: [&str; 4] = ["restart", "shutdown", "signout", "bin"];

fn run(state: &Shared, cx: &Ctx, args: &Value) -> Result<Value, String> {
    // Enter held down, or pressed twice: the first one ran the row and closed the bar.
    if lock(state).open.is_none() {
        return Err("The command bar is closed.".into());
    }
    let id = args["id"].as_str().unwrap_or_default();
    let (kind, rest) = id.split_once(':').unwrap_or((id, ""));
    let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
    match kind {
        "app" => {
            let window = {
                let s = lock(state);
                let target = s.cat.apps.iter().find(|a| a.id == rest).map(|a| a.target.clone()).ok_or("That app is not installed any more.")?;
                s.cat.windows.iter().find(|w| same_file(&w.exe, &target)).map(|w| w.hwnd)
            };
            // Both while the bar is still in front: Windows then lets the other window, or the new app, come to the front.
            match window.filter(|_| args["new"].as_bool() != Some(true)) {
                Some(hwnd) if svc_windows::activate(hwnd) => {}
                _ => svc_shell::open(&format!("shell:AppsFolder\\{rest}"))?,
            }
            close(state, cx, false);
        }
        "win" => {
            let hwnd = lock(state).cat.windows.iter().map(|w| w.hwnd).find(|h| h.to_string() == rest).ok_or("That window is closed.")?;
            let came = svc_windows::activate(hwnd);
            close(state, cx, !came);
            if !came {
                return Err("That window is closed.".into());
            }
        }
        "sys" => {
            if ASKED_FIRST.contains(&rest) && args["sure"].as_bool() != Some(true) {
                return Err("This command asks first.".into());
            }
            close(state, cx, true);
            // The bar is gone before the screen changes.
            std::thread::sleep(Duration::from_millis(150));
            system(rest)?;
        }
        "folder" | "os" => {
            if !(rest.starts_with("shell:") || rest.starts_with("ms-settings:")) {
                return Err("unknown row".into());
            }
            svc_shell::open(rest)?;
            close(state, cx, false);
        }
        "info" | "answer" => {
            copy(&text("copy"))?;
            close(state, cx, true);
        }
        "emoji" => {
            // Typed where the text cursor was: the window gets the keyboard back first.
            close(state, cx, true);
            if svc_inject::type_text(rest).is_err() {
                // An administrator's window, or none to type into: it is on the clipboard instead.
                copy(rest)?;
                return Ok(json!({ "copied": true }));
            }
        }
        "menu" => {
            let command = lock(state).cat.menu.iter().map(|m| m.id).find(|m| m.to_string() == rest).ok_or("That menu command is gone.")?;
            if let Some(window) = close(state, cx, true).filter(|w| *w != 0) {
                unsafe {
                    let _ = PostMessageW(Some(HWND(window as *mut _)), WM_COMMAND, WPARAM(command as usize), LPARAM(0));
                }
            }
        }
        "link" => {
            let link = lock(state).cat.links.iter().find(|l| l.name == rest).cloned().ok_or("That link is not in Settings any more.")?;
            svc_shell::open(&rows::destination(&link, &text("argument")))?;
            close(state, cx, false);
        }
        "url" => {
            notch_core::win::open_url(&rows::web_address(&text("text")).ok_or("Not a web address.")?);
            close(state, cx, false);
        }
        "file" => {
            if !Path::new(rest).exists() {
                return Err("That file is not there any more.".into());
            }
            svc_shell::open(rest)?;
            close(state, cx, false);
        }
        _ => return Err("unknown row".into()),
    }
    if matches!(kind, "app" | "sys" | "folder" | "os" | "info" | "emoji" | "link") {
        learn(state, |l| l.record(id, now()));
    }
    // Said in so many words: to the page, nothing at all is a call that failed.
    Ok(json!({ "ok": true }))
}

/// What Ctrl+K offers for a row.
fn actions(state: &Shared, id: &str, learns: bool) -> Vec<Value> {
    let item = |id: &str, label: &str, icon: &str| json!({ "id": id, "label": notch_core::i18n::t(label), "icon": icon });
    let (kind, rest) = id.split_once(':').unwrap_or((id, ""));
    let s = lock(state);
    let mut out = Vec::new();
    match kind {
        "app" => {
            let target = s.cat.apps.iter().find(|a| a.id == rest).map(|a| a.target.as_str()).unwrap_or_default();
            if s.cat.windows.iter().any(|w| same_file(&w.exe, target)) {
                out.push(item("new", "Open a new window", "plus"));
            }
            if Path::new(target).exists() {
                out.push(item("reveal", "Show in folder", "folder"));
            }
        }
        "win" => out.push(item("close-window", "Close window", "close")),
        "file" => out.push(item("reveal", "Show in folder", "folder")),
        _ => {}
    }
    if learns {
        let pinned = s.learned.pins.iter().any(|p| p == id);
        out.push(if pinned { item("unpin", "Unpin", "pin") } else { item("pin", "Pin to the top", "pin") });
        if s.learned.usage.contains_key(id) {
            out.push(item("forget", "Forget how often I use this", "replay"));
        }
    }
    out
}

fn act(state: &Shared, cx: &Ctx, args: &Value) -> Result<Value, String> {
    if lock(state).open.is_none() {
        return Err("The command bar is closed.".into());
    }
    let id = args["id"].as_str().unwrap_or_default().to_string();
    let (kind, rest) = id.split_once(':').unwrap_or((&id, ""));
    match args["action"].as_str().unwrap_or_default() {
        "new" => return run(state, cx, &json!({ "id": id, "new": true })),
        "reveal" => {
            let path = if kind == "file" {
                rest.to_string()
            } else {
                lock(state).cat.apps.iter().find(|a| a.id == rest).map(|a| a.target.clone()).unwrap_or_default()
            };
            svc_shell::reveal(Path::new(&path))?;
            close(state, cx, false);
        }
        "close-window" => {
            // Only a window the bar listed, and only its own × (it may ask to save).
            let hwnd = lock(state).cat.windows.iter().map(|w| w.hwnd).find(|h| h.to_string() == rest).ok_or("That window is closed.")?;
            svc_windows::close(hwnd);
            lock(state).cat.windows.retain(|w| w.hwnd != hwnd);
        }
        "pin" => learn(state, |l| {
            if !l.pins.contains(&id) && l.pins.len() < 30 {
                l.pins.push(id.clone());
            }
        }),
        "unpin" => learn(state, |l| l.pins.retain(|p| *p != id)),
        "forget" => learn(state, |l| {
            l.usage.remove(&id);
        }),
        _ => return Err("unknown action".into()),
    }
    Ok(json!({ "ok": true }))
}

// ── The feature ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct CommandBar {
    state: Shared,
    /// The selection bar's watch, while its option is on.
    watch: Mutex<Option<selection::Watch>>,
}

impl CommandBar {
    fn arm(&self, cx: &Ctx) {
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        let on = cx.settings(ID)["selectionBar"].as_bool() == Some(true);
        if on && watch.is_none() {
            *watch = Some(selection::watch(cx));
        } else if !on && watch.take().is_some() {
            cx.window().close(selection::LABEL);
        }
    }
}

impl Feature for CommandBar {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let file = cx.data_dir(ID).join("learned.json");
        let mut s = lock(&self.state);
        s.learned = storage::json_load(&file);
        s.file = Some(file);
        drop(s);
        self.arm(cx);
        Ok(())
    }

    fn settings_changed(&self, cx: &Ctx, _settings: &Value) {
        self.arm(cx);
    }

    fn stop(&self, cx: &Ctx) {
        self.watch.lock().unwrap_or_else(|e| e.into_inner()).take();
        cx.window().close(selection::LABEL);
        close(&self.state, cx, true);
        cx.window().close(LABEL);
        *lock(&self.state) = State::default();
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let state = &self.state;
        match method {
            // The shortcut opens the bar, and closes it when it is open.
            "shortcut" => {
                if args["id"].as_str() == Some("toggle") {
                    if lock(state).open.is_some() {
                        close(state, cx, true);
                    } else {
                        open(state, cx)?;
                    }
                }
                Ok(Value::Null)
            }
            "search" => {
                let query: Query = serde_json::from_value(args).map_err(|e| e.to_string())?;
                let s = lock(state);
                Ok(json!(rows::search(&query, &s.cat, &s.learned, now())))
            }
            // Files by name, in the folders named in Settings: asked for apart, it takes longer than the rest.
            "files" => {
                let folders = cx.settings(ID)["fileFolders"].as_str().unwrap_or_default().to_string();
                Ok(json!(rows::files(args["text"].as_str().unwrap_or_default(), &folders)))
            }
            // The picture of an app, a program or a file, for the rows on screen.
            "icon" => {
                let png = svc_shell::icon(args["picture"].as_str().unwrap_or_default(), 32)?;
                Ok(json!(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))))
            }
            // The selection bar's page: the text it was shown for, a copy to the clipboard, and its end.
            "selection-get" => Ok(json!(self.watch.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|w| w.text.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap_or_default())),
            "selection-copy" => {
                copy(args["text"].as_str().unwrap_or_default())?;
                Ok(Value::Null)
            }
            "selection-hide" => {
                if let Some(w) = self.watch.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    selection::hide(cx, &w.text);
                }
                Ok(Value::Null)
            }
            "run" => run(state, cx, &args),
            // A row of another feature ran, in the page: it counts like the bar's own.
            "used" => {
                learn(state, |l| l.record(args["id"].as_str().unwrap_or_default(), now()));
                Ok(Value::Null)
            }
            // Settings › Command bar: the counts go, the pins stay.
            "forget-all" => {
                learn(state, |l| l.usage.clear());
                Ok(Value::Null)
            }
            "actions" => Ok(json!(actions(state, args["id"].as_str().unwrap_or_default(), args["learn"].as_bool() == Some(true)))),
            "act" => act(state, cx, &args),
            // Esc, a click beside the panel, or a row the page runs itself.
            "close" => {
                close(state, cx, args["back"].as_bool() != Some(false));
                Ok(Value::Null)
            }
            // The page of Settings a row leads to; the page then asks for the window.
            // debt: reaches into the settings page's address; `open_settings_window` should take the page.
            "settings-page" => {
                let page = args["page"].as_str().unwrap_or_default();
                if page.is_empty() || !page.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err("unknown page".into());
                }
                if let Some(settings) = cx.app().get_webview_window("settings") {
                    let _ = settings.eval(format!("location.hash = '#/{page}'"));
                }
                Ok(Value::Null)
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/command-bar/strings.json")
    }
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(CommandBar::default())
}
