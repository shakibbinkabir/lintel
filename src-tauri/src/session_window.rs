// "Open terminal" brings the window a Claude Code session runs in to the front,
// instead of opening its folder in VS Code (Mac 0.1.0: jump to the terminal).
//
// The relay is a grandchild of Claude Code, which runs in a shell, which runs
// in the terminal or editor whose window we want. When the relay connects, its
// process is still alive, so walking up from it finds that window's process.
// What is found is kept per session ID, here in memory only, and looked up when
// the button is clicked. Where nothing is found — a classic console, whose
// window belongs to a conhost that is no ancestor, or Linux — the island falls
// back to VS Code as before.
//
// On Linux the same walk reads /proc. Which ancestor owns the window is only
// asked when the button is clicked (the display server answers then, not from
// the relay's thread), so every ancestor is kept, nearest first; on Windows
// only the one that owns a window.
//
// The OS calls live in platform/; this file is the logic, tested on fixtures.

use std::collections::HashMap;
use std::sync::Mutex;

pub use notch_core::win::Proc;

/// Never the window of a session: the desktop shell and the services that sit
/// at the top of every process tree. Reaching one of them means the terminal
/// window was not an ancestor (a classic console window belongs to conhost).

const WINDOWS_TREE_TOPS: &[&str] = &[
    "explorer.exe", "services.exe", "wininit.exe", "winlogon.exe", "svchost.exe",
    "smss.exe", "csrss.exe", "system", "sihost.exe", "userinit.exe",
];

/// How far up we look. A session sits a handful of levels below its window.
pub const MAX_DEPTH: usize = 16;

/// The ancestors of `start`, nearest first, stopping before the top of the
/// tree. `start` itself — the relay — is not included. A loop in the parent
/// links (a parent ID reused by a newer process) ends the walk.
pub fn ancestors(procs: &HashMap<u32, Proc>, start: u32) -> Vec<u32> {
    ancestors_below(procs, start, WINDOWS_TREE_TOPS)
}

fn ancestors_below(procs: &HashMap<u32, Proc>, start: u32, tops: &[&str]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut seen = vec![start];
    let mut current = start;
    while out.len() < MAX_DEPTH {
        let Some(proc) = procs.get(&current) else { break };
        let parent = proc.parent;
        if parent == 0 || seen.contains(&parent) {
            break;
        }
        let Some(up) = procs.get(&parent) else { break };
        if tops.contains(&up.exe.to_ascii_lowercase().as_str()) {
            break;
        }
        out.push(parent);
        seen.push(parent);
        current = parent;
    }
    out
}

/// The processes the relay listed itself (`lintel_ancestors`, nearest first),
/// cut before the top of the tree as `ancestors` is. One the snapshot no longer
/// has (the shell that ran the relay, already gone) is kept: only a known tree
/// top ends the list. (After upstream pull request 607.)
pub fn below_tops(procs: &HashMap<u32, Proc>, listed: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    for &pid in listed.iter().take(MAX_DEPTH) {
        let top = procs.get(&pid).is_some_and(|p| WINDOWS_TREE_TOPS.contains(&p.exe.to_ascii_lowercase().as_str()));
        if top || pid == 0 || out.contains(&pid) {
            break;
        }
        out.push(pid);
    }
    out
}

/// The nearest ancestor that owns a window: the terminal or the editor. The
/// Windows side walks `ancestors` itself (platform/windows.rs); this states the
/// rule the tests pin down.
/// Up to `count` lines of `path` from line `from` (1-based) for the session's
/// code view, only when the file lies inside the session's own folder and is a
/// plain file under 1 MB. Anything else is `None`: the view then shows the diff alone.
pub fn file_lines(cwd: &str, path: &str, from: usize, count: usize) -> Option<Vec<String>> {
    let root = std::fs::canonicalize(cwd).ok()?;
    let file = std::fs::canonicalize(path).ok()?;
    let meta = std::fs::metadata(&file).ok()?;
    if !file.starts_with(&root) || !meta.is_file() || meta.len() > 1_000_000 {
        return None;
    }
    let text = String::from_utf8(std::fs::read(&file).ok()?).ok()?;
    Some(text.lines().skip(from.saturating_sub(1)).take(count.min(60)).map(|l| l.chars().take(240).collect()).collect())
}

#[cfg(test)]
pub fn window_owner(procs: &HashMap<u32, Proc>, start: u32, has_window: impl Fn(u32) -> bool) -> Option<u32> {
    ancestors_below(procs, start, WINDOWS_TREE_TOPS).into_iter().find(|pid| has_window(*pid))
}

/// Which of a process's windows to bring forward: the one whose title names
/// the session's folder (an editor with several projects open), else the first.
pub fn pick_window<'a, W>(windows: &'a [(W, String)], folder: &str) -> Option<&'a W> {
    let folder = folder.to_lowercase();
    let named = (!folder.is_empty())
        .then(|| windows.iter().find(|(_, title)| title.to_lowercase().contains(&folder)))
        .flatten();
    named.or_else(|| windows.first()).map(|(w, _)| w)
}

/// The last folder name of a path, either separator.
pub fn folder_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default()
}

// ── Sessions seen so far ──────────────────────────────────────────────────────

/// Session ID → the processes that may own its window, nearest first: the one
/// that does on Windows, every ancestor on Linux. Empty for a session whose
/// window could not be found, so it is not looked for on every event. Small:
/// an old session is dropped once there are more than this many.
const MAX_SESSIONS: usize = 64;

static SESSIONS: Mutex<Vec<(String, Vec<u32>)>> = Mutex::new(Vec::new());

