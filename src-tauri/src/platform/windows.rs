// Windows: Win32 for the island window, the cursor and session windows.

use std::os::windows::io::RawHandle;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager, WebviewWindow};

use ::windows::core::{BOOL, PCWSTR, PWSTR};
use ::windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, HWND, LPARAM, POINT};
use ::windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use ::windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use ::windows::Win32::System::Ole::RevokeDragDrop;
use ::windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use ::windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayoutList, MapVirtualKeyExW, SendInput, ToUnicodeEx, HKL, INPUT, INPUT_0,
    INPUT_MOUSE, MAPVK_VK_TO_VSC, MOUSEEVENTF_MOVE, MOUSEINPUT, VK_CONTROL, VK_LBUTTON, VK_MENU, VK_SHIFT,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowLongW, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow, SetWindowLongW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use notch_core::win::{find_on_path, process_table, top_windows};

use super::keyhook;
use crate::session_window;

/// File name of the Claude Code relay.
pub use notch_brand::HOOK_EXE;

// ── Processes ─────────────────────────────────────────────────────────────────

/// Where the Codex CLI may be, best first: %PATH% (codex.exe or npm's
/// codex.cmd), then npm's global folder and the Volta / Bun / pnpm ones.
/// Rust quotes the one fixed argument safely for a `.cmd` (see find_on_path).
pub fn codex_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = find_on_path("codex").into_iter().collect();
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(appdata) = var("APPDATA") {
        out.push(appdata.join("npm").join("codex.cmd"));
    }
    if let Some(local) = var("LOCALAPPDATA") {
        out.push(local.join("Volta").join("bin").join("codex.exe"));
        out.push(local.join("pnpm").join("codex.cmd"));
    }
    if let Some(home) = var("USERPROFILE") {
        out.push(home.join(".bun").join("bin").join("codex.exe"));
        out.push(home.join(".local").join("bin").join("codex.exe"));
    }
    // The Codex desktop app's own CLI, one opaque folder per release: newest first.
    // (After upstream pull request 715.)
    if let Some(bin) = var("LOCALAPPDATA").map(|l| l.join("OpenAI").join("Codex").join("bin")) {
        let mut bundled: Vec<_> = std::fs::read_dir(bin)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join("codex.exe"))
            .filter_map(|exe| Some((exe.metadata().ok().filter(|m| m.is_file())?.modified().ok(), exe)))
            .collect();
        bundled.sort_by(|a, b| b.0.cmp(&a.0));
        out.extend(bundled.into_iter().map(|(_, exe)| exe));
    }
    out.retain(|p| p.is_file());
    out
}

// The display name only feeds Mascot's greeting (identity.rs). The calls are
// declared here by hand, with their documented C signatures, so they need no
// extra `windows` crate features.
#[link(name = "secur32")]
extern "system" {
    fn GetUserNameExW(name_format: i32, name_buffer: *mut u16, size: *mut u32) -> u8;
}

#[link(name = "netapi32")]
extern "system" {
    fn NetUserGetInfo(server: *const u16, user: *const u16, level: u32, buffer: *mut *mut u8) -> u32;
    fn NetApiBufferFree(buffer: *mut core::ffi::c_void) -> u32;
}

/// EXTENDED_NAME_FORMAT::NameDisplay.
const NAME_DISPLAY: i32 = 3;

/// The account's display name ("Louis Raille"): the directory's for a domain
/// or Entra account, else the local account's "Full name", else nothing.
pub fn user_full_name() -> Option<String> {
    directory_display_name().or_else(local_full_name).filter(|n| !n.trim().is_empty())
}

fn directory_display_name() -> Option<String> {
    let mut buf = [0u16; 256];
    let mut size = buf.len() as u32;
    // On success `size` is the length copied, without the terminating null.
    let ok = unsafe { GetUserNameExW(NAME_DISPLAY, buf.as_mut_ptr(), &mut size) } != 0;
    if !ok || size as usize > buf.len() {
        return None;
    }
    String::from_utf16(&buf[..size as usize]).ok()
}

