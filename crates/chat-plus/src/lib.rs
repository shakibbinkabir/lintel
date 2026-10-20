// Chat plus: "Always" on approval cards, drag the mascot onto a window to attach
// it as chat context, and send a dropped file by email.
// The contract is docs/ARCHITECTURE.md §2: nothing runs before `start`.

mod mail;
pub mod target;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use notch_core::{Ctx, Feature, Group, Manifest, Monitor, Use, WindowSpec};
use serde_json::{json, Value};
use target::Hit;
use tauri::{PhysicalPosition, PhysicalSize};

static MANIFEST: Manifest = Manifest {
    id: "chat-plus",
    name: "Chat plus",
    group: Group::Chat,
    default_on: true,
    uses: &[Use::Network],
    shortcuts: &[],
    tray: &[],
    defaults: r#"{ "from": "" }"#,
};

const HALO: &str = "chat-plus-halo";
const MAIL: &str = "chat-plus-mail";

/// Requests the user answered with Always; `pipe::answer` takes the mark off.
static ALWAYS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static DRAGGING: AtomicBool = AtomicBool::new(false);
static HALO_GEN: AtomicU32 = AtomicU32::new(0);
/// The inbox copy the mail window is about.
static MAIL_FILE: Mutex<Option<String>> = Mutex::new(None);
/// What the mail window starts with (to, subject, message): empty for a plain dropped file.
static MAIL_DRAFT: Mutex<[String; 3]> = Mutex::new([String::new(), String::new(), String::new()]);

