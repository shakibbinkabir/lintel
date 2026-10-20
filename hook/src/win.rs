//! The little bit of Win32 the relay needs: who we are, and who is on the other
//! end of the pipe.
//!
//! Named pipes live in a machine-wide namespace, so `\\.\pipe\lintel-<name>` can
//! be created by *any* account that gets there first. Two defences, both cheap:
//! the pipe name carries our SID, and once connected we check the server process
//! really belongs to us before sending anything.

use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::CONNECT_TIMEOUT;

/// `ERROR_PIPE_BUSY` — every instance is serving someone else right now. This is
/// the one error worth retrying: the server exists and a slot will free up.
const ERROR_PIPE_BUSY: i32 = 231;

/// `SECURITY_IDENTIFICATION` (winbase.h). Without this quality of service,
/// whoever serves the pipe may impersonate us at the full SecurityImpersonation
/// level: a service account that took the name before Lintel could then act as
/// us. This backs up `pipe_server_is_same_user`; Lintel never impersonates the
/// relay, so it loses nothing. (Ported from upstream `windows/hook/src/win.rs`,
/// pull request 426.)
const SECURITY_IDENTIFICATION: u32 = 1 << 16;

/// `\\.\pipe\lintel-<sid>`. The SID keeps two accounts on the same machine from
/// ever meeting on the same pipe; the name falls back to the user name only if
/// the SID cannot be read at all, which should not happen.
pub fn pipe_path() -> String {
    let key = current_user_sid()
        .unwrap_or_else(|| std::env::var("USERNAME").unwrap_or_else(|_| "user".into()));
    format!(r"\\.\pipe\lintel-{key}")
}

/// Opens the pipe at `path`: the app's is `pipe_path()`, a test names its own.
/// Retries only while the server is busy: any other error means there is
/// nothing to talk to, and waiting would only delay Claude Code.
pub fn connect_to(path: &str) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    let mut options = std::fs::OpenOptions::new();
    // security_qos_flags adds SECURITY_SQOS_PRESENT by itself.
    options.read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION);
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match options.open(path) {
            Ok(file) => {
                let handle = HANDLE(file.as_raw_handle());
                // Somebody else's server on our pipe name gets nothing from us.
                return pipe_server_is_same_user(handle).then_some(file);
            }
            Err(err) => {
                if err.raw_os_error() != Some(ERROR_PIPE_BUSY) || Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
}

/// A job holding a child process and everything it starts, so a timeout can end
/// the whole tree: killing the child alone leaves its own children running.
pub struct Tree(HANDLE);

impl Tree {
    pub fn of(child: &std::process::Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        // SAFETY: plain Win32 calls on a handle we own and the child's live handle.
        unsafe {
            let tree = Tree(CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?);
            AssignProcessToJobObject(tree.0, HANDLE(child.as_raw_handle())).ok()?;
            Some(tree)
        }
    }

    pub fn kill(&self) {
        // SAFETY: our own job handle.
        let _ = unsafe { windows::Win32::System::JobObjects::TerminateJobObject(self.0, 1) };
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // Closing does not end the processes: only `kill` does.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The processes we run under, nearest first (the shell that ran us, Claude
/// Code, its shell, the terminal…), at most 16, from one process snapshot.
/// Sent with every event so the app finds the session's window even once we
/// have exited, which an event nobody waits for has by the time it looks.
/// (After upstream pull request 607.)
pub fn ancestors() -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let mut parents = std::collections::HashMap::new();
    // SAFETY: a snapshot handle we own, walked with a sized entry, then closed.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return Vec::new() };
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    let mut out = Vec::new();
    let mut current = std::process::id();
    while out.len() < 16 {
        let Some(&parent) = parents.get(&current) else { break };
        // PID 0 ends the tree; a reused PID could loop.
        if parent == 0 || parent == current || out.contains(&parent) {
            break;
        }
        out.push(parent);
        current = parent;
    }
    out
}

/// The process that ran us, held open so its end is seen even if Windows
/// reuses its number. A prompt answered in the terminal ends it (Claude Code
/// ends the shell between us), and nothing is left to answer.
pub struct Parent(HANDLE);

impl Parent {
    /// None when it cannot be opened: then the budget alone ends the wait.
    pub fn of(pid: u32) -> Option<Self> {
        use windows::Win32::System::Threading::PROCESS_SYNCHRONIZE;
        // SAFETY: plain Win32 call; the handle is ours and closed on drop.
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok().map(Parent) }
    }

    pub fn gone(&self) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: our own process handle; a zero timeout never blocks.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The SID of the account this process runs as, as `S-1-5-21-…`.
pub fn current_user_sid() -> Option<String> {
    unsafe { token_sid(GetCurrentProcess()) }
}

/// True when the process serving `handle` runs as the same user we do.
///
/// A failure to answer is treated as "not ours": refusing to talk to a pipe we
/// cannot vouch for costs one hook event, while trusting it could hand another
/// account on this machine the contents of every tool call.
pub fn pipe_server_is_same_user(handle: HANDLE) -> bool {
    let Some(mine) = current_user_sid() else { return false };
    unsafe {
        let mut pid = 0u32;
        if GetNamedPipeServerProcessId(handle, &mut pid).is_err() || pid == 0 {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let theirs = token_sid(process);
        let _ = CloseHandle(process);
        theirs.as_deref() == Some(mine.as_str())
    }
}

/// The user SID behind a process handle. `process` is borrowed, never closed.
unsafe fn token_sid(process: HANDLE) -> Option<String> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;

    // First call sizes the buffer, second fills it.
    let mut needed = 0u32;
    let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    if needed == 0 {
        let _ = CloseHandle(token);
        return None;
    }
    // TOKEN_USER holds a pointer: the buffer must be aligned for one, which a
    // byte vector is not promised to be.
    let mut buf = vec![0u64; needed as usize / 8 + 1];
    let ok = GetTokenInformation(
        token,
        TokenUser,
        Some(buf.as_mut_ptr().cast()),
        needed,
        &mut needed,
    )
    .is_ok();
    let _ = CloseHandle(token);
    if !ok {
        return None;
    }

    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let mut text = PWSTR::null();
    ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
    let sid = text.to_string().ok();
    let _ = LocalFree(Some(HLOCAL(text.0 as *mut _)));
    sid
}