fn local_full_name() -> Option<String> {
    let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty())?;
    let wide: Vec<u16> = user.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info: *mut u8 = std::ptr::null_mut();
    // Level 10 is USER_INFO_10 { name, comment, usr_comment, full_name }, four
    // wide-string pointers; any user may read it about themselves.
    let status = unsafe { NetUserGetInfo(std::ptr::null(), wide.as_ptr(), 10, &mut info) };
    if status != 0 || info.is_null() {
        return None;
    }
    unsafe {
        let full = *(info as *const *const u16).add(3);
        let text = if full.is_null() {
            None
        } else {
            let mut len = 0usize;
            while len < 256 && *full.add(len) != 0 {
                len += 1;
            }
            String::from_utf16(std::slice::from_raw_parts(full, len)).ok()
        };
        NetApiBufferFree(info.cast());
        text
    }
}

// ── Cursor ────────────────────────────────────────────────────────────────────

/// Cursor position in physical screen pixels.
pub fn cursor_physical() -> Option<(f64, f64)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x as f64, p.y as f64))
}

/// True while the left mouse button is held — the only signal we get that a
/// drag might be in flight before it reaches the window.
pub fn left_button_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
}

// ── Island window ─────────────────────────────────────────────────────────────

fn hwnd_of(win: &WebviewWindow) -> Option<HWND> {
    let raw = win.hwnd().ok()?.0 as isize;
    if raw == 0 {
        return None;
    }
    Some(HWND(raw as *mut _))
}

/// Keeps the settings window from showing the "no drop" cursor.
///
/// wry installs its drop target by walking the webview's child windows **once**,
/// when the webview is created. WebView2 creates `Chrome_RenderWidgetHostHWND`
/// later and registers its own target on it; being the innermost window, that one
/// wins, and since the page has no HTML5 drop handler it refuses everything.
/// Revoking it makes OLE fall through to wry's target. The island is the other
/// way round: it takes drops through WebView2's own target (see webview_drop.rs),
/// so its windows are left alone.
///
/// Cheap and idempotent, so it is simply re-run whenever a drag might be starting.
pub fn unblock_webview_drops(app: &AppHandle) {
    let Some(win) = app.get_webview_window("settings") else { return };
    let Some(hwnd) = hwnd_of(&win) else { return };
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(revoke_render_widget), LPARAM(0));
    }
}

unsafe extern "system" fn revoke_render_widget(hwnd: HWND, _: LPARAM) -> BOOL {
    let mut name = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut name) };
    if len > 0 {
        let class = String::from_utf16_lossy(&name[..len as usize]);
        if class == "Chrome_RenderWidgetHostHWND" {
            let _ = unsafe { RevokeDragDrop(hwnd) };
        }
    }
    true.into()
}

/// Clicks on the island never take the keyboard from the window the user types
/// in (WS_EX_NOACTIVATE). Asked through the window's own flag: tao writes the
/// extended styles again from its flags each time one of them changes, which
/// the click-through toggle does many times a second, so a style set behind its
/// back (as this used to be) was gone the first time the pointer moved.
pub fn make_non_activating(win: &WebviewWindow) {
    let _ = win.set_focusable(false);
}

/// Click-through on or off, then WS_EX_TOOLWINDOW (out of Alt-Tab) put back:
/// tao rewrites the extended styles from its own flags on each toggle, which
/// drops a style set behind its back. Only writes when the bit is missing.
pub fn set_click_through(win: &WebviewWindow, ignore: bool) {
    let _ = win.set_ignore_cursor_events(ignore);
    if let Some(hwnd) = hwnd_of(win) {
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
            if style as u32 & WS_EX_TOOLWINDOW.0 == 0 {
                SetWindowLongW(hwnd, GWL_EXSTYLE, style | WS_EX_TOOLWINDOW.0 as i32);
            }
        }
    }
}

/// Shows a window without making it the active one: `show()` activates even a
/// window that cannot be focused, which took the keyboard from the app in front
/// whenever a feature's overlay (the desktop companion) came up.
pub fn show_inactive(win: &WebviewWindow) {
    match hwnd_of(win) {
        Some(hwnd) => unsafe {
            let _ = ::windows::Win32::UI::WindowsAndMessaging::ShowWindow(hwnd, ::windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE);
        },
        None => {
            let _ = win.show();
        }
    }
}

/// The island's window exists: it never takes the keyboard by appearing, and
/// the hook that reads its keys under the pointer knows which window it is.
pub fn island_ready(app: &AppHandle, win: &WebviewWindow) {
    make_non_activating(win);
    if let Some(hwnd) = hwnd_of(win) {
        keyhook::install(app, hwnd);
    }
}

// ── The keyboard: taken by a click or a shortcut, handed back on folding ──────
// (docs/ARCHITECTURE.md §4, "Keyboard"; the keys of an island that does not
// hold it are in keyhook.rs)

