// Claude Code running inside WSL. Ported from upstream
// `windows/src-tauri/src/wsl.rs` (pull request 763), trimmed to the hooks and
// the paths; ideas from pull requests 61 and 58.
//
// A session in a WSL distro reads the distro's own ~/.claude/settings.json and
// cannot see the island's named pipe. It can start Windows programs (interop),
// and lintel-hook.exe is one: the hooks there run it, it talks to the island
// over the pipe and prints its answer, so Allow and Deny reach the session in
// WSL like any other.
//
// This module finds the places and writes the command (hooks.rs installs, with
// the same diff, backup and fingerprint as for Windows):
// * the distros, and which of them run: a stopped distro's files are never read,
//   as that would start it;
// * the Linux users with a ~/.claude, and their settings.json as Windows reaches
//   it (`\\wsl.localhost\<distro>\home\<user>\.claude\settings.json`);
// * the relay's path as the distro writes it (`/mnt/c/Users/…`);
// * a session's Linux path seen from Windows, for the diff's ↗ and Open
//   terminal's fallback (`/home/me/proj` → `\\wsl.localhost\Ubuntu\home\me\proj`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::platform;

/// Distros WSL keeps for Docker Desktop: nobody runs Claude Code there.
const SKIPPED: &[&str] = &["docker-desktop", "docker-desktop-data"];

/// A Linux user of a running distro who has a `~/.claude`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WslUser {
    pub name: String,
    pub settings_path: String,
    /// Lintel's hooks are in that settings.json.
    pub installed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WslDistro {
    pub name: String,
    pub running: bool,
    /// Only for a running distro.
    pub users: Vec<WslUser>,
}

/// What an install needs, once the names were checked against the real list.
pub struct Target {
    pub settings_path: PathBuf,
    /// The relay as this distro writes its path: `/mnt/c/Users/…/lintel-hook.exe`.
    pub relay: String,
}

/// A distro or user name that cannot lead anywhere else: it comes from the
/// settings window and ends up in a path.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
}

/// What `wsl.exe -l -q` prints: UTF-16 (CRLF), one distro a line; or UTF-8 when
/// `WSL_UTF8` is set. Docker's own distros are left out.
fn parse_distro_list(bytes: &[u8]) -> Vec<String> {
    let text = if bytes.contains(&0) {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(|line| line.trim().trim_end_matches(" (Default)").trim().to_string())
        .filter(|name| !name.is_empty() && !SKIPPED.contains(&name.as_str()) && is_safe_name(name))
        .collect()
}

/// `[automount] root = …` of a distro's /etc/wsl.conf; `/mnt/` by default.
fn parse_automount_root(wsl_conf: &str) -> String {
    let mut in_automount = false;
    for line in wsl_conf.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_automount = section.trim().eq_ignore_ascii_case("automount");
        } else if let Some((key, value)) = line.split_once('=').filter(|_| in_automount) {
            let value = value.trim().trim_matches('"');
            if key.trim().eq_ignore_ascii_case("root") && value.starts_with('/') {
                return format!("{}/", value.trim_end_matches('/'));
            }
        }
    }
    "/mnt/".to_string()
}

/// `C:\Users\me\x.exe` → `/mnt/c/Users/me/x.exe`. Nothing for a path that is not
/// on a drive letter.
fn to_wsl_path(windows_path: &str, automount_root: &str) -> Option<String> {
    let mut chars = windows_path.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let rest = chars.as_str().strip_prefix(':')?.strip_prefix(['\\', '/'])?.replace('\\', "/");
    if rest.is_empty() || rest.contains("//") || rest.split('/').any(|part| part == "..") {
        return None;
    }
    Some(format!("{automount_root}{}/{rest}", drive.to_ascii_lowercase()))
}

/// The hook command written in a distro's settings.json. The relay exits at once
/// when Lintel is closed; when it cannot start at all (interop off, the app
/// uninstalled) `|| true` keeps Claude Code from reporting a failing hook.
/// WSLENV hands it the variable that marks a headless run (main.rs `headless`).
pub fn relay_command(relay: &str, event: &str) -> String {
    let relay = format!("'{}'", relay.replace('\'', r"'\''"));
    format!("[ -x {relay} ] && WSLENV=\"CLAUDE_CODE_ENTRYPOINT:$WSLENV\" {relay} {event} || true")
}

/// `\\wsl.localhost\<distro>\home\me\proj` for `/home/me/proj`. Only a full Linux
/// path with nothing in it that could lead elsewhere.
fn unc_path(distro: &str, linux_path: &str) -> Option<PathBuf> {
    if !is_safe_name(distro) || !linux_path.starts_with('/') {
        return None;
    }
    if linux_path.contains(['\\', '\0']) || linux_path.split('/').any(|part| part == "..") {
        return None;
    }
    let mut path = root(distro);
    path.extend(linux_path.split('/').filter(|p| !p.is_empty() && *p != "."));
    Some(path)
}

