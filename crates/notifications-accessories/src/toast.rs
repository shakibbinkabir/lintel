// Notification pop-ups read as they appear. Windows lets only a signed, packaged
// app listen for notifications (mirror.rs); this is the free way for everyone:
// when the toast window of the shell shows (is uncloaked), its text is read through UI
// Automation. Event-driven: a WinEvent hook waits on its own thread, a reader
// thread wakes for each shown window, nothing is polled and nothing runs while
// the `Watch` is not held. Windows' Focus / Do Not Disturb hides the toasts, so
// then there is simply nothing to read.
//
// What is read is handed on and forgotten here: never stored, never logged.

use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, SetWinEventHook, TreeScope_Descendants, UnhookWinEvent, HWINEVENTHOOK, UIA_TextControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetClassNameW, GetMessageW, GetWindowThreadProcessId, PostThreadMessageW, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_UNCLOAKED, MSG, WINEVENT_OUTOFCONTEXT, WM_QUIT,
};

use crate::mirror::Toast;

/// The shell paints its toasts in a window of this class, owned by this program.
const CLASS: &str = "Windows.UI.Core.CoreWindow";
const VIEW: &str = "FlexibleToastView";
const HOST: &str = "ShellExperienceHost.exe";

/// Where the hook's callback leaves the handle of a window that showed.
static SHOWN: Mutex<Option<Sender<isize>>> = Mutex::new(None);

/// Dropping it removes the hook and ends both threads.
pub struct Watch {
    hook_thread: u32,
    threads: Vec<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.hook_thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        // The reader ends when the sender is gone.
        *SHOWN.lock().unwrap() = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

unsafe extern "system" fn shown(_: HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
    if hwnd.0.is_null() {
        return;
    }
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
    if String::from_utf16_lossy(&class[..len]) != CLASS {
        return;
    }
    if let Some(tx) = SHOWN.lock().unwrap().as_ref() {
        let _ = tx.send(hwnd.0 as isize);
    }
}

/// Tells of every toast that shows from now on, on the reader thread.
pub fn watch(tell: impl Fn(Toast) + Send + 'static) -> Result<Watch, String> {
    let (tx, rx) = channel::<isize>();
    *SHOWN.lock().unwrap() = Some(tx);
    let (ready, started) = channel::<Result<u32, String>>();

    let hook = std::thread::spawn(move || unsafe {
        // The toast window is not created for each toast: the shell uncloaks it, or renames it when one follows another.
        let hook = SetWinEventHook(EVENT_OBJECT_UNCLOAKED, EVENT_OBJECT_UNCLOAKED, None, Some(shown), 0, 0, WINEVENT_OUTOFCONTEXT);
        let renamed = SetWinEventHook(EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE, None, Some(shown), 0, 0, WINEVENT_OUTOFCONTEXT);
        if hook.0.is_null() || renamed.0.is_null() {
            let _ = UnhookWinEvent(hook);
            let _ = UnhookWinEvent(renamed);
            let _ = ready.send(Err("Windows did not accept the event hook".into()));
            return;
        }
        let _ = ready.send(Ok(GetCurrentThreadId()));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        let _ = UnhookWinEvent(hook);
        let _ = UnhookWinEvent(renamed);
    });
    let reader = std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let auto: Option<IUIAutomation> = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.ok();
        let mut recent: Vec<(Vec<String>, Instant)> = Vec::new();
        let mut next_id = 0u32;
        while let Ok(raw) = rx.recv() {
            let Some(auto) = &auto else { continue };
            for texts in read(auto, raw) {
                // The same pop-up is announced more than once while it slides in.
                recent.retain(|(_, at)| at.elapsed() < Duration::from_secs(4));
                if recent.iter().any(|(t, _)| *t == texts) {
                    continue;
                }
                recent.push((texts.clone(), Instant::now()));
                next_id += 1;
                tell(toast(next_id, texts));
            }
        }
        drop(auto);
        unsafe { CoUninitialize() };
    });

    match started.recv() {
        Ok(Ok(hook_thread)) => Ok(Watch { hook_thread, threads: vec![hook, reader] }),
        other => {
            *SHOWN.lock().unwrap() = None;
            let _ = hook.join();
            let _ = reader.join();
            Err(other.map_or_else(|_| "the hook thread ended".into(), |r| r.err().unwrap_or_default()))
        }
    }
}