/// The window to hand the keyboard back to: its handle and the thread that owns
/// it (a handle can be given to another window once its own has closed).
static KEYBOARD_FROM: Mutex<Option<(isize, u32)>> = Mutex::new(None);
/// True from a `take_keyboard` that worked to `give_keyboard_back`, or to the
/// user giving the keyboard to another window (`keyboard_was_taken_away`).
static HOLDS_KEYBOARD: AtomicBool = AtomicBool::new(false);

/// Remembers the window in front as the one to go back to, unless it is the
/// island itself or there is none.
fn remember_front(island: HWND) {
    unsafe {
        let front = GetForegroundWindow();
        if front != island && !front.is_invalid() {
            let thread = GetWindowThreadProcessId(front, None);
            *KEYBOARD_FROM.lock().unwrap() = Some((front.0 as isize, thread));
        }
    }
}

/// Where the keyboard goes when Settings closes, noted when it was shown.
static SETTINGS_FROM: Mutex<Option<(isize, u32)>> = Mutex::new(None);

/// Settings is about to be shown and to take the keyboard: notes who has it.
/// Another program's window, or, when Settings is opened from the island while
/// it holds the keyboard, the window the island took it from. Opened from the
/// tray, nobody is known (the menu's own window is in front): nothing is noted.
pub fn settings_opening(settings: &WebviewWindow) {
    let Some(hwnd) = hwnd_of(settings) else { return };
    unsafe {
        let front = GetForegroundWindow();
        if front == hwnd {
            return; // already in front: what was noted when it came up still holds
        }
        let mut pid = 0u32;
        let thread = GetWindowThreadProcessId(front, Some(&mut pid));
        *SETTINGS_FROM.lock().unwrap() = if !front.is_invalid() && pid != std::process::id() {
            Some((front.0 as isize, thread))
        } else if HOLDS_KEYBOARD.load(Ordering::Relaxed) {
            *KEYBOARD_FROM.lock().unwrap()
        } else {
            None
        };
    }
}

/// Settings is about to be hidden. If it is in front, the keyboard goes back to
/// the window noted when it was shown, rather than to whatever Windows picks
/// next: that was the island, hidden, and the keys went nowhere.
pub fn settings_closing(settings: &WebviewWindow) {
    let to = SETTINGS_FROM.lock().unwrap().take();
    let Some(hwnd) = hwnd_of(settings) else { return };
    unsafe {
        if GetForegroundWindow() == hwnd {
            if let Some((raw, _)) = to.filter(|(raw, thread)| still_there(*raw, *thread)) {
                let _ = SetForegroundWindow(HWND(raw as *mut _));
            }
        }
    }
}

/// A window remembered earlier is still the same one, on screen and not minimised.
fn still_there(raw: isize, thread: u32) -> bool {
    let w = HWND(raw as *mut _);
    unsafe {
        IsWindow(Some(w)).as_bool()
            && GetWindowThreadProcessId(w, None) == thread
            && IsWindowVisible(w).as_bool()
            && !IsIconic(w).as_bool()
    }
}

/// The island takes the keyboard (a click inside it, a shortcut that opens it,
/// a view with a text field) and remembers who had it. True when it has it.
/// Asked again while it holds it, it only makes sure the page has the keys.
/// On the main thread, and never from inside a web view's own callback: the
/// focus has to go into the web view, which does not answer from in there.
pub fn take_keyboard(win: &WebviewWindow) -> bool {
    let Some(hwnd) = hwnd_of(win) else { return false };
    // A window that refuses activation cannot be the active one.
    let _ = win.set_focusable(true);
    unsafe {
        if GetForegroundWindow() != hwnd {
            remember_front(hwnd);
            if !bring_to_front(hwnd) {
                KEYBOARD_FROM.lock().unwrap().take();
                let _ = win.set_focusable(false);
                return false;
            }
        }
    }
    HOLDS_KEYBOARD.store(true, Ordering::Relaxed);
    // The window is in front; the keys have to reach the page inside it.
    let _ = AsRef::<tauri::Webview>::as_ref(win).set_focus();
    true
}