fn root(distro: &str) -> PathBuf {
    PathBuf::from(format!(r"\\wsl.localhost\{distro}"))
}

fn settings_of(distro: &str, user: &str) -> PathBuf {
    root(distro).join("home").join(user).join(".claude").join("settings.json")
}

// ── Asking Windows ────────────────────────────────────────────────────────────

/// What `wsl.exe` prints, or nothing if it does not answer within 5 s (it never
/// should for `-l`, but a wedged WSL must not hold a click).
fn wsl_output(args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("wsl.exe");
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = platform::no_console(&mut command).spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // No WSL, or no distro: it says so in words, which are not distro names.
    if !status.success() {
        return None;
    }
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut child.stdout.take()?, &mut out).ok()?;
    Some(out)
}

/// Every distro and whether it runs. Nothing here starts one.
fn distros() -> Vec<(String, bool)> {
    let all = wsl_output(&["-l", "-q"]).map(|b| parse_distro_list(&b)).unwrap_or_default();
    let running = running_now();
    all.into_iter()
        .map(|name| {
            let up = running.contains(&name);
            (name, up)
        })
        .collect()
}

/// The running distros as `wsl.exe` last said, at most 30 s ago: a diff's ↗ asks.
static RUNNING: std::sync::Mutex<Option<(Instant, Vec<String>)>> = std::sync::Mutex::new(None);

fn running_distros() -> Vec<String> {
    let cached = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match cached {
        Some((at, list)) if at.elapsed() < Duration::from_secs(30) => list,
        _ => running_now(),
    }
}

fn running_now() -> Vec<String> {
    let list = wsl_output(&["-l", "--running", "-q"]).map(|b| parse_distro_list(&b)).unwrap_or_default();
    *RUNNING.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), list.clone()));
    list
}

/// The users of a running distro who have a ~/.claude.
fn users(distro: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root(distro).join("home")) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join(".claude").is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_safe_name(name))
        .collect();
    names.sort();
    names
}

fn automount_root(distro: &str) -> String {
    std::fs::read_to_string(root(distro).join("etc").join("wsl.conf"))
        .map(|text| parse_automount_root(&text))
        .unwrap_or_else(|_| "/mnt/".to_string())
}

/// The distros, and for each running one its users and whether Lintel's hooks
/// are in their settings.json. Read-only.
pub fn list() -> Vec<WslDistro> {
    distros()
        .into_iter()
        .map(|(name, running)| {
            let users = if running { users(&name) } else { Vec::new() };
            let users = users
                .into_iter()
                .map(|user| {
                    let path = settings_of(&name, &user);
                    let installed = crate::config_file::read(&path)
                        .ok()
                        .and_then(|b| crate::config_file::parse_json(b.as_deref(), "settings.json").ok())
                        .is_some_and(|v| crate::hooks::has_ours(&v));
                    WslUser { name: user, settings_path: path.to_string_lossy().to_string(), installed }
                })
                .collect();
            WslDistro { name, running, users }
        })
        .collect()
}

/// The settings.json and relay path for `distro` and `user`, after checking that
/// both are really there and the distro runs.
pub fn resolve(distro: &str, user: &str) -> Result<Target, String> {
    let known = is_safe_name(distro)
        && is_safe_name(user)
        && running_distros().iter().any(|d| d == distro)
        && users(distro).iter().any(|u| u == user);
    if !known {
        return Err(crate::i18n::tf(
            "{user} has no ~/.claude in {distro}, or {distro} is not running.",
            &[("user", user), ("distro", distro)],
        ));
    }
    let relay = to_wsl_path(&crate::settings::hook_exe_path().to_string_lossy(), &automount_root(distro))
        .ok_or_else(|| crate::i18n::t("The relay is not on a local drive, so WSL cannot run it."))?;
    Ok(Target { settings_path: settings_of(distro, user), relay })
}

// ── Commands (Settings › Agents) ──────────────────────────────────────────────
// They ask `wsl.exe` and read files over the distros' share: off the main thread.

#[tauri::command]
pub async fn wsl_hooks_list() -> Vec<WslDistro> {
    tauri::async_runtime::spawn_blocking(list).await.unwrap_or_default()
}

/// The diff the user has to look at before anything is written to a distro.
#[tauri::command]
pub async fn wsl_hooks_preview(distro: String, user: String, install: bool) -> Result<crate::hooks::HookPreview, String> {
    tauri::async_runtime::spawn_blocking(move || crate::hooks::wsl_preview(&distro, &user, install))
        .await
        .map_err(|e| e.to_string())?
}

/// Only ever from an explicit click, and only for the diff that was shown.
#[tauri::command]
pub async fn wsl_hooks_apply(distro: String, user: String, install: bool, fingerprint: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || crate::hooks::wsl_write(&distro, &user, install, &fingerprint))
        .await
        .map_err(|e| e.to_string())?
}

