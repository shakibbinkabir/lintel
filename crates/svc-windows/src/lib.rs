// Windows — a shared service (docs/ARCHITECTURE.md §8). Not a feature: the
// features that need it list this crate in their own Cargo.toml.
//
// The windows that are open, the one in front, putting a window in front and
// handing the front back to the one that had it, the installed apps, and the
// test for a click on the on-screen keyboard. Nothing here runs on its own:
// every call works on the caller's thread and leaves nothing behind (no hook,
// no thread, no cache), so there is nothing to subscribe to and nothing to stop.
// Whoever wants to be told that the window in front changed asks svc-sysevents
// (`on_foreground`).

use std::path::Path;
use std::time::Duration;

use windows::core::{Interface, BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, PROPERTYKEY, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_ANY};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, IShellItem2, SHGetKnownFolderItem, KF_FLAG_DEFAULT,
    SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetAncestor, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, PostMessageW, SetForegroundWindow,
    ShowWindow, WindowFromPoint, GA_ROOT, GWL_EXSTYLE, GW_HWNDNEXT, GW_OWNER, SC_CLOSE, SW_RESTORE, WM_SYSCOMMAND,
    WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

// ── The windows that are open ─────────────────────────────────────────────────

/// One top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Win {
    pub hwnd: isize,
    pub title: String,
    pub pid: u32,
    /// The program's full path; empty when Windows does not tell (a protected process).
    pub exe: String,
    pub minimized: bool,
}

impl Win {
    /// The program's name without folder or extension: `notepad`.
    pub fn app(&self) -> &str {
        Path::new(&self.exe).file_stem().and_then(|s| s.to_str()).unwrap_or_default()
    }
}

fn handle(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// The full path of a running program. None for a process that ended or that
/// does not let itself be looked at.
pub fn exe_path(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut name = [0u16; 1024];
        let mut len = name.len() as u32;
        let read = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(name.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        read.ok().map(|_| String::from_utf16_lossy(&name[..len as usize]))
    }
}

fn describe(hwnd: HWND) -> Win {
    let mut text = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut text) }.max(0) as usize;
    let pid = pid_of(hwnd);
    Win {
        hwnd: hwnd.0 as isize,
        title: String::from_utf16_lossy(&text[..len]),
        pid,
        exe: exe_path(pid).unwrap_or_default(),
        minimized: unsafe { IsIconic(hwnd).as_bool() },
    }
}

/// What Alt+Tab shows: visible, not a tool window, nobody's dialog (unless it
/// asks for a taskbar button) and not cloaked (another virtual desktop, a
/// suspended store app).
fn switchable(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let wants_button = style & WS_EX_APPWINDOW.0 != 0;
        if style & WS_EX_TOOLWINDOW.0 != 0 || (style & WS_EX_NOACTIVATE.0 != 0 && !wants_button) {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) && !wants_button {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
        cloaked == 0
    }
}

/// The windows Alt+Tab would show, front to back, each with a title.
pub fn windows() -> Vec<Win> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe { (*(lparam.0 as *mut Vec<HWND>)).push(hwnd) };
        true.into()
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    all.into_iter().filter(|w| switchable(*w)).map(describe).filter(|w| !w.title.is_empty()).collect()
}

/// The window in front, whatever it is (a tool window, one of the app's own). None between two windows.
pub fn front() -> Option<Win> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_invalid()).then(|| describe(hwnd))
}

/// Asks the window to close, as its own × does (it may ask to save first).
pub fn close(hwnd: isize) {
    unsafe {
        let _ = PostMessageW(Some(handle(hwnd)), WM_SYSCOMMAND, WPARAM(SC_CLOSE as usize), LPARAM(0));
    }
}

// ── The window in front: take it, hand it back ────────────────────────────────

/// The front has changed hands when Windows says so, which is a moment after it was asked.
fn in_front(hwnd: HWND) -> bool {
    for _ in 0..10 {
        if unsafe { GetForegroundWindow() } == hwnd {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Puts a window in front (restored if it was minimised) and says whether it
/// now is: the answer is read back, never taken from `SetForegroundWindow`,
/// which Windows refuses to a process that is neither in front nor the last
/// to have had or sent input. Refused, it is asked again as the last one to
/// have sent input (a mouse event that moves nothing), then from inside the
/// input queue of the window in front. Never with a pressed Alt: the window in
/// front would take it for its menu key.
pub fn activate(hwnd: isize) -> bool {
    let hwnd = handle(hwnd);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        if in_front(hwnd) {
            return true;
        }
        let nudge =
            INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: MOUSEEVENTF_MOVE, ..Default::default() } } };
        SendInput(&[nudge], std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
        if in_front(hwnd) {
            return true;
        }
        let (me, other) = (GetCurrentThreadId(), GetWindowThreadProcessId(GetForegroundWindow(), None));
        let _ = AttachThreadInput(me, other, true);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = AttachThreadInput(me, other, false);
        in_front(hwnd)
    }
}