/// Makes `hwnd` the window in front, and says whether it now is. Windows lets
/// a process do that only while it is in front itself or was the last one to
/// get or to send input; a click on the island went to the web view's own
/// process, not to this one, and with the keys in the web view even the
/// island in front is sometimes refused when it passes the front on. A mouse
/// event that moves nothing makes this process the last to have sent input.
/// tao's `set_focus` presses Alt for the same end: a browser or an editor in
/// front takes a bare Alt for its menu key.
unsafe fn bring_to_front(hwnd: HWND) -> bool {
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        if GetForegroundWindow() == hwnd {
            return true;
        }
        let nudge = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: MOUSEEVENTF_MOVE, ..Default::default() } },
        };
        SendInput(&[nudge], std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
        if GetForegroundWindow() == hwnd {
            return true;
        }
        // Still refused: share the input queue of the window in front for the moment of the call.
        let front = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let attached = front != 0 && front != me && AttachThreadInput(me, front, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(me, front, false);
        }
        GetForegroundWindow() == hwnd
    }
}

/// True when the window in front belongs to one of these processes (a session's terminal).
pub fn in_front_of(owners: &[u32]) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
    pid != 0 && owners.contains(&pid)
}

/// True once, when the island held the keyboard and the user has given it to
/// another window (a click outside, Alt+Tab). The island is then an ordinary
/// non-activating window again, and a click inside it takes the keyboard anew.
/// Asked on every tick of the cursor poll.
pub fn keyboard_was_taken_away(win: &WebviewWindow) -> bool {
    if !HOLDS_KEYBOARD.load(Ordering::Relaxed) {
        return false;
    }
    let Some(hwnd) = hwnd_of(win) else { return false };
    let front = unsafe { GetForegroundWindow() };
    // No window in front is a moment between two windows, not a choice.
    if front == hwnd || front.is_invalid() {
        return false;
    }
    HOLDS_KEYBOARD.store(false, Ordering::Relaxed);
    KEYBOARD_FROM.lock().unwrap().take();
    let _ = win.set_focusable(false);
    true
}

/// The island lets go of the keyboard (it folded, hid, or the pointer left it
/// with no text being typed) and hands it back to the window it took it from,
/// see `hand_back_to`. Without one to go back to, the window Windows would
/// pick gets it: a hidden island must never be left holding the keyboard.
pub fn give_keyboard_back(win: &WebviewWindow) {
    HOLDS_KEYBOARD.store(false, Ordering::Relaxed);
    let from = KEYBOARD_FROM.lock().unwrap().take();
    if let Some(hwnd) = hwnd_of(win) {
        unsafe {
            let (front, island) = (GetForegroundWindow().0 as isize, hwnd.0 as isize);
            let target = hand_back_to(front, island, from, still_there)
                .or_else(|| (front == island).then(next_window).flatten());
            if let Some(target) = target {
                // Asked the way the take is, and checked: Windows may refuse even
                // the window in front (seen after a keyboard taken in the middle
                // of an Alt chord), and a call that is merely trusted leaves the
                // island in front, folded, holding the keyboard for nobody.
                if !bring_to_front(HWND(target as *mut _)) && !next_window().is_some_and(|next| bring_to_front(HWND(next as *mut _))) {
                    crate::log::line("the keyboard could not be handed back: Windows keeps the island in front");
                }
            }
        }
    }
    let _ = win.set_focusable(false);
}

/// The first window down the stack that could take the keyboard and is not
/// one of ours: what Windows itself picks when the window in front closes.
fn next_window() -> Option<isize> {
    top_windows().into_iter().map(|(hwnd, pid, _)| (hwnd, pid)).find_map(|(raw, pid)| {
        let w = HWND(raw as *mut _);
        let style = unsafe { GetWindowLongW(w, GWL_EXSTYLE) } as u32;
        let takes_keys = style & (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) == 0;
        (pid != std::process::id() && takes_keys && unsafe { !IsIconic(w).as_bool() }).then_some(raw)
    })
}

/// Who gets the keyboard back, if anyone. Only while the island itself is still
/// in front: when the user already clicked another window, that one keeps the
/// keyboard. And never a window that has closed, was minimised or hidden since.
fn hand_back_to(
    front: isize,
    island: isize,
    from: Option<(isize, u32)>,
    still_there: impl Fn(isize, u32) -> bool,
) -> Option<isize> {
    let (hwnd, thread) = from?;
    (front == island && still_there(hwnd, thread)).then_some(hwnd)
}

// ── Session windows ("Open terminal") ─────────────────────────────────────────
//
// See session_window.rs: the relay's ancestors lead to the terminal or editor
// window a Claude Code session runs in.

/// The process on the other end of a relay connection.
pub fn pipe_client_pid(handle: RawHandle) -> Option<u32> {
    let mut pid = 0u32;
    unsafe { GetNamedPipeClientProcessId(HANDLE(handle), &mut pid).ok()? };
    (pid != 0).then_some(pid)
}