/// True when the window belongs to the shell's toast host.
fn is_host(raw: isize) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(HWND(raw as *mut _), Some(&mut pid)) };
    notch_core::win::exe_name_of_pid(pid).is_some_and(|e| e.eq_ignore_ascii_case(HOST))
}

/// The texts of each toast showing in the window, in tree order (app, title, body lines).
/// The content is drawn a moment after the window shows, so an empty tree is asked again a few times.
pub fn read(auto: &IUIAutomation, raw: isize) -> Vec<Vec<String>> {
    if !is_host(raw) {
        return vec![];
    }
    for _ in 0..6 {
        let toasts = texts(auto, raw).unwrap_or_default();
        if !toasts.is_empty() {
            return toasts;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    vec![]
}

fn texts(auto: &IUIAutomation, raw: isize) -> windows::core::Result<Vec<Vec<String>>> {
    unsafe {
        let root = auto.ElementFromHandle(HWND(raw as *mut _))?;
        let all = root.FindAll(TreeScope_Descendants, &auto.CreateTrueCondition()?)?;
        let mut out: Vec<Vec<String>> = Vec::new();
        for i in 0..all.Length()? {
            let el = all.GetElement(i)?;
            // Windows 11 wraps each pop-up in a view of this class; a layout without it is one toast.
            if el.CurrentClassName()?.to_string() == VIEW {
                out.push(Vec::new());
                continue;
            }
            if el.CurrentControlType()? != UIA_TextControlTypeId {
                continue;
            }
            let name = el.CurrentName()?.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
            if name.is_empty() {
                continue;
            }
            if out.is_empty() {
                out.push(Vec::new());
            }
            out.last_mut().unwrap().push(name);
        }
        out.retain(|t| !t.is_empty());
        Ok(out)
    }
}

fn clip(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

/// Windows 11 (probe, build 26300): TextBlocks in order app, title, body; the buttons are not text.
// debt: Windows 10 is not seen on this machine; a layout without a title shows the app alone.
fn toast(id: u32, texts: Vec<String>) -> Toast {
    let mut it = texts.into_iter();
    let app = it.next().unwrap_or_default();
    let title = it.next().unwrap_or_default();
    Toast { id, app: clip(&app, 100), aumid: String::new(), title: clip(&title, 200), body: clip(&it.collect::<Vec<_>>().join(" "), 1000) }
}


fn plain(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Brings the app a pop-up came from forward: its window by program name, then by title; or
/// starts it from its Start menu shortcut. The pop-up names the app and nothing else.
// debt: a Store app that is not running has no shortcut file here, so it is not started.
pub fn open_app(app: &str) -> bool {
    let want = plain(app);
    if want.chars().count() < 3 {
        return false;
    }
    let procs = notch_core::win::process_table();
    let windows = notch_core::win::top_windows();
    let named = |exe_match: bool| {
        windows.iter().find(|(_, pid, title)| {
            let exe = procs.get(pid).map(|p| plain(p.exe.trim_end_matches(".exe"))).unwrap_or_default();
            !exe.starts_with("lintel") && if exe_match { exe == want } else { plain(title).contains(&want) }
        })
    };
    if let Some((hwnd, ..)) = named(true).or_else(|| named(false)) {
        return notch_core::win::bring_forward(*hwnd);
    }
    let roots = [std::env::var_os("APPDATA"), std::env::var_os("ProgramData")];
    for root in roots.into_iter().flatten() {
        let mut dirs = vec![(std::path::PathBuf::from(root).join(r"Microsoft\Windows\Start Menu\Programs"), 0)];
        while let Some((dir, depth)) = dirs.pop() {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() && depth < 3 {
                    dirs.push((path, depth + 1));
                } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) && plain(&path.file_stem().unwrap_or_default().to_string_lossy()) == want {
                    notch_core::win::open_url(&path.to_string_lossy());
                    return true;
                }
            }
        }
    }
    false
}
