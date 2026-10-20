// Which apps hold the microphone open. Windows records it for the privacy page, under
// HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone:
// one subkey per packaged app, and under `NonPackaged` one per desktop exe (its path with `#` for
// `\`). While an app records, `LastUsedTimeStart` is set and `LastUsedTimeStop` is 0. The registry is
// only ever read; `RegNotifyChangeKeyValue` wakes the watcher on a change, so nothing polls.

use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegNotifyChangeKeyValue, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE};

const STORE: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone");
// The camera keeps the same record under `webcam`.
const CAMERA: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\webcam");

fn open(parent: HKEY, path: PCWSTR, access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(parent, path, None, access, &mut key) }.is_ok().then_some(key)
}

fn subkeys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        let ok = unsafe {
            RegEnumKeyExW(key, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
        };
        if ok.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    out
}

fn qword(key: HKEY, name: PCWSTR) -> u64 {
    let mut v = 0u64;
    let mut size = 8u32;
    let ok = unsafe { RegQueryValueExW(key, name, None, None, Some(&mut v as *mut u64 as *mut u8), Some(&mut size)) };
    if ok.is_ok() && size == 8 { v } else { 0 }
}

/// Is this app recording now? Start was written and Stop is still 0.
fn recording(parent: HKEY, name: &str) -> bool {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let Some(key) = open(parent, PCWSTR(wide.as_ptr()), KEY_READ) else { return false };
    let on = qword(key, w!("LastUsedTimeStart")) > 0 && qword(key, w!("LastUsedTimeStop")) == 0;
    unsafe { _ = RegCloseKey(key) };
    on
}

/// "Microsoft.WindowsCamera_8wekyb3d8bbwe" -> "WindowsCamera"; "C:#Apps#Teams.exe" -> "Teams".
fn nice(name: &str, packaged: bool) -> String {
    if packaged {
        let base = name.split('_').next().unwrap_or(name);
        base.rsplit('.').next().unwrap_or(base).to_string()
    } else {
        let file = name.rsplit('#').next().unwrap_or(name);
        file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string()
    }
}

/// The apps with the microphone open now, other than Lintel itself (its dictation is announced
/// by its own HUD, not as a stranger). Sorted, without repeats.
pub fn in_use() -> Vec<String> {
    holders(STORE, std::env::current_exe().ok().map(|p| p.to_string_lossy().replace('\\', "#").to_lowercase()))
}

/// The apps with the camera open now.
pub fn camera_in_use() -> Vec<String> {
    holders(CAMERA, None)
}

/// The same with this program in it: the probe checks that the difference is this program alone.
pub fn everyone() -> Vec<String> {
    holders(STORE, None)
}

/// `me`: one desktop program left out, by its registry name in lower case.
fn holders(path: PCWSTR, me: Option<String>) -> Vec<String> {
    let Some(store) = open(HKEY_CURRENT_USER, path, KEY_READ) else { return Vec::new() };
    let mut out = Vec::new();
    for name in subkeys(store) {
        if name == "NonPackaged" {
            let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
            let Some(loose) = open(store, PCWSTR(wide.as_ptr()), KEY_READ) else { continue };
            for app in subkeys(loose) {
                if me.as_deref() != Some(app.to_lowercase().as_str()) && recording(loose, &app) {
                    out.push(nice(&app, false));
                }
            }
            unsafe { _ = RegCloseKey(loose) };
        } else if recording(store, &name) {
            out.push(nice(&name, true));
        }
    }
    unsafe { _ = RegCloseKey(store) };
    out.sort();
    out.dedup();
    out
}

/// A watcher thread; dropping it ends the thread.
pub struct Watch {
    stop: Arc<AtomicIsize>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe { _ = SetEvent(HANDLE(self.stop.load(Relaxed) as *mut _)) };
        if let Some(j) = self.join.take() {
            _ = j.join();
        }
        unsafe { _ = CloseHandle(HANDLE(self.stop.load(Relaxed) as *mut _)) };
    }
}

/// Calls `on` with the apps holding the microphone and the camera whenever either list changes (and once at the start).
pub fn watch(on: impl Fn(Vec<String>, Vec<String>) + Send + 'static) -> Option<Watch> {
    let stop = unsafe { CreateEventW(None, true, false, None) }.ok()?;
    let stop_raw = Arc::new(AtomicIsize::new(stop.0 as isize));
    let raw = stop.0 as isize;
    let join = std::thread::spawn(move || {
        let stop = HANDLE(raw as *mut _);
        let Some(store) = open(HKEY_CURRENT_USER, STORE, KEY_READ | KEY_NOTIFY) else { return };
        let Ok(woken) = (unsafe { CreateEventW(None, false, false, None) }) else { return };
        // A PC whose camera was never used has no such key: only the microphone then wakes it.
        let cam = open(HKEY_CURRENT_USER, CAMERA, KEY_READ | KEY_NOTIFY);
        let mut last: Option<(Vec<String>, Vec<String>)> = None;
        loop {
            // Armed before reading, so a change in between is not lost.
            let flags = REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME;
            let armed = unsafe { RegNotifyChangeKeyValue(store, true, flags, Some(woken), true) };
            if let Some(cam) = cam {
                unsafe { _ = RegNotifyChangeKeyValue(cam, true, flags, Some(woken), true) };
            }
            let now = (in_use(), camera_in_use());
            if last.as_ref() != Some(&now) {
                on(now.0.clone(), now.1.clone());
                last = Some(now);
            }
            if armed.is_err() {
                break;
            }
            if unsafe { WaitForMultipleObjects(&[woken, stop], false, INFINITE) } != WAIT_OBJECT_0 {
                break;
            }
            // A burst of writes (start, stop, several apps) settles into one reading.
            if unsafe { WaitForSingleObject(stop, 150) } == WAIT_OBJECT_0 {
                break;
            }
        }
        unsafe {
            _ = RegCloseKey(store);
            if let Some(cam) = cam {
                _ = RegCloseKey(cam);
            }
            _ = CloseHandle(woken);
        }
    });
    Some(Watch { stop: stop_raw, join: Some(join) })
}