// ── A session's Linux path, opened from Windows ──────────────────────────────

/// Opens a path a session in WSL reported, as Open terminal (a folder) or the
/// diff's ↗ (a file) do for a Windows one: in VS Code (in the distro itself when
/// its Remote - WSL extension is there), else in Explorer. Only running distros
/// are looked in, and only an existing folder or file goes any further. A path
/// two running distros both have goes to the first. On a thread of its own:
/// `wsl.exe` and the share can take a moment, and the callers are sync commands.
pub fn open_in_editor(linux_path: &str, folder: bool) -> bool {
    let linux_path = linux_path.to_string();
    std::thread::spawn(move || open_now(&linux_path, folder));
    true
}

fn open_now(linux_path: &str, folder: bool) -> bool {
    let exists = |p: &Path| if folder { p.is_dir() } else { p.is_file() };
    let Some((distro, unc)) = running_distros()
        .into_iter()
        .find_map(|d| unc_path(&d, linux_path).filter(|p| exists(p)).map(|p| (d, p)))
    else {
        return false;
    };
    if let Some(code) = platform::find_on_path("code") {
        let args: Vec<OsString> = if remote_wsl_installed() {
            vec!["--remote".into(), format!("wsl+{distro}").into(), linux_path.into()]
        } else {
            vec![unc.clone().into()]
        };
        if platform::no_console(Command::new(code).args(args)).spawn().is_ok() {
            return true;
        }
    }
    let shown = if folder { Some(unc.as_path()) } else { unc.parent() };
    if let Some(dir) = shown {
        platform::reveal_folder(&dir.to_string_lossy());
    }
    false
}

/// VS Code's Remote - WSL is among its extensions (`ms-vscode-remote.remote-wsl-<version>`).
fn remote_wsl_installed() -> bool {
    let names = std::fs::read_dir(platform::home_dir().join(".vscode").join("extensions"));
    names.into_iter().flatten().flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .strip_prefix("ms-vscode-remote.remote-wsl-")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_exe_lists_distros_in_utf16_and_utf8() {
        let utf16: Vec<u8> = "Ubuntu\r\ndocker-desktop\r\nDebian (Default)\r\n".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(parse_distro_list(&utf16), ["Ubuntu", "Debian"]);
        assert_eq!(parse_distro_list(b"Arch\n\n"), ["Arch"]);
        assert!(parse_distro_list(b"").is_empty());
    }

    #[test]
    fn the_relay_is_written_as_the_distro_sees_it_and_never_fails() {
        assert_eq!(parse_automount_root("[automount]\nroot = /win  # drives\n"), "/win/");
        assert_eq!(parse_automount_root("[network]\nroot=/x\n"), "/mnt/");
        let exe = r"C:\Users\O'Neil\AppData\Local\lintel\bin\lintel-hook.exe";
        let relay = to_wsl_path(exe, "/mnt/").unwrap();
        assert_eq!(relay, "/mnt/c/Users/O'Neil/AppData/Local/lintel/bin/lintel-hook.exe");
        assert_eq!(to_wsl_path(r"\\server\share\x.exe", "/mnt/"), None);
        assert_eq!(to_wsl_path(r"C:\a\..\b.exe", "/mnt/"), None);
        let command = relay_command(&relay, "Stop");
        assert!(command.contains(r"'/mnt/c/Users/O'\''Neil/AppData/Local/lintel/bin/lintel-hook.exe' Stop || true"), "{command}");
        // The marker hooks.rs knows its entries by.
        assert!(command.contains(notch_brand::HOOK_BIN));
    }

    #[test]
    fn a_linux_path_becomes_a_path_on_the_distros_share_and_nothing_else() {
        assert_eq!(
            unc_path("Ubuntu", "/home/me//./proj"),
            Some(PathBuf::from(r"\\wsl.localhost\Ubuntu").join("home").join("me").join("proj"))
        );
        for bad in ["home/me", "", r"C:\Users", "/home/../etc", r"/a\b"] {
            assert_eq!(unc_path("Ubuntu", bad), None, "{bad:?}");
        }
        assert_eq!(unc_path("../x", "/home"), None);
    }

    #[test]
    fn the_hooks_go_in_beside_the_users_own_and_out_again() {
        use serde_json::json;
        let theirs = json!({ "model": "opus", "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "/home/me/notify.sh" }] }] } });
        let command = |event: &str| relay_command("/mnt/c/x/lintel-hook.exe", event);
        let after = crate::hooks::merged_with(&theirs, &command).unwrap();
        assert!(crate::hooks::has_ours(&after));
        assert_eq!(after["hooks"]["Stop"][0], theirs["hooks"]["Stop"][0]);
        assert_eq!(crate::hooks::without_ours(&after).unwrap(), theirs);
    }
}
