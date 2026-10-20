// Restarts Lintel after a crash. The app starts `lintel.exe --watch <pid>` once it is
// the running instance; that process only sleeps on the app's process handle
// (no polling) and wakes when it ends. A normal quit, a kill by the installer or
// the user (exit code 0 or 1) ends the watch; a crash (an NTSTATUS code such as
// 0xC0000005, or a Rust panic's 101) starts the app again with `--restarted`.
// Five deaths in a row within a minute of starting and it gives up.

use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::CloseHandle;
use ::windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};

pub const WATCH: &str = "--watch";
pub const RESTARTED: &str = "--restarted";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Called from `setup` of the running instance. Not in debug builds, nor for a restarted run
/// (its watcher is already waiting on it).
pub fn start() {
    if cfg!(debug_assertions) || std::env::args().any(|a| a == RESTARTED) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args([WATCH, &std::process::id().to_string()])
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn();
    }
}

/// The watcher's whole life: `lintel.exe --watch <pid>`.
pub fn run(mut pid: u32) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut quick = 0;
    loop {
        let started = Instant::now();
        let Some(code) = wait_for(pid) else { return };
        let crashed = code == 101 || code >= 0xC000_0000;
        if !crashed {
            return;
        }
        quick = if started.elapsed() < Duration::from_secs(60) { quick + 1 } else { 1 };
        if quick >= 5 {
            return;
        }
        let Ok(child) = std::process::Command::new(&exe).arg(RESTARTED).creation_flags(CREATE_NO_WINDOW).spawn() else {
            return;
        };
        pid = child.id();
    }
}

/// Sleeps until the process ends; its exit code, or None when it cannot be watched.
fn wait_for(pid: u32) -> Option<u32> {
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        WaitForSingleObject(handle, INFINITE);
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code).is_ok();
        let _ = CloseHandle(handle);
        ok.then_some(code)
    }
}
