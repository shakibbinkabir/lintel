// cargo run -p notch-feat-input-tools --example input-probe
//
// Proves the input hub (svc-input) and the stages of this feature on a window
// of the probe's own: a small box with one text field, opened under the
// pointer where it rests (the pointer is never moved) and brought to the front.
//
// Keys are only sent while that window is the one in front, and wheel turns
// and clicks only while the pointer is over it; a step that cannot have that
// is skipped, never sent elsewhere. The hooks live for the few seconds of the
// run. A watchdog ends the process after 30 seconds whatever happens, and
// Windows takes a process's hooks out with it; the last steps show both that
// they are gone after a clean stop and after a kill (a child of the probe).
//
// Not touched: the pointer's acceleration (read, never written), the
// clipboard, any sound. Caps Lock is put back if a failed step toggled it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notch_feat_input_tools::engine::{key_slot, Config, Engine, Event, Snippet};
use notch_feat_input_tools::{keys, pointer, text};
use svc_inject::ForegroundGuard;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, MapVirtualKeyW, SendInput, SetFocus, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetForegroundWindow, GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, WindowFromPoint, GWLP_WNDPROC, MSG, SW_SHOW, WINDOW_STYLE, WM_CHAR, WM_CLOSE, WM_DESTROY,
    WM_GETTEXT, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_SETFOCUS, WM_SETTEXT, WM_SYSKEYDOWN,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WNDPROC, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

static EDIT: AtomicIsize = AtomicIsize::new(0);
static EDIT_PROC: AtomicIsize = AtomicIsize::new(0);

/// What the probe's window was sent.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Got {
    Key(u16),
    Wheel(i32),
    /// The wheel, sideways.
    HWheel(i32),
    SideButton,
    LeftDown,
}

static GOT: Mutex<Vec<Got>> = Mutex::new(Vec::new());

fn got(what: Got) {
    GOT.lock().unwrap().push(what);
}

