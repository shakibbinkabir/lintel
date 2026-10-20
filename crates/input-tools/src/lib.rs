// Input tools — text snippets and their library, key and click debounce, key
// remaps, blocked shortcuts per app, the wheel's direction, side buttons and
// pointer acceleration.
//
// The contract is docs/ARCHITECTURE.md §2: nothing runs before `start`,
// everything is gone after `stop`, every command arrives through `call`. This
// file is the glue: settings into the engine's configuration, the library
// window, the journal of the pointer setting. What happens to a key or a click
// is in engine.rs, on the shared hub (svc-input). Every tool is off until the
// user turns it on, and with all of them off there is no hook.

pub mod engine;
pub mod keys;
pub mod pointer;
pub mod text;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine::{Config, Engine, Event, Snippet};
use notch_core::{log, Ctx, Feature, Group, Manifest, Monitor, ShortcutDef, Use, WindowSpec};
use serde_json::{json, Value};
use svc_inject::ForegroundGuard;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

const ID: &str = "input-tools";
const LIBRARY: &str = "input-tools-library";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Input tools",
    group: Group::Input,
    // Opt-in: it reads the keyboard and the mouse.
    default_on: false,
    uses: &[Use::InputHook, Use::Clipboard],
    shortcuts: &[ShortcutDef { id: "library", label: "Open the snippet library", default: Some("Ctrl+Alt+L") }],
    tray: &[],
    // snippetList: { id, name, trigger, text, mode: "delimiter" | "immediate", on, ignoreCase, folder, library }.
    // remapList: { from, to, on }, combinations as keys.rs names them ("" for nothing).
    // keyDebounceKeys: "E:40, Space:25". Lists of apps are program names (notepad.exe).
    defaults: r#"{ "snippets": false, "snippetList": [], "keyDebounce": false, "keyDebounceMs": 5, "keyDebounceKeys": "", "clickDebounce": false, "clickDebounceMs": 25, "pointerAccelOff": false, "invertVertical": false, "invertHorizontal": false, "padVertical": false, "padHorizontal": false, "sideways": false, "sidewaysKey": "Shift", "scrollExceptions": [], "buttons": false, "buttonBack": "", "buttonForward": "", "wheelLeft": "", "wheelRight": "", "buttonExceptions": [], "remaps": false, "remapList": [], "guard": false, "guardApps": [], "guardKeys": [] }"#,
};

// ── Settings into configuration ───────────────────────────────────────────────

/// The snippets as stored, the unusable ones left out: a trigger is 2 to 40
/// characters without spaces, and there must be a text.
fn snippet_list(s: &Value) -> Vec<Snippet> {
    let text = |v: &Value, key: &str| v[key].as_str().unwrap_or_default().to_string();
    s["snippetList"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| Snippet {
            id: text(v, "id"),
            name: text(v, "name"),
            trigger: text(v, "trigger").chars().filter(|c| !c.is_whitespace()).take(40).collect(),
            text: text(v, "text"),
            immediate: v["mode"] == "immediate",
            ignore_case: v["ignoreCase"] == true,
            folder: text(v, "folder"),
            library: v["library"] != false,
            on: v["on"] != false,
        })
        .filter(|s| s.trigger.chars().count() >= 2 && !s.text.is_empty())
        .collect()
}

fn config(s: &Value) -> Config {
    let on = |key: &str| s[key] == true;
    let text = |key: &str| s[key].as_str().unwrap_or_default();
    let list = |key: &str| s[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::trim).filter(|x| !x.is_empty());
    let apps = |key: &str| -> Vec<String> { list(key).map(str::to_lowercase).collect() };
    // The upstream clamps: out of range falls back to the default.
    let ms = |key: &str, range: std::ops::RangeInclusive<u64>, fallback: u32| s[key].as_u64().filter(|v| range.contains(v)).map_or(fallback, |v| v as u32);
    let pressed = |key: &str| keys::strokes(text(key)).unwrap_or_default();
    let guard = on("guard");
    Config {
        snippets: if on("snippets") { snippet_list(s) } else { Vec::new() },
        key_debounce: on("keyDebounce"),
        key_ms: ms("keyDebounceMs", 0..=500, 5),
        key_overrides: text("keyDebounceKeys")
            .split(',')
            .filter_map(|pair| {
                let (name, ms) = pair.split_once(':')?;
                let vk = keys::parse(name.trim())?.vk;
                let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
                Some((engine::key_slot(scan, false), ms.trim().parse::<u32>().ok()?.min(500)))
            })
            .collect(),
        click_debounce: on("clickDebounce"),
        click_ms: ms("clickDebounceMs", 5..=100, 25),
        invert_vertical: on("invertVertical"),
        invert_horizontal: on("invertHorizontal"),
        pad_vertical: on("padVertical"),
        pad_horizontal: on("padHorizontal"),
        sideways: on("sideways").then(|| match text("sidewaysKey") {
            "Alt" => keys::ALT,
            "Ctrl" => keys::CTRL,
            _ => keys::SHIFT,
        }),
        scroll_exceptions: apps("scrollExceptions"),
        buttons: if on("buttons") { ["buttonBack", "buttonForward", "wheelLeft", "wheelRight"].map(pressed) } else { Default::default() },
        button_exceptions: apps("buttonExceptions"),
        remaps: s["remapList"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| on("remaps") && r["on"] != false)
            .filter_map(|r| Some((keys::parse(r["from"].as_str()?)?, keys::strokes(r["to"].as_str().unwrap_or_default())?)))
            .collect(),
        guard_apps: if guard { apps("guardApps") } else { Vec::new() },
        guard_keys: if guard { list("guardKeys").filter_map(keys::parse).collect() } else { Vec::new() },
        probe: false,
    }
}