/// The window that had the keyboard before one of the app's windows took it.
/// `remember()` first, `activate(own)` next, `give_back(own)` when done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Before {
    hwnd: isize,
    /// A handle is given to another window once its own has closed: the thread tells them apart.
    thread: u32,
}

/// Notes the window in front, to hand the keyboard back to it later.
pub fn remember() -> Before {
    let hwnd = unsafe { GetForegroundWindow() };
    Before { hwnd: hwnd.0 as isize, thread: unsafe { GetWindowThreadProcessId(hwnd, None) } }
}

impl Before {
    /// 0 when no window was in front.
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// Still the same window, on screen and not minimised.
    pub fn still_there(&self) -> bool {
        let hwnd = handle(self.hwnd);
        unsafe {
            self.hwnd != 0
                && IsWindow(Some(hwnd)).as_bool()
                && GetWindowThreadProcessId(hwnd, None) == self.thread
                && IsWindowVisible(hwnd).as_bool()
                && !IsIconic(hwnd).as_bool()
        }
    }

    /// Hands the keyboard back, and says whether the remembered window is in
    /// front again. Only while `holder` (the window that took it) is still the
    /// one in front: if the user has clicked another window meanwhile, that one
    /// keeps the keyboard and nothing is done.
    pub fn give_back(&self, holder: isize) -> bool {
        let front = unsafe { GetForegroundWindow() }.0 as isize;
        if front == self.hwnd && self.hwnd != 0 {
            return true;
        }
        front == holder && self.still_there() && activate(self.hwnd)
    }
}

// ── Installed apps ────────────────────────────────────────────────────────────

/// One entry of the Start menu's list of apps (the shell's AppsFolder, which
/// holds the shortcuts of both Start menus and the packaged apps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Its AppUserModelID: what does not change when the app moves. Start it
    /// with `shell:AppsFolder\<id>` (`svc_shell::open`), draw it with `svc_shell::icon(id, px)`.
    pub id: String,
    pub name: String,
    /// The program a desktop app's shortcut leads to; empty for a packaged app.
    pub target: String,
}

/// System.Link.TargetParsingPath.
const LINK_TARGET: PROPERTYKEY =
    PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0xb9b4b3fc_2b51_4a42_b5d8_324146afcf25), pid: 2 };

unsafe fn taken(text: windows::core::Result<PWSTR>) -> String {
    let Ok(text) = text else { return String::new() };
    let out = unsafe { text.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(text.0 as *const _)) };
    out
}

unsafe fn named(item: &IShellItem, form: SIGDN) -> String {
    unsafe { taken(item.GetDisplayName(form)) }
}

/// The Start menu also lists what an installer left beside its app.
fn not_an_app(name: &str, target: &str) -> bool {
    let (name, target) = (name.to_lowercase(), target.to_lowercase());
    let paper = [".url", ".chm", ".txt", ".htm", ".html", ".pdf", ".rtf", ".ini", ".website"];
    name.contains("uninstall") || target.starts_with("http") || paper.iter().any(|ext| target.ends_with(ext))
}

unsafe fn scan() -> windows::core::Result<Vec<App>> {
    unsafe {
        let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
        let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
        let mut apps = Vec::new();
        loop {
            let mut one = [None];
            let mut got = 0u32;
            if items.Next(&mut one, Some(&mut got)).is_err() || got == 0 {
                break;
            }
            let Some(item) = one[0].take() else { break };
            let (name, id) = (named(&item, SIGDN_NORMALDISPLAY), named(&item, SIGDN_PARENTRELATIVEPARSING));
            let target = item.cast::<IShellItem2>().map(|i| taken(i.GetString(&LINK_TARGET))).unwrap_or_default();
            if !name.is_empty() && !id.is_empty() && !not_an_app(&name, &target) {
                apps.push(App { id, name, target });
            }
        }
        Ok(apps)
    }
}

/// Every installed app, in the shell's order. A few hundred milliseconds of
/// shell work on a thread of its own: call it from a blocking thread, never on
/// a keystroke.
pub fn apps() -> Vec<App> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| unsafe {
                let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let apps = scan().unwrap_or_default();
                if com.is_ok() {
                    CoUninitialize();
                }
                apps
            })
            .join()
            .unwrap_or_default()
    })
}

// ── The on-screen keyboard ────────────────────────────────────────────────────
//
// Someone who types by clicking keys on the on-screen keyboard makes a click
// for every character. Whatever reads a click outside its window, or another
// window coming to the front, as "the user has left" asks here first.