/// A session ID arrives in a hook payload: only what Claude Code sends (a
/// UUID) is kept, at most 128 characters.
fn valid_session(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn remember(session: &str, mut owners: Vec<u32>) {
    if !valid_session(session) {
        return;
    }
    owners.truncate(MAX_DEPTH);
    let mut list = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(s, _)| s != session);
    list.push((session.to_string(), owners));
    let excess = list.len().saturating_sub(MAX_SESSIONS);
    list.drain(..excess);
}

/// True once the session has been looked at, window found or not.
pub fn known(session: &str) -> bool {
    let list = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    list.iter().any(|(s, _)| s == session)
}

/// The processes that may own the session's window, nearest first, if any.
pub fn lookup(session: &str) -> Option<Vec<u32>> {
    let list = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    list.iter().find(|(s, _)| s == session).map(|(_, pids)| pids.clone()).filter(|pids| !pids.is_empty())
}

pub fn forget(session: &str) {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(s, _)| s != session);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(entries: &[(u32, u32, &str)]) -> HashMap<u32, Proc> {
        entries
            .iter()
            .map(|(pid, parent, exe)| (*pid, Proc { parent: *parent, exe: exe.to_string() }))
            .collect()
    }

    #[test]
    fn windows_terminal_is_found_above_the_shells() {
        // WindowsTerminal → pwsh → claude (node) → bash → lintel-hook
        let procs = tree(&[
            (4, 0, "System"),
            (100, 4, "explorer.exe"),
            (200, 100, "WindowsTerminal.exe"),
            (300, 200, "pwsh.exe"),
            (400, 300, "node.exe"),
            (500, 400, "bash.exe"),
            (600, 500, "lintel-hook.exe"),
        ]);
        assert_eq!(ancestors_below(&procs, 600, WINDOWS_TREE_TOPS), [500, 400, 300, 200]);
        assert_eq!(window_owner(&procs, 600, |pid| pid == 200 || pid == 100), Some(200));
    }

    #[test]
    fn a_list_from_the_relay_is_cut_at_the_tree_top_and_keeps_the_gone() {
        // bash (500) already gone, claude, a shell started from the Start menu, explorer.
        let procs = tree(&[(4, 0, "System"), (100, 4, "explorer.exe"), (300, 100, "powershell.exe"), (400, 300, "claude.exe")]);
        assert_eq!(below_tops(&procs, &[500, 400, 300, 100, 4]), [500, 400, 300]);
        assert!(below_tops(&procs, &[]).is_empty());
        assert_eq!(below_tops(&procs, &[500, 0, 400]), [500]);
    }

    #[test]
    fn vs_code_is_found_through_its_pty_host() {
        let procs = tree(&[
            (100, 1, "explorer.exe"),
            (210, 100, "Code.exe"),
            (220, 210, "Code.exe"),
            (300, 220, "powershell.exe"),
            (400, 300, "node.exe"),
            (600, 400, "lintel-hook.exe"),
        ]);
        assert_eq!(window_owner(&procs, 600, |pid| pid == 210), Some(210));
    }

    #[test]
    fn a_classic_console_finds_nothing_rather_than_the_desktop() {
        // cmd.exe's window belongs to conhost, which is not an ancestor; the
        // walk must stop at explorer instead of picking the taskbar.
        let procs = tree(&[
            (100, 1, "explorer.exe"),
            (300, 100, "cmd.exe"),
            (400, 300, "node.exe"),
            (600, 400, "lintel-hook.exe"),
        ]);
        assert_eq!(window_owner(&procs, 600, |pid| pid == 100), None);
    }

    #[test]
    fn a_relay_already_gone_or_a_loop_ends_the_walk() {
        assert!(ancestors(&tree(&[]), 600).is_empty());
        let looped = tree(&[(1, 2, "a.exe"), (2, 1, "b.exe"), (3, 1, "lintel-hook.exe")]);
        assert_eq!(ancestors(&looped, 3), [1, 2]);
        let deep: Vec<(u32, u32, &str)> = (1..100).map(|i| (i, i + 1, "x.exe")).collect();
        assert_eq!(ancestors(&tree(&deep), 1).len(), MAX_DEPTH);
    }


    #[test]
    fn the_window_named_after_the_project_wins() {
        let windows = vec![
            (1, "notes — Visual Studio Code".to_string()),
            (2, "app.ts — lintel — Visual Studio Code".to_string()),
        ];
        assert_eq!(pick_window(&windows, "lintel"), Some(&2));
        assert_eq!(pick_window(&windows, "other"), Some(&1));
        assert_eq!(pick_window(&windows, ""), Some(&1));
        assert_eq!(pick_window::<i32>(&[], "x"), None);
        assert_eq!(folder_name(r"C:\Users\me\lintel\"), "lintel");
        assert_eq!(folder_name("/home/me/proj"), "proj");
    }

    #[test]
    fn sessions_are_remembered_by_id_and_only_plausible_ids_are_kept() {
        remember("s-test-1", vec![42]);
        remember("s-test-1", vec![43, 44]);
        assert_eq!(lookup("s-test-1"), Some(vec![43, 44]));
        remember("not a session/../x", vec![7]);
        assert!(!known("not a session/../x"));
        forget("s-test-1");
        assert!(!known("s-test-1"));

        // A session without a window is settled, with nothing to show.
        remember("s-test-2", Vec::new());
        assert!(known("s-test-2"));
        assert_eq!(lookup("s-test-2"), None);
        forget("s-test-2");

        // Only the latest sessions are kept. (One test: the list is shared.)
        for i in 0..(MAX_SESSIONS + 5) {
            remember(&format!("s-cap-{i}"), vec![1]);
        }
        assert!(!known("s-cap-0"));
        assert!(known(&format!("s-cap-{}", MAX_SESSIONS + 4)));
        for i in 0..(MAX_SESSIONS + 5) {
            forget(&format!("s-cap-{i}"));
        }
    }
}