// ── Relay pipe access (ported from the upstream fork base's
// windows/src-tauri/src/platform/windows.rs, pull requests 426 and 390) ────────
//
// Without an explicit descriptor a named pipe gets the default one: full control
// for SYSTEM, administrators and the creator owner, and read access for Everyone
// and the anonymous account. Every instance of the relay pipe (pipe.rs) is
// created with PipeSecurity instead.

/// The pipe's SDDL: a protected DACL (`P`, nothing inherited), full access for
/// `sid` and for SYSTEM, no other entry. None unless `sid` has the `S-1-…`
/// shape: nothing else may slip into the string.
pub fn pipe_sddl(sid: &str) -> Option<String> {
    let rest = sid.strip_prefix("S-1-")?;
    let well_formed = rest.split('-').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    well_formed.then(|| format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"))
}

/// The security descriptor that reserves the relay pipe for one user.
pub struct PipeSecurity(PSECURITY_DESCRIPTOR);

// Memory allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW,
// owned by us alone and never modified afterwards: the listening task may move
// it from one thread to another.
unsafe impl Send for PipeSecurity {}
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    /// None for a malformed SID (see pipe_sddl) or a descriptor Windows refuses.
    pub fn for_user(sid: &str) -> Option<Self> {
        let sddl: Vec<u16> = pipe_sddl(sid)?.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .ok()?;
        }
        Some(Self(descriptor))
    }

    /// Calls `create` with a pointer to a SECURITY_ATTRIBUTES carrying this
    /// descriptor, the form `ServerOptions::create_with_security_attributes_raw`
    /// expects. The pointer is only valid during the call.
    pub fn with_attributes<R>(&self, create: impl FnOnce(*mut std::ffi::c_void) -> R) -> R {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        };
        create((&mut attributes as *mut SECURITY_ATTRIBUTES).cast())
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// A kernel object's DACL as SDDL, so tests check what Windows actually applied
/// to the pipe.
#[cfg(test)]
pub fn dacl_sddl(handle: RawHandle) -> Option<String> {
    use ::windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use ::windows::Win32::Security::DACL_SECURITY_INFORMATION;
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        GetSecurityInfo(
            HANDLE(handle),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&mut descriptor),
        )
        .ok()
        .ok()?;
        descriptor_sddl(descriptor, DACL_SECURITY_INFORMATION)
    }
}

/// `sid` as Windows writes it in SDDL: its alias when it has one (`SY`, `LA` for
/// the built-in Administrator…), `S-1-…` otherwise. This lets tests compare a
/// DACL read back with the current user, whoever that is.
#[cfg(test)]
pub fn sddl_trustee(sid: &str) -> Option<String> {
    use ::windows::Win32::Security::OWNER_SECURITY_INFORMATION;
    pipe_sddl(sid)?; // only a well-formed SID goes into the string below
    let sddl: Vec<u16> = format!("O:{sid}").encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .ok()?;
        descriptor_sddl(descriptor, OWNER_SECURITY_INFORMATION)?.strip_prefix("O:").map(str::to_string)
    }
}

/// `descriptor` as SDDL, limited to `parts`. Frees `descriptor`, which must come
/// from LocalAlloc.
#[cfg(test)]
unsafe fn descriptor_sddl(
    descriptor: PSECURITY_DESCRIPTOR,
    parts: ::windows::Win32::Security::OBJECT_SECURITY_INFORMATION,
) -> Option<String> {
    use ::windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    let mut text = PWSTR::null();
    let converted =
        ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, SDDL_REVISION_1, parts, &mut text, None)
            .is_ok();
    let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    if !converted {
        return None;
    }
    let sddl = text.to_string().ok();
    let _ = LocalFree(Some(HLOCAL(text.0 as *mut _)));
    sddl
}

/// The executable a process runs, as a full path with links resolved.
fn process_image(pid: u32) -> Option<PathBuf> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return None;
        }
        std::fs::canonicalize(String::from_utf16_lossy(&buf[..len as usize])).ok()
    }
}

/// True when `pid` runs the relay the app deployed (settings::hook_exe_path):
/// the only program allowed to raise an Allow / Deny card (pipe.rs). Anything
/// that cannot be told for sure reads as "not the relay".
pub fn is_deployed_relay(pid: u32) -> bool {
    let Ok(relay) = std::fs::canonicalize(crate::settings::hook_exe_path()) else { return false };
    process_image(pid).is_some_and(|image| same_file(&image, &relay))
}