/// One line, no control characters, at most `max` characters.
fn one_line(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

/// True once, if the user clicked Always on this request.
pub fn take_always(request_id: &str) -> bool {
    ALWAYS.lock().map(|mut s| { let n = s.len(); s.retain(|i| i != request_id); s.len() != n }).unwrap_or(false)
}

#[derive(Default)]
pub struct ChatPlus;

impl Feature for ChatPlus {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, _cx: &Ctx) -> Result<(), String> {
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        ALWAYS.lock().map(|mut s| s.clear()).ok();
        cx.window().close(HALO);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let text = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
        match method {
            "always" => {
                let id = text("requestId");
                if id.is_empty() || id.len() > 64 {
                    return Err("no request".into());
                }
                ALWAYS.lock().map_err(|e| e.to_string())?.push(id);
                Ok(json!({}))
            }
            "drag" => drag(cx),
            // A capture the user just took (Ctrl or Shift on the mascot) joins the chat: the chat only
            // takes files from its inbox, so the picture is copied there.
            "adopt" => {
                let name = text("name");
                if name.is_empty() || std::path::Path::new(&name).file_name() != Some(std::ffi::OsStr::new(&name)) {
                    return Err("no such capture".into());
                }
                let from = cx.data_dir("capture").join("captures").join(&name);
                let inbox = notch_core::files::inbox_dir();
                std::fs::create_dir_all(&inbox).map_err(|e| e.to_string())?;
                let to = inbox.join(&name);
                std::fs::copy(&from, &to).map_err(|e| format!("The capture could not be attached: {e}"))?;
                Ok(json!({ "name": name, "path": to.to_string_lossy() }))
            }
            // The text of a dropped file, for the code card in the chat: only a copy in the inbox, small, and text.
            "read_text" => {
                let path = std::path::PathBuf::from(text("path"));
                let (Ok(inbox), Ok(file)) = (notch_core::files::inbox_dir().canonicalize(), path.canonicalize()) else {
                    return Ok(Value::Null);
                };
                let small = std::fs::metadata(&file).map(|m| m.is_file() && m.len() <= 100_000).unwrap_or(false);
                if file.parent() != Some(inbox.as_path()) || !small {
                    return Ok(Value::Null);
                }
                Ok(std::fs::read_to_string(&file).map(Value::from).unwrap_or(Value::Null))
            }
            "mail_open" => {
                // The file is optional (voice prepares an email with or without one). A path that is
                // given is still checked as before; the draft only fills the window's fields, it never sends.
                let path = text("path");
                let file = if path.is_empty() { None } else { Some(mail::checked_file(&path)?) };
                *MAIL_FILE.lock().map_err(|e| e.to_string())? = file.map(|f| f.to_string_lossy().to_string());
                let body: String = text("message").chars().take(20_000).collect();
                *MAIL_DRAFT.lock().map_err(|e| e.to_string())? = [one_line(&text("to"), 320), one_line(&text("subject"), 300), body];
                cx.window().open(WindowSpec {
                    label: MAIL.into(),
                    feature: "chat-plus".into(),
                    page: "mail".into(),
                    w: 420.0,
                    h: 400.0,
                    frameless: true,
                    topmost: true,
                    monitor: Monitor::Island,
                    ..Default::default()
                })?;
                // A window that was already open shows the new draft.
                cx.emit("chat-plus", "mailChanged", json!({}));
                Ok(json!({}))
            }
            "mail_info" => {
                let file = mail_file().ok();
                let size = file.as_ref().and_then(|f| std::fs::metadata(f).ok()).map(|m| m.len()).unwrap_or(0);
                let [to, subject, message] = MAIL_DRAFT.lock().map_err(|e| e.to_string())?.clone();
                Ok(json!({
                    "name": file.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "size": size,
                    "to": to, "subject": subject, "message": message,
                    "resend": notch_core::secrets::present("resend-api-key"),
                    "from": cx.settings("chat-plus").get("from").and_then(Value::as_str).unwrap_or_default(),
                }))
            }
            "mail_close" => {
                cx.window().close(MAIL);
                Ok(json!({}))
            }
            "mail_send" => {
                // A mail written by voice has no file; one that is there must still be a dropped copy.
                let file = match MAIL_FILE.lock().map_err(|e| e.to_string())?.clone() {
                    Some(path) => Some(mail::checked_file(&path)?),
                    None => None,
                };
                let file = file.as_ref();
                let (to, from, text_body) = (text("to"), text("from"), text("message"));
                let name = file.and_then(|f| f.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let subject = Some(text("subject")).filter(|s| !s.is_empty()).unwrap_or(name);
                if !mail::valid_address(&to) {
                    return Err("Enter a valid recipient address.".into());
                }
                if subject.contains(['\r', '\n']) {
                    return Err("The subject must be one line.".into());
                }
                match notch_core::secrets::get("resend-api-key") {
                    Some(key) => {
                        if cx.paused() {
                            return Err("Lintel is paused.".into());
                        }
                        if !mail::valid_address(&from) {
                            return Err("Set the sender address (a domain verified in Resend).".into());
                        }
                        let sent = mail::via_resend(&key, &from, &to, &subject, &text_body, file)?;
                        cx.patch_settings("chat-plus", json!({ "from": from }));
                        Ok(sent)
                    }
                    None => Ok(mail::via_client(&to, &subject, &text_body, file)),
                }
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/chat-plus/strings.json")
    }
}

fn mail_file() -> Result<std::path::PathBuf, String> {
    let path = MAIL_FILE.lock().map_err(|e| e.to_string())?.clone().ok_or("No file to send.")?;
    mail::checked_file(&path)
}

/// While the left button is down, follows the pointer with a halo over the window
/// under it; on release over another app's window, tells the page which one.
fn drag(cx: &Ctx) -> Result<Value, String> {
    if DRAGGING.swap(true, Ordering::SeqCst) {
        return Ok(json!({}));
    }
    let gen = HALO_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let cx = cx.clone();
    std::thread::spawn(move || {
        let landed = follow(&cx);
        DRAGGING.store(false, Ordering::SeqCst);
        cx.emit("chat-plus", "ended", json!({}));
        if let Some((app, title)) = landed {
            cx.emit("chat-plus", "attached", json!({ "appName": app, "title": title }));
            // debt: the halo clears on a timer, not when the answer arrives.
            std::thread::sleep(Duration::from_secs(4));
        }
        if HALO_GEN.load(Ordering::SeqCst) == gen {
            cx.window().close(HALO);
        }
    });
    Ok(json!({}))
}

fn follow(cx: &Ctx) -> Option<(String, String)> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    let mut halo: Option<tauri::WebviewWindow> = None;
    let mut halo_id = 0isize;
    let mut shown: Option<target::Target> = None;
    let mut last = None;
    let start = std::time::Instant::now();
    while unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000 != 0 && start.elapsed() < Duration::from_secs(120) {
        if let Some((x, y)) = target::cursor() {
            match target::at(x, y, halo_id) {
                Hit::Window(t) => {
                    if halo.is_none() {
                        halo = open_halo(cx);
                        halo_id = halo.as_ref().and_then(|w| w.hwnd().ok()).map(|h| h.0 as isize).unwrap_or(0);
                    }
                    if let (Some(w), true) = (&halo, shown.as_ref() != Some(&t)) {
                        let (l, tp, r, b) = t.rect;
                        let _ = w.set_position(PhysicalPosition::new(l, tp));
                        let _ = w.set_size(PhysicalSize::new((r - l) as u32, (b - tp) as u32));
                        let _ = w.show();
                    }
                    shown = Some(t.clone());
                    last = Some(t);
                }
                _ => {
                    if shown.take().is_some() {
                        if let Some(w) = &halo {
                            let _ = w.hide();
                        }
                    }
                    last = None;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    // Released over a window: that is the one. Over the island or nothing: no attach.
    last.map(|t| (target::app_name(t.pid), t.title))
}

fn open_halo(cx: &Ctx) -> Option<tauri::WebviewWindow> {
    let w = cx
        .window()
        .open(WindowSpec {
            label: HALO.into(),
            feature: "chat-plus".into(),
            page: "halo".into(),
            w: 200.0,
            h: 200.0,
            frameless: true,
            topmost: true,
            no_activate: true,
            exclude_from_capture: true,
            transparent: true,
            ..Default::default()
        })
        .ok()?;
    let _ = w.hide();
    let _ = w.set_ignore_cursor_events(true);
    Some(w)
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(ChatPlus)
}