/// The classic On-Screen Keyboard, the touch keyboard of Windows 10, and the
/// host of the touch keyboard and the emoji panel of Windows 11.
const KEYBOARDS: [&str; 3] = ["osk.exe", "tabtip.exe", "textinputhost.exe"];

/// Whether a program, by path or by file name, is one of the on-screen keyboards.
pub fn is_keyboard_exe(exe: &str) -> bool {
    let name = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    KEYBOARDS.iter().any(|k| name.eq_ignore_ascii_case(k))
}

/// The window belongs to an on-screen keyboard: its coming to the front is not the user leaving.
pub fn is_onscreen_keyboard(hwnd: isize) -> bool {
    exe_path(pid_of(handle(hwnd))).is_some_and(|exe| is_keyboard_exe(&exe))
}

/// A click at this point (physical screen pixels) pressed a key of an
/// on-screen keyboard. The window on top at the point decides, read at once and
/// never remembered; one of the app's own windows on top does not hide what is
/// under it (some let clicks through, which cannot be seen from here), so the
/// first window below that is not the app's decides instead.
pub fn is_onscreen_keyboard_point(x: i32, y: i32) -> bool {
    let point = POINT { x, y };
    unsafe {
        let hit = GetAncestor(WindowFromPoint(point), GA_ROOT);
        if hit.is_invalid() {
            return false;
        }
        let me = GetCurrentProcessId();
        if pid_of(hit) != me {
            return is_onscreen_keyboard(hit.0 as isize);
        }
        let mut next = GetWindow(hit, GW_HWNDNEXT);
        while let Ok(under) = next {
            let mut rect = RECT::default();
            let clickable = IsWindowVisible(under).as_bool()
                && pid_of(under) != me
                && GetWindowLongW(under, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0 == 0
                && GetWindowRect(under, &mut rect).is_ok()
                && (rect.left..rect.right).contains(&x)
                && (rect.top..rect.bottom).contains(&y);
            if clickable {
                return is_onscreen_keyboard(under.0 as isize);
            }
            next = GetWindow(under, GW_HWNDNEXT);
        }
        false
    }
}

// ── Virtual desktops ──────────────────────────────────────────────────────────

/// The virtual desktops, left to right: the name the user gave each (None while
/// it still has the default one) and the index of the one in front. Read from
/// the registry on the call (Explorer keeps it there), so nothing watches it.
/// One desktop with no name when Explorer has not written the list yet.
pub fn desktops() -> (Vec<Option<String>>, usize) {
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops";
    fn read(sub: &str, value: &str) -> Option<Vec<u8>> {
        let (sub, value): (Vec<u16>, Vec<u16>) = (sub.encode_utf16().chain([0]).collect(), value.encode_utf16().chain([0]).collect());
        let (sub, value) = (windows::core::PCWSTR(sub.as_ptr()), windows::core::PCWSTR(value.as_ptr()));
        let mut len = 0u32;
        unsafe {
            RegGetValueW(HKEY_CURRENT_USER, sub, value, RRF_RT_ANY, None, None, Some(&mut len)).ok().ok()?;
            let mut buf = vec![0u8; len as usize];
            RegGetValueW(HKEY_CURRENT_USER, sub, value, RRF_RT_ANY, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)).ok().ok()?;
            buf.truncate(len as usize);
            Some(buf)
        }
    }
    // A GUID in memory order (three little-endian numbers, then 8 bytes) as the registry spells it.
    let guid = |b: &[u8]| {
        let n = |r: std::ops::Range<usize>| b[r].iter().rev().fold(String::new(), |s, x| s + &format!("{x:02X}"));
        format!("{{{}-{}-{}-{}-{}}}", n(0..4), n(4..6), n(6..8), b[8..10].iter().map(|x| format!("{x:02X}")).collect::<String>(), b[10..16].iter().map(|x| format!("{x:02X}")).collect::<String>())
    };
    let ids: Vec<String> = read(KEY, "VirtualDesktopIDs").map(|b| b.chunks_exact(16).map(guid).collect()).unwrap_or_default();
    if ids.is_empty() {
        return (vec![None], 0);
    }
    let now = read(KEY, "CurrentVirtualDesktop").filter(|b| b.len() >= 16).map(|b| guid(&b));
    let at = now.and_then(|g| ids.iter().position(|i| *i == g)).unwrap_or(0);
    let names = ids
        .iter()
        .map(|id| {
            let b = read(&format!(r"{KEY}\Desktops\{id}"), "Name")?;
            let w: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
            Some(String::from_utf16_lossy(&w)).filter(|n| !n.is_empty())
        })
        .collect();
    (names, at)
}