/// Two canonical paths to one file: Windows paths compare without case.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

#[link(name = "advapi32")]
extern "system" {
    /// RtlGenRandom: the system's cryptographic generator.
    #[link_name = "SystemFunction036"]
    fn rtl_gen_random(buffer: *mut u8, len: u32) -> u8;
}

/// 128 random bits as hex, for an id nobody can guess. None if the system's
/// generator fails, which the caller must treat as "no id".
pub fn random_token() -> Option<String> {
    let mut bytes = [0u8; 16];
    let ok = unsafe { rtl_gen_random(bytes.as_mut_ptr(), bytes.len() as u32) } != 0;
    ok.then(|| bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The ancestors of a process, nearest first, below the desktop shell.
pub fn process_ancestors(pid: u32) -> Vec<u32> {
    session_window::ancestors(&process_table(), pid)
}

/// The first of `pids` that owns a window.
pub fn first_with_window(pids: &[u32]) -> Option<u32> {
    let windows = top_windows();
    pids.iter().copied().find(|pid| windows.iter().any(|(_, owner, _)| owner == pid))
}

/// What a session remembers: the ancestor that owns a window, or nothing.
pub fn window_owners(ancestors: &[u32]) -> Vec<u32> {
    // Never this app: a task the chat started runs under it.
    let ancestors: Vec<u32> = ancestors.iter().copied().filter(|pid| *pid != std::process::id()).collect();
    first_with_window(&ancestors)
        .or_else(|| ancestors.iter().find_map(|pid| console_window_owner(*pid)))
        .into_iter()
        .collect()
}

/// The processes the relay listed itself, cut below the desktop shell and the services.
pub fn below_tree_tops(listed: &[u32]) -> Vec<u32> {
    session_window::below_tops(&process_table(), listed)
}

// Declared by hand, like the calls above: no extra `windows` crate feature.
#[link(name = "kernel32")]
extern "system" {
    fn AttachConsole(pid: u32) -> i32;
    fn FreeConsole() -> i32;
    fn GetConsoleWindow() -> *mut core::ffi::c_void;
}

/// The console route to a session's window, where no ancestor owns one: a
/// PowerShell started from the Start menu has explorer.exe for a parent, and
/// Windows Terminal only hosts it as the default console, never an ancestor.
/// Attached to `pid`'s console for a moment, GetConsoleWindow gives a hidden
/// pseudo-console window whose root owner is the Windows Terminal window, or a
/// classic console's own window; its process is what the session remembers.
/// The console is process-wide: one caller at a time, and never while the app
/// has a console of its own (a dev build run from a terminal), which freeing
/// would lose. (After upstream pull request 607.)
fn console_window_owner(pid: u32) -> Option<u32> {
    use ::windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GA_ROOTOWNER};
    static CONSOLE: Mutex<()> = Mutex::new(());
    let _one = CONSOLE.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        if !GetConsoleWindow().is_null() || AttachConsole(pid) == 0 {
            return None;
        }
        let console = HWND(GetConsoleWindow());
        let _ = FreeConsole();
        if console.is_invalid() {
            return None;
        }
        let root = GetAncestor(console, GA_ROOTOWNER);
        let root = if root.is_invalid() { console } else { root };
        let mut owner = 0u32;
        GetWindowThreadProcessId(root, Some(&mut owner));
        (owner != 0 && owner != std::process::id() && IsWindowVisible(root).as_bool()).then_some(owner)
    }
}

/// A session's or an app's window to the front: restored when it is minimised, then taken the checked way
/// (`bring_to_front`). The plain `SetForegroundWindow` of `notch_core::win::bring_forward` is refused whenever this
/// process did not have the last input: after a global shortcut, and after a click whose island has already handed
/// the keyboard back on its way out. The session's own window then stayed behind and the editor opened instead.
/// (debt: `notch_core::win::bring_forward` itself should do this; `svc-media` still calls it.)
fn bring_forward(raw: isize) -> bool {
    let hwnd = HWND(raw as *mut _);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ::windows::Win32::UI::WindowsAndMessaging::ShowWindow(hwnd, ::windows::Win32::UI::WindowsAndMessaging::SW_RESTORE);
        }
        bring_to_front(hwnd)
    }
}