/// The field's own procedure, with a look at what arrives first.
unsafe extern "system" fn field(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => got(Got::Key(wparam.0 as u16)),
        // A plain field beeps at control characters (Esc, Ctrl+B): they stop here. Backspace is its own.
        WM_CHAR if wparam.0 < 0x20 && wparam.0 != 8 => return LRESULT(0),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16 as i32;
            got(if msg == WM_MOUSEWHEEL { Got::Wheel(delta) } else { Got::HWheel(delta) });
            return LRESULT(0);
        }
        WM_XBUTTONDOWN => {
            got(Got::SideButton);
            return LRESULT(1);
        }
        WM_XBUTTONUP => return LRESULT(1),
        WM_LBUTTONDOWN => got(Got::LeftDown),
        _ => {}
    }
    let own: WNDPROC = unsafe { std::mem::transmute(EDIT_PROC.load(Ordering::SeqCst)) };
    unsafe { CallWindowProcW(own, hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SETFOCUS => {
            let _ = unsafe { SetFocus(Some(HWND(EDIT.load(Ordering::SeqCst) as *mut _))) };
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn edit() -> HWND {
    HWND(EDIT.load(Ordering::SeqCst) as *mut _)
}

fn field_text() -> String {
    let mut text = [0u16; 512];
    let len = unsafe { SendMessageW(edit(), WM_GETTEXT, Some(WPARAM(text.len())), Some(LPARAM(text.as_mut_ptr() as isize))) }.0;
    String::from_utf16_lossy(&text[..len as usize])
}

fn clear_field() {
    unsafe { SendMessageW(edit(), WM_SETTEXT, None, Some(LPARAM(w!("").as_ptr() as isize))) };
    GOT.lock().unwrap().clear();
}

fn wait(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

// ── Input the probe sends: unmarked, as another program's would be ────────────

fn key(vk: u16, down: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let flags = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn tap(vk: u16) -> [INPUT; 2] {
    [key(vk, true), key(vk, false)]
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

/// The keys that type `text` on this keyboard's layout.
fn typing(text: &str) -> Option<Vec<INPUT>> {
    let mut out = Vec::new();
    for c in text.encode_utf16() {
        let found = unsafe { VkKeyScanW(c) };
        if found == -1 {
            return None;
        }
        let (vk, shift) = (found as u16 & 0xFF, (found as u16 >> 8) as u8);
        // 1 Shift, 2 Ctrl, 4 Alt (the last two together are AltGr).
        let held: Vec<u16> = [(1, 0x10u16), (2, 0x11), (4, 0x12)].iter().filter(|m| shift & m.0 != 0).map(|m| m.1).collect();
        out.extend(held.iter().map(|m| key(*m, true)));
        out.extend(tap(vk));
        out.extend(held.iter().rev().map(|m| key(*m, false)));
    }
    Some(out)
}

struct Probe {
    own: isize,
    all: bool,
    skipped: u32,
}

impl Probe {
    fn in_front(&self) -> bool {
        unsafe { GetForegroundWindow() }.0 as isize == self.own
    }

    fn under_pointer(&self) -> bool {
        let mut at = POINT::default();
        let hit = unsafe {
            let _ = GetCursorPos(&mut at);
            WindowFromPoint(at)
        };
        hit.0 as isize == self.own || hit == edit()
    }

    fn skip(&mut self, what: &str, why: &str) {
        println!("SKIP {what}: {why}; nothing was sent");
        self.skipped += 1;
    }

    /// Keys, to the probe's window only.
    fn keys(&mut self, what: &str, inputs: &[INPUT]) -> bool {
        if !self.in_front() {
            self.skip(what, "another window came to the front");
            return false;
        }
        unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        wait(350);
        true
    }

    /// Wheel turns and clicks, to the probe's window only.
    fn mouse(&mut self, what: &str, inputs: &[INPUT]) -> bool {
        if !self.in_front() || !self.under_pointer() {
            self.skip(what, "the probe's window is not under the pointer and in front");
            return false;
        }
        unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        wait(350);
        true
    }

    fn check(&mut self, what: &str, ok: bool, detail: String) {
        println!("{} {what}: {detail}", if ok { "ok  " } else { "FAIL" });
        self.all &= ok;
    }

    /// One notch of the wheel sent, and what the window was given for it.
    fn wheel(&mut self, what: &str, sent: i32, expected: i32) {
        GOT.lock().unwrap().clear();
        if self.mouse(what, &[mouse(MOUSEEVENTF_WHEEL, sent)]) {
            let seen = GOT.lock().unwrap().clone();
            self.check(what, seen == [Got::Wheel(expected)], format!("sent {sent}, the window got {seen:?}"));
        }
    }
}

fn hub() -> String {
    let s = svc_input::status();
    format!(
        "keyboard hook {}, mouse hook {}, subscribers {} + {}, suspended by {:?}, reinstalled {}",
        if s.keyboard { "installed" } else { "absent" },
        if s.mouse { "installed" } else { "absent" },
        s.keyboard_subscribers,
        s.mouse_subscribers,
        s.suspended_by,
        s.refreshed
    )
}

fn exercise(own: isize) -> bool {
    let mut p = Probe { own, all: true, skipped: 0 };
    let before = ForegroundGuard::capture();
    let caps = || unsafe { GetKeyState(0x14) } & 1 != 0;
    let caps_before = caps();
    p.check("key names and snippet variables", keys::self_check() && text::self_check(), "self-checks".into());
    println!("pointer acceleration as Windows has it (read only, never written here): {:?}", pointer::read());

    if let Err(err) = ForegroundGuard::of(own).restore() {
        println!("SKIP everything: the probe's window could not be brought to the front ({err}); nothing was sent");
        return false;
    }
    wait(200);

    let exe = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase())).unwrap_or_default();
    let recorded = Arc::new(Mutex::new(None::<String>));
    let engine = {
        let recorded = recorded.clone();
        Engine::start(move |event| {
            println!("     event: {event:?}");
            if let Event::Recorded(chord) = event {
                *recorded.lock().unwrap() = Some(chord);
            }
        })
    };
    engine.apply(Config::default());
    let idle = svc_input::status();
    p.check("every tool off", !idle.keyboard && !idle.mouse && idle.keyboard_subscribers + idle.mouse_subscribers == 0, hub());

    let q = key_slot(unsafe { MapVirtualKeyW(0x51, MAPVK_VK_TO_VSC) } as u16, false);
    let config = |guard_app: &str, scroll_exception: Option<&str>| Config {
        snippets: vec![
            Snippet { id: "sig".into(), trigger: ";sig".into(), text: "Kind regards, Ada ({{date:yyyy}})".into(), on: true, ..Default::default() },
            Snippet { id: "now".into(), trigger: ";;n".into(), text: "at once".into(), immediate: true, on: true, ..Default::default() },
        ],
        // Only the Q key is debounced here, so that nothing else typed on this PC meanwhile is.
        key_debounce: true,
        key_ms: 0,
        key_overrides: vec![(q, 60)],
        click_debounce: true,
        click_ms: 25,
        invert_vertical: true,
        sideways: Some(keys::SHIFT),
        scroll_exceptions: scroll_exception.map(str::to_string).into_iter().collect(),
        buttons: [keys::strokes("Ctrl+F9").unwrap(), Vec::new(), Vec::new(), Vec::new()],
        remaps: vec![(keys::parse("CapsLock").unwrap(), keys::strokes("Esc").unwrap())],
        guard_apps: vec![guard_app.to_string()],
        guard_keys: vec![keys::parse("Ctrl+B").unwrap()],
        probe: true,
        ..Default::default()
    };
    engine.apply(config(&exe, None));
    let live = svc_input::status();
    p.check("tools on", live.keyboard && live.mouse, format!("{}; stages {:?}", hub(), engine.stages()));

    // A snippet that expands after a space, and one that expands at once.
    let year = text::expand("{{date:yyyy}}", &String::new);
    let mut expands = false;
    clear_field();
    match typing(";sig ") {
        Some(inputs) => {
            if p.keys("snippet", &inputs) {
                let (text, wanted) = (field_text(), format!("Kind regards, Ada ({year}) "));
                expands = text == wanted;
                p.check("snippet \";sig\" then a space", expands, format!("the field holds {text:?}"));
            }
        }
        None => p.skip("snippet", "this keyboard layout has no key for one of its characters"),
    }
    clear_field();
    if let Some(inputs) = typing(";;n") {
        if p.keys("snippet at once", &inputs) {
            let text = field_text();
            p.check("snippet \";;n\" expanding on its last character", text == "at once", format!("the field holds {text:?}"));
        }
    }

    // A doubled press: the second Q comes within its window and is swallowed with its release.
    clear_field();
    if p.keys("key debounce", &[tap(0x51), tap(0x51)].concat()) {
        let text = field_text().to_lowercase();
        p.check("a doubled Q press", text == "q", format!("two presses sent, the field holds {text:?}"));
        wait(100);
        if p.keys("key debounce", &tap(0x51)) {
            let text = field_text().to_lowercase();
            p.check("a later Q press", text == "qq", format!("the field holds {text:?}"));
        }
    }

    // Caps Lock to Esc. Only tried once the hub is seen to work: unhooked, the key would toggle Caps Lock.
    clear_field();
    if !expands {
        p.skip("remap", "the snippet step did not show the hooks at work");
    } else if p.keys("remap", &tap(0x14)) {
        let seen = GOT.lock().unwrap().clone();
        let ok = seen.contains(&Got::Key(0x1B)) && !seen.contains(&Got::Key(0x14)) && caps() == caps_before;
        p.check("Caps Lock remapped to Esc", ok, format!("Caps Lock sent, the window got {seen:?}; Caps Lock state unchanged: {}", caps() == caps_before));
    }

    // Ctrl+B is blocked while this program is in front, and passes when the list names another.
    let ctrl_b = [key(0x11, true), key(0x42, true), key(0x42, false), key(0x11, false)];
    clear_field();
    if p.keys("blocked shortcut", &ctrl_b) {
        let seen = GOT.lock().unwrap().clone();
        let ok = engine.guard_live() && seen.contains(&Got::Key(0x11)) && !seen.contains(&Got::Key(0x42));
        p.check("Ctrl+B blocked while the probe's window is in front", ok, format!("guard live: {}, the window got {seen:?}", engine.guard_live()));
    }
    engine.apply(config("another-app.exe", None));
    clear_field();
    if p.keys("blocked shortcut", &ctrl_b) {
        let seen = GOT.lock().unwrap().clone();
        let ok = !engine.guard_live() && seen.contains(&Got::Key(0x42));
        p.check("Ctrl+B not blocked when the list names another app", ok, format!("guard live: {}, the window got {seen:?}", engine.guard_live()));
    }
    engine.apply(config(&exe, None));

    // The recorder hears a combination and keeps it from the window.
    clear_field();
    engine.record();
    if p.keys("recorder", &[key(0x11, true), key(0x10, true), key(0x4B, true), key(0x4B, false), key(0x10, false), key(0x11, false)]) {
        let heard = recorded.lock().unwrap().clone();
        let seen = GOT.lock().unwrap().clone();
        p.check("recorder", heard.as_deref() == Some("Ctrl+Shift+K") && seen.is_empty(), format!("heard {heard:?}, the window got {seen:?}"));
    }
    engine.record_stop();

    // The wheel: a whole notch is a mouse and is turned round, a part notch is a touchpad and is not.
    p.wheel("wheel inverted (a mouse notch)", 120, -120);
    p.wheel("wheel left alone (a touchpad step)", 40, 40);
    // "Reverse the touchpad": a part notch is turned round, a whole notch is left alone.
    engine.apply(Config { pad_vertical: true, probe: true, ..Default::default() });
    p.wheel("touchpad step turned round (reverse the touchpad)", 40, -40);
    p.wheel("mouse notch left alone (reverse the touchpad)", 120, 120);
    engine.apply(config(&exe, None));
    // With Shift held (and only Shift) a notch away from the user goes to the left.
    clear_field();
    if p.keys("sideways", &[key(0x10, true)]) {
        if p.mouse("sideways", &[mouse(MOUSEEVENTF_WHEEL, 120)]) {
            let seen = GOT.lock().unwrap().clone();
            p.check("wheel sideways while Shift is held", seen.contains(&Got::HWheel(-120)) && !seen.contains(&Got::Wheel(120)), format!("sent 120, the window got {seen:?}"));
        }
        unsafe { SendInput(&[key(0x10, false)], std::mem::size_of::<INPUT>() as i32) };
        wait(100);
    }
    {
        let _held = svc_input::suspend_all("probe");
        p.wheel("wheel while the hub is suspended", 120, 120);
    }
    // What resume, unlock and a display change do: both hooks installed again, and still at work.
    svc_input::refresh();
    wait(150);
    let again = svc_input::status();
    p.check("hooks installed again", again.keyboard && again.mouse && again.refreshed == 2, hub());
    p.wheel("wheel after the hooks were installed again", 120, -120);
    engine.apply(config(&exe, Some(&exe)));
    p.wheel("wheel over an app on the exception list", 120, 120);
    engine.apply(config(&exe, None));

    // The Back button presses Ctrl+F9 and never reaches the window.
    clear_field();
    if p.mouse("side button", &[mouse(MOUSEEVENTF_XDOWN, 1), mouse(MOUSEEVENTF_XUP, 1)]) {
        wait(300);
        let seen = GOT.lock().unwrap().clone();
        let ok = seen.contains(&Got::Key(0x78)) && seen.contains(&Got::Key(0x11)) && !seen.contains(&Got::SideButton);
        p.check("Back button mapped to Ctrl+F9", ok, format!("the window got {seen:?}"));
    }

    // A doubled click: the second press comes 0 ms after the release.
    clear_field();
    let click = [mouse(MOUSEEVENTF_LEFTDOWN, 0), mouse(MOUSEEVENTF_LEFTUP, 0)];
    if p.mouse("click debounce", &[click, click].concat()) {
        let presses = GOT.lock().unwrap().iter().filter(|g| **g == Got::LeftDown).count();
        p.check("a doubled click", presses == 1, format!("two clicks sent, the window got {presses}"));
    }

    // Stopped: nothing is hooked, and nothing is changed any more.
    drop(engine);
    let gone = svc_input::status();
    p.check("after stop", !gone.keyboard && !gone.mouse, hub());
    p.wheel("wheel after stop", 120, 120);
    clear_field();
    if let Some(inputs) = typing(";sig ") {
        if p.keys("snippet after stop", &inputs) {
            let text = field_text();
            p.check("snippet after stop", text == ";sig ", format!("the field holds {text:?}"));
        }
    }

    // Killed: a child of the probe hooks the wheel, is seen to, and is killed without a chance to clean up.
    match std::env::current_exe().and_then(|exe| Command::new(exe).arg("--hold").stdout(Stdio::piped()).spawn()) {
        Ok(mut child) => {
            let mut line = String::new();
            let _ = BufReader::new(child.stdout.take().unwrap()).read_line(&mut line);
            println!("     child {}: {}", child.id(), line.trim());
            p.wheel("wheel while the child's hook lives", 120, -120);
            let _ = child.kill();
            let _ = child.wait();
            wait(300);
            p.wheel("wheel after the child was killed", 120, 120);
        }
        Err(err) => p.check("child", false, format!("could not start: {err}")),
    }

    if caps() != caps_before && p.in_front() {
        println!("     Caps Lock was toggled by a failed step: put back");
        unsafe { SendInput(&tap(0x14), std::mem::size_of::<INPUT>() as i32) };
    }
    if before.hwnd() != 0 && before.hwnd() != own {
        println!("window in front before, restored: {:?}", before.restore());
    }
    println!("{} step(s) skipped", p.skipped);
    p.all && p.skipped == 0
}

/// `--hold`: hooks the wheel and waits to be killed.
fn hold() {
    let engine = Engine::start(|_| {});
    engine.apply(Config { invert_vertical: true, probe: true, ..Default::default() });
    println!("ready, {}", hub());
    let _ = std::io::stdout().flush();
    wait(20_000);
}

fn main() {
    // Whatever happens, the process (and with it every hook it holds) is gone after 30 seconds.
    std::thread::spawn(|| {
        wait(30_000);
        eprintln!("watchdog: 30 seconds, ending the process and its hooks");
        std::process::exit(3);
    });
    if std::env::args().any(|a| a == "--hold") {
        return hold();
    }
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("notch-input-probe");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: module.into(), lpszClassName: class, ..Default::default() });
        // Under the pointer where it is, so that the wheel and the clicks of the probe land on this window.
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            w!("input-probe (closes by itself)"),
            WS_POPUP | WS_CAPTION,
            at.x - 220,
            at.y - 75,
            440,
            130,
            None,
            None,
            Some(module.into()),
            None,
        )
        .expect("window");
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        // ES_AUTOHSCROLL
        let field_hwnd = CreateWindowExW(
            Default::default(),
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x80),
            0,
            0,
            client.right,
            client.bottom,
            Some(hwnd),
            None,
            Some(module.into()),
            None,
        )
        .expect("field");
        EDIT.store(field_hwnd.0 as isize, Ordering::SeqCst);
        EDIT_PROC.store(SetWindowLongPtrW(field_hwnd, GWLP_WNDPROC, field as *const () as isize), Ordering::SeqCst);
        let _ = ShowWindow(hwnd, SW_SHOW);

        let own = hwnd.0 as isize;
        let worker = std::thread::spawn(move || {
            let ok = exercise(own);
            let _ = PostMessageW(Some(HWND(own as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            ok
        });
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let ok = worker.join().unwrap_or(false);
        println!("probe window closed. {}", if ok { "ALL OK" } else { "NOT ALL OK" });
        std::process::exit(if ok { 0 } else { 1 });
    }
}