// ── The feature ───────────────────────────────────────────────────────────────

struct Running {
    engine: Engine,
    /// The window that was in front when the library opened: where its pick is typed.
    target: Mutex<isize>,
}

#[derive(Default)]
pub struct InputTools {
    run: Mutex<Option<Arc<Running>>>,
}

fn own_window(hwnd: isize) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };
    pid == std::process::id()
}

/// The window in front, or the last one of another program when the app itself is in front.
fn other_window(engine: &Engine) -> isize {
    let front = ForegroundGuard::capture().hwnd();
    if own_window(front) { engine.last_front() } else { front }
}

fn set_pointer(cx: &Ctx, off: bool) {
    if let Err(why) = pointer::apply(&cx.data_dir(ID).join("pointer.json"), off) {
        log::line(format!("input-tools: pointer acceleration: {why}"));
    }
}

impl InputTools {
    fn running(&self) -> Result<Arc<Running>, String> {
        self.run.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| "Input tools is turned off.".into())
    }
}

impl Feature for InputTools {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let mut run = self.run.lock().unwrap_or_else(|e| e.into_inner());
        if run.is_some() {
            return Ok(());
        }
        let engine = {
            let cx = cx.clone();
            Engine::start(move |event| match event {
                Event::Expanded { id } => cx.emit(ID, "expanded", json!({ "id": id })),
                Event::Recorded(chord) => cx.emit(ID, "recorded", json!({ "chord": chord })),
            })
        };
        let settings = cx.settings(ID);
        engine.apply(config(&settings));
        // Also the launch after a crash: with the switch off, a value left in the journal is put back.
        set_pointer(cx, settings["pointerAccelOff"] == true);
        *run = Some(Arc::new(Running { engine, target: Mutex::new(0) }));
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        // Dropping the engine drops its subscriptions: the hooks go with the last one.
        if self.run.lock().unwrap_or_else(|e| e.into_inner()).take().is_some() {
            set_pointer(cx, false);
        }
    }

    fn settings_changed(&self, cx: &Ctx, settings: &Value) {
        if let Ok(run) = self.running() {
            run.engine.apply(config(settings));
            set_pointer(cx, settings["pointerAccelOff"] == true);
        }
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let run = self.running()?;
        match method {
            // What is installed, for Settings.
            "status" => {
                let hub = svc_input::status();
                Ok(json!({
                    "keyboard": hub.keyboard,
                    "mouse": hub.mouse,
                    "suspended": !hub.suspended_by.is_empty(),
                    "elevated": hub.elevated,
                    "refreshed": hub.refreshed,
                    "stages": run.engine.stages(),
                    // Windows' own setting as it is now (null when it cannot be read).
                    "acceleration": pointer::read().map(|mouse| mouse[2] != 0),
                }))
            }
            // The programs with a window, for the lists of apps.
            "apps" => {
                let table = notch_core::win::process_table();
                let mut apps: Vec<String> = notch_core::win::top_windows()
                    .into_iter()
                    .filter(|w| w.1 != std::process::id())
                    .filter_map(|w| table.get(&w.1).map(|p| p.exe.to_lowercase()))
                    .collect();
                apps.sort();
                apps.dedup();
                Ok(json!(apps))
            }
            "record" => {
                run.engine.record();
                Ok(Value::Null)
            }
            "record-stop" => {
                run.engine.record_stop();
                Ok(Value::Null)
            }
            // A snippet's text as it would be inserted now, for the editor.
            "preview" => Ok(json!(engine::expand(args["text"].as_str().unwrap_or_default()))),
            "shortcut" | "library" => {
                if method == "shortcut" && args["id"] != "library" {
                    return Err("unknown method".into());
                }
                *run.target.lock().unwrap_or_else(|e| e.into_inner()) = other_window(&run.engine);
                // debt: the library window is built anew each time it is asked for.
                cx.window().open(WindowSpec {
                    label: LIBRARY.into(),
                    feature: ID.into(),
                    page: "library".into(),
                    w: 460.0,
                    h: 420.0,
                    frameless: true,
                    topmost: true,
                    monitor: Monitor::Cursor,
                    ..Default::default()
                })?;
                Ok(Value::Null)
            }
            "library-close" => {
                cx.window().close(LIBRARY);
                Ok(Value::Null)
            }
            // A pick in the library (or the command bar): typed into the window the user was in.
            "insert" => {
                let id = args["id"].as_str().unwrap_or_default();
                let snippet = snippet_list(&cx.settings(ID)).into_iter().find(|s| s.id == id).ok_or("That snippet is not there any more.")?;
                let kept = std::mem::take(&mut *run.target.lock().unwrap_or_else(|e| e.into_inner()));
                let target = if kept != 0 { kept } else { other_window(&run.engine) };
                cx.window().close(LIBRARY);
                std::thread::sleep(Duration::from_millis(150));
                let sent = ForegroundGuard::of(target).restore().and_then(|_| {
                    // A key still held would turn what is typed into shortcuts.
                    if !svc_inject::wait_modifiers_up() {
                        return Err("Release the keys you are holding and try again.".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(60));
                    engine::insert(0, &engine::expand(&snippet.text), None)
                });
                Ok(match sent {
                    Ok(()) => json!({ "inserted": true }),
                    Err(why) => {
                        log::line(format!("input-tools: a snippet was not inserted: {why}"));
                        let result = json!({ "inserted": false, "elevated": why == svc_inject::ELEVATED });
                        cx.emit(ID, "insert-failed", result.clone());
                        result
                    }
                })
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/input-tools/strings.json")
    }
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(InputTools::default())
}