/// Brings `pid`'s window forward — the one titled after `folder` if there are
/// several — and says which one came.
pub fn focus_process_window(pid: u32, folder: &str) -> Option<isize> {
    let candidates: Vec<(isize, String)> = top_windows()
        .into_iter()
        .filter(|(_, owner, _)| *owner == pid)
        .map(|(hwnd, _, title)| (hwnd, title))
        .collect();
    session_window::pick_window(&candidates, folder).copied().filter(|hwnd| bring_forward(*hwnd))
}

/// The title of the window a session remembered, the one named after `folder` if its owner has several:
/// Claude Code writes a one-line summary of the session there (after upstream pull request 607).
// debt: in Windows Terminal this is the title of the tab in front, which is another session's when this one is behind.
pub fn session_window_title(owners: &[u32], folder: &str) -> Option<String> {
    let pid = *owners.first()?;
    let candidates: Vec<(isize, String)> =
        top_windows().into_iter().filter(|(_, owner, _)| *owner == pid).map(|(hwnd, _, title)| (hwnd, title)).collect();
    let hwnd = *session_window::pick_window(&candidates, folder)?;
    candidates.into_iter().find(|(h, _)| *h == hwnd).map(|(_, title)| title).filter(|t| !t.is_empty())
}

/// Brings forward the window a session remembered (`window_owners`): the window, when it came.
pub fn focus_session_window(owners: &[u32], folder: &str) -> Option<isize> {
    owners.first().and_then(|pid| focus_process_window(*pid, folder))
}

/// The apps a session can live in when that is not a terminal: the name the
/// page sends, the program whose window is brought forward, and the launcher on
/// PATH that opens a folder in it, when it has one. This is the whole list:
/// nothing outside it is ever brought forward or started by name, and nothing
/// in it comes from an agent's message.
const OWNERS: &[(&str, &str, Option<&str>)] = &[
    ("claude-desktop", "claude.exe", None),
    ("cursor", "Cursor.exe", Some("cursor")),
    // The `codex` on PATH is the agent itself: never started from here.
    ("codex", "Codex.exe", None),
    ("antigravity", "Antigravity.exe", Some("antigravity")),
];

pub fn is_owner(name: &str) -> bool {
    OWNERS.iter().any(|(owner, ..)| *owner == name)
}

/// Brings a session's app forward when it runs. When it does not: the Claude
/// app is started from where its installer puts it, an editor by its own
/// launcher on the session's folder (an existing folder, handed over as an
/// argument, never through a shell), and anything else is left alone.
pub fn open_owner(name: &str, folder: Option<&str>) -> bool {
    let Some((_, exe, launcher)) = OWNERS.iter().find(|(owner, ..)| *owner == name) else { return false };
    if name == "claude-desktop" {
        return open_claude_desktop();
    }
    if focus_app(exe) {
        return true;
    }
    let folder = folder.map(std::path::Path::new).filter(|p| p.is_absolute() && p.is_dir());
    match (launcher.and_then(find_on_path), folder) {
        (Some(bin), Some(dir)) => notch_core::win::no_console(Command::new(bin).arg(dir)).spawn().is_ok(),
        _ => false,
    }
}

/// Brings forward the window of a running app, by executable name.
fn focus_app(exe: &str) -> bool {
    let pids: Vec<u32> = process_table()
        .into_iter()
        .filter(|(_, p)| p.exe.eq_ignore_ascii_case(exe))
        .map(|(pid, _)| pid)
        .collect();
    top_windows()
        .into_iter()
        .find(|(_, owner, _)| pids.contains(owner))
        .is_some_and(|(hwnd, _, _)| bring_forward(hwnd))
}

/// The Claude desktop app: brought forward when it runs (Claude Code's own
/// `claude.exe` has no window of its own, so it is never the one picked),
/// started from where its installer puts it otherwise.
pub fn open_claude_desktop() -> bool {
    if focus_app("claude.exe") {
        return true;
    }
    let squirrel = std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("AnthropicClaude").join("claude.exe"));
    if let Some(exe) = squirrel.filter(|exe| exe.is_file()) {
        return Command::new(exe).spawn().is_ok();
    }
    // Installed from the Store (MSIX) there is no such file: the app answers to
    // its own registered protocol (upstream pull request 417). A fixed address.
    // debt: an install with neither gets Windows' "choose an app" question.
    notch_core::win::open_url("claude://");
    true
}

// ── Global shortcuts ──────────────────────────────────────────────────────────

/// The character Ctrl+Alt(+Shift)+`vk` types on one of the installed keyboard
/// layouts, if any. Windows reads Ctrl+Alt as AltGr, so registering such a
/// combination as a global shortcut would stop the user typing that character
/// (AltGr+E is € on most European layouts, AltGr+A is ą in Polish).
pub fn ctrl_alt_types(vk: u16, shift: bool) -> Option<String> {
    let mut layouts = [HKL::default(); 32];
    let count = unsafe { GetKeyboardLayoutList(Some(&mut layouts[..])) };
    let count = usize::try_from(count).unwrap_or(0).min(layouts.len());

    let mut state = [0u8; 256];
    state[VK_CONTROL.0 as usize] = 0x80;
    state[VK_MENU.0 as usize] = 0x80;
    if shift {
        state[VK_SHIFT.0 as usize] = 0x80;
    }
    for &layout in &layouts[..count] {
        let scan = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC, Some(layout)) };
        let mut buf = [0u16; 8];
        // Flag 0x4: leave the keyboard state alone, so a dead key met here
        // doesn't change what the user types next (Windows 10 1607 and later).
        let n = unsafe { ToUnicodeEx(u32::from(vk), scan, &state, &mut buf, 0x4, Some(layout)) };
        let typed = match n {
            // A dead key (´ ^ ¨…) is still something the user types with it.
            n if n < 0 => String::from_utf16_lossy(&buf[..1]),
            0 => continue,
            n => String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]),
        };
        if typed.chars().any(|c| !c.is_control()) {
            return Some(typed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pipe_sddl_grants_the_user_and_system_only() {
        let sid = "S-1-5-21-1004336348-1177238915-682003330-1001";
        assert_eq!(
            pipe_sddl(sid).as_deref(),
            Some("D:P(A;;GA;;;S-1-5-21-1004336348-1177238915-682003330-1001)(A;;GA;;;SY)")
        );
        assert_eq!(pipe_sddl("S-1-5-18").as_deref(), Some("D:P(A;;GA;;;S-1-5-18)(A;;GA;;;SY)"));
    }

    #[test]
    fn a_malformed_sid_never_reaches_the_sddl() {
        for bad in ["", "S-1-", "S-1-5-", "S-1-5--21", "s-1-5-21", "S-1-5-21 ", "WD", "S-1-5-21)(A;;GA;;;WD", "S-1-5-21-1001)(A;;GA;;;AN"] {
            assert_eq!(pipe_sddl(bad), None, "{bad:?}");
            assert!(PipeSecurity::for_user(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn windows_accepts_the_descriptor_for_the_current_user() {
        let sid = notch_core::win::user_sid().expect("the current user's SID");
        assert!(PipeSecurity::for_user(&sid).is_some());
        assert_eq!(sddl_trustee("S-1-5-18").as_deref(), Some("SY"));
        assert!(sddl_trustee(&sid).is_some());
        assert_eq!(sddl_trustee("S-1-5-21)(A;;GA;;;WD"), None);
    }

    #[test]
    fn only_the_deployed_relay_counts_as_the_relay() {
        // This test binary is a program of the user's, but not the relay.
        assert!(!is_deployed_relay(std::process::id()));
        // A process that does not exist.
        assert!(!is_deployed_relay(0));
        let image = process_image(std::process::id()).expect("our own image");
        assert!(same_file(&image, &std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()));
        assert!(same_file(std::path::Path::new(r"\?\C:\A\Lintel-Hook.exe"), std::path::Path::new(r"\?\c:\a\lintel-hook.EXE")));
        assert!(!same_file(std::path::Path::new(r"\?\C:\a\lintel-hook.exe"), std::path::Path::new(r"\?\C:\b\lintel-hook.exe")));
    }

    #[test]
    fn the_keyboard_goes_back_only_to_a_window_still_there_and_only_from_the_island() {
        let (island, editor, other) = (10isize, 20isize, 30isize);
        let there = |_: isize, _: u32| true;
        let gone = |_: isize, _: u32| false;
        // The island still has the keyboard and the editor is still open.
        assert_eq!(hand_back_to(island, island, Some((editor, 7)), there), Some(editor));
        // The user already clicked another window: nothing is taken from it.
        assert_eq!(hand_back_to(other, island, Some((editor, 7)), there), None);
        assert_eq!(hand_back_to(0, island, Some((editor, 7)), there), None);
        // The editor closed (or its handle now belongs to another window).
        assert_eq!(hand_back_to(island, island, Some((editor, 7)), gone), None);
        // The island never took the keyboard from anyone.
        assert_eq!(hand_back_to(island, island, None, there), None);
    }
}
