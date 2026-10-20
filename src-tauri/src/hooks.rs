// Claude Code hook installation.
//
// The rule from CLAUDE.md is strict and is followed to the letter:
// read %USERPROFILE%\.claude\settings.json (or CLAUDE_CONFIG_DIR's), take a dated backup, merge without
// touching anybody else's hooks, show the diff, and write only after an explicit
// click. Uninstall removes Lintel's entries and nothing else.
//
// The command is only the quoted exe path in forward slashes plus the event name:
// on Windows Claude Code runs hook commands through Git Bash, and anything with
// PowerShell or cmd in it breaks.
//
// Reading, the diff, the backup and the write itself live in config_file.rs,
// shared with every other agent's installer (agents.rs).

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager};
use crate::agents::{self, Shell};
use crate::config_file::{self, FileEdit};
use crate::{platform, settings};

/// Every event the island reacts to, with the hook timeout written to settings.json.
/// PermissionRequest waits for a human, so it gets the decision timeout + 10 s.
pub const HOOK_EVENTS: &[(&str, u64)] = &[
    ("SessionStart", 10),
    ("SessionEnd", 10),
    ("UserPromptSubmit", 10),
    ("PreToolUse", 10),
    ("PostToolUse", 10),
    ("PostToolUseFailure", 10),
    ("PermissionRequest", 120),
    ("Notification", 10),
    ("Stop", 10),
    ("StopFailure", 10),
    ("SubagentStart", 10),
    ("SubagentStop", 10),
];

/// Marker that identifies a Lintel entry inside settings.json.
const MARKER: &str = notch_brand::HOOK_BIN;

/// The hooks that start a turn. With the opt-in of Settings › Agents their
/// command carries `--launch`, and the relay then starts the app when it finds
/// it closed (hook/src/launch.rs). Never PermissionRequest or any other event.
const LAUNCH_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit"];
const LAUNCH_FLAG: &str = "--launch";
/// Written beside the relay: where this app is, for the relay to start it.
const APP_PATH_FILE: &str = "app-path";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatus {
    pub installed: bool,
    /// Lintel's status line relay (plan usage) is the one in settings.json.
    pub plan_relay_installed: bool,
    /// The installed hooks start the app when it is closed (`--launch`).
    pub auto_launch: bool,
    pub settings_path: String,
    pub hook_path: String,
    pub hook_ready: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookPreview {
    pub diff: String,
    pub backup: String,
    pub settings_path: String,
    /// Identifies the bytes this diff was computed from; handed back to `write`
    /// so we only ever apply what the user actually looked at.
    pub fingerprint: String,
}

pub fn settings_path() -> PathBuf {
    claude_dir(std::env::var_os("CLAUDE_CONFIG_DIR"), &platform::home_dir()).join("settings.json")
}

/// Claude Code's own folder, read as Claude Code reads it: `CLAUDE_CONFIG_DIR`
/// when the user moved it, else `~/.claude`. Hooks written anywhere else never
/// run. (After upstream pull request 810.)
fn claude_dir(config_dir: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    match config_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(".claude"),
    }
}

/// Claude Code sessions running when Lintel starts: `(session id, folder)` from
/// Claude Code's own `sessions/<pid>.json`, kept only when that process still
/// runs. Own user's folder, read only; the pills come at once instead of at
/// the session's next event. (After upstream pull request 516.)
pub fn running_sessions() -> Vec<(String, String)> {
    let dir = claude_dir(std::env::var_os("CLAUDE_CONFIG_DIR"), &platform::home_dir()).join("sessions");
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let procs = notch_core::win::process_table();
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(v) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { continue };
        let (Some(pid), Some(id), Some(cwd)) = (v["pid"].as_u64(), v["sessionId"].as_str(), v["cwd"].as_str()) else { continue };
        if v["kind"].as_str().is_some_and(|k| k != "interactive") {
            continue;
        }
        // A reused process id is not the session: only the runtimes Claude Code runs as count.
        let alive = procs.get(&(pid as u32)).is_some_and(|x| matches!(x.exe.to_ascii_lowercase().as_str(), "claude.exe" | "node.exe" | "bun.exe"));
        if alive && !id.is_empty() {
            out.push((id.to_string(), cwd.to_string()));
        }
    }
    out
}

/// The settings as they are, or an empty object when we cannot tell. Only for
/// read-only paths like `status()`, which must never fail loudly; anything that
/// writes goes through `config_file`, which surfaces the error instead.
fn read_settings_lossy() -> Value {
    config_file::read(&settings_path())
        .ok()
        .and_then(|bytes| config_file::parse_json(bytes.as_deref(), "settings.json").ok())
        .unwrap_or_else(|| json!({}))
}

/// On Windows Claude Code runs hook commands through Git Bash; on Linux through
/// `sh`. Either way the relay path is one quoted shell word.
fn hook_command(event: &str) -> String {
    agents::relay_command(Shell::Sh, event)
}

/// The command of one event's hook: with the opt-in, the two that start a turn
/// also tell the relay to start the app.
fn event_command(event: &str, launch: bool) -> String {
    if launch && LAUNCH_EVENTS.contains(&event) {
        hook_command(&format!("{LAUNCH_FLAG} {event}"))
    } else {
        hook_command(event)
    }
}

/// True when one of Lintel's own hook commands carries the launch flag.
fn launches(settings: &Value) -> bool {
    let flag = format!(" {LAUNCH_FLAG} ");
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|hooks| hooks.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter(|entry| entry_is_ours(entry))
        .filter_map(|entry| entry.get("hooks")?.as_array())
        .flatten()
        .filter_map(|hook| hook.get("command")?.as_str())
        .any(|command| command.contains(MARKER) && command.contains(&flag))
}

fn entry_is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| c.contains(MARKER))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// The status line in settings.json is Lintel's relay (old installs wrote
/// `lintel-hook StatusLine`, new ones `lintel-hook --statusline`; both match).
fn status_line_is_ours(v: &Value) -> bool {
    v.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(MARKER))
}

/// The relay's own path when `command` is a status line relay built from this
/// code: ours, an older or renamed build, another copy (`"…/x-hook.exe"
/// --statusline`, or the old `StatusLine`).
fn relay_exe(command: &str) -> Option<&str> {
    let (exe, arg) = command.trim().rsplit_once(char::is_whitespace)?;
    if arg != "--statusline" && arg != "StatusLine" {
        return None;
    }
    let exe = exe.trim().trim_matches(['"', '\'']);
    let name = exe.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    (name.ends_with("-hook") || name.ends_with("-hook.exe")).then_some(exe)
}

/// The status line to keep as the user's own when the relay takes `line`'s
/// place. Another relay is never kept: two relays that keep each other run each
/// other forever (#3). What that relay itself kept is, unless it is a relay too.
fn users_own(line: &Value) -> Option<Value> {
    let command = line.get("command").and_then(Value::as_str).unwrap_or_default();
    let Some(exe) = relay_exe(command) else {
        return Some(line.clone());
    };
    let kept = std::fs::read(Path::new(exe).with_file_name("statusline-previous.json")).ok()?;
    let kept = serde_json::from_slice::<Value>(&kept).ok().filter(Value::is_object)?;
    let command = kept.get("command").and_then(Value::as_str)?;
    relay_exe(command).is_none().then_some(kept)
}

/// Settings with Lintel's hooks added; everything else is left untouched. A
/// `hooks` (or one of its events) that is not what Claude Code documents is
/// refused rather than replaced. `launch`: see `event_command`.
fn merged(existing: &Value, launch: bool) -> Result<Value, String> {
    merged_with(existing, &|event| event_command(event, launch))
}

/// `merged` with the command each event runs: this machine's relay, or the one a
/// WSL distro (wsl.rs) or an SSH host (remote.rs) reaches it by.
pub fn merged_with(existing: &Value, command: &dyn Fn(&str) -> String) -> Result<Value, String> {
    let mut root = existing.as_object().cloned().unwrap_or_default();
    let mut hooks = match root.get("hooks") {
        None => Map::new(),
        Some(Value::Object(h)) => h.clone(),
        Some(_) => return Err(unexpected("\"hooks\"")),
    };

    for (event, timeout) in HOOK_EVENTS {
        let mut list = match hooks.get(*event) {
            None => Vec::new(),
            Some(Value::Array(list)) => list.clone(),
            Some(_) => return Err(unexpected(&format!("\"hooks\".\"{event}\""))),
        };
        list.retain(|entry| !entry_is_ours(entry));
        list.push(json!({
            "hooks": [{
                "type": "command",
                "command": command(event),
                "timeout": timeout,
            }]
        }));
        hooks.insert((*event).to_string(), Value::Array(list));
    }

    root.insert("hooks".into(), Value::Object(hooks));
    Ok(Value::Object(root))
}

fn unexpected(what: &str) -> String {
    crate::i18n::tf("settings.json: {what} has an unexpected type — {app} has not touched it.", &[("what", what)])
}

/// Settings with every Lintel entry removed, and nothing else changed.
pub fn without_ours(existing: &Value) -> Result<Value, String> {
    let mut root = existing.as_object().cloned().unwrap_or_default();
    let hooks = match root.get("hooks") {
        None => return Ok(Value::Object(root)),
        Some(Value::Object(h)) => h.clone(),
        Some(_) => return Err(unexpected("\"hooks\"")),
    };
    let mut out = Map::new();
    for (event, value) in hooks {
        match value.as_array() {
            Some(list) => {
                let kept: Vec<Value> =
                    list.iter().filter(|e| !entry_is_ours(e)).cloned().collect();
                if !kept.is_empty() {
                    out.insert(event, Value::Array(kept));
                }
            }
            None => {
                out.insert(event, value);
            }
        }
    }
    if out.is_empty() {
        root.remove("hooks");
    } else {
        root.insert("hooks".into(), Value::Object(out));
    }
    Ok(Value::Object(root))
}

/// `launch`: whether the hooks also start the app; `None` keeps what the file
/// has now, so installing again never switches it on or off by itself.
fn edits(install: bool, launch: Option<bool>) -> Vec<FileEdit<'static>> {
    vec![FileEdit {
        path: settings_path(),
        edit: config_file::json_edit("settings.json".into(), move |current| {
            if install {
                merged(current, launch.unwrap_or_else(|| launches(current))).map(Some)
            } else {
                without_ours(current).map(Some)
            }
        }),
    }]
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn status() -> HookStatus {
    let current = read_settings_lossy();
    let installed = has_ours(&current);
    let hook_path = settings::hook_exe_path();
    HookStatus {
        installed,
        plan_relay_installed: plan_relay_installed(&current),
        auto_launch: launches(&current),
        settings_path: settings_path().to_string_lossy().to_string(),
        hook_ready: hook_path.exists(),
        hook_path: hook_path.to_string_lossy().to_string(),
    }
}

pub fn preview(install: bool, launch: Option<bool>) -> Result<HookPreview, String> {
    Ok(plan_to_preview(config_file::preview(&edits(install, launch))?))
}

fn plan_to_preview(plan: config_file::Plan) -> HookPreview {
    HookPreview {
        diff: plan.diff,
        backup: plan.backup,
        settings_path: plan.path,
        fingerprint: plan.fingerprint,
    }
}

/// Writes the merged (or cleaned) settings after taking a dated backup, and
/// returns where the backup went ("" when there was no file to back up).
///
/// `fingerprint` is the one the preview was computed from: a settings.json that
/// changed in between is refused rather than overwritten (see config_file).
/// `launch` is what the preview was asked with.
pub fn write(install: bool, fingerprint: &str, launch: Option<bool>) -> Result<String, String> {
    let backups = config_file::apply(&edits(install, launch), fingerprint)?;
    Ok(backups.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
}

// ── Claude Code in WSL ────────────────────────────────────────────────────────
//
// The same hooks, in a WSL distro's own ~/.claude/settings.json, which Windows
// reaches as \\wsl.localhost\<distro>\home\<user>\.claude\. Only the command
// differs: it starts lintel-hook.exe through WSL interop (wsl.rs). The reading,
// the diff, the dated backup and the fingerprint are the same code. (After
// upstream pull request 763.)

/// True when one of Lintel's entries is in `settings`' hooks.
pub fn has_ours(settings: &Value) -> bool {
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| hooks.values().filter_map(Value::as_array).flatten().any(entry_is_ours))
}

fn wsl_edits(target: crate::wsl::Target, install: bool) -> Vec<FileEdit<'static>> {
    let relay = target.relay;
    vec![FileEdit {
        path: target.settings_path,
        edit: config_file::json_edit("settings.json".into(), move |current| {
            if install {
                merged_with(current, &|event| crate::wsl::relay_command(&relay, event)).map(Some)
            } else {
                without_ours(current).map(Some)
            }
        }),
    }]
}

pub fn wsl_preview(distro: &str, user: &str, install: bool) -> Result<HookPreview, String> {
    Ok(plan_to_preview(config_file::preview(&wsl_edits(crate::wsl::resolve(distro, user)?, install))?))
}

/// As `write`, for a distro's settings.json; returns where the backup went.
pub fn wsl_write(distro: &str, user: &str, install: bool, fingerprint: &str) -> Result<String, String> {
    let backups = config_file::apply(&wsl_edits(crate::wsl::resolve(distro, user)?, install), fingerprint)?;
    Ok(backups.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
}

// ── Status line (plan usage) ──────────────────────────────────────────────────
//
// Claude Code runs one `statusLine` command and hands it the plan limits. Lintel
// puts its relay there; a status line the user already had is kept in
// statusline-previous.json beside the relay, and the relay still runs it, so it
// keeps working. Installing and removing it is separate from the hooks, and
// goes through the same hardened writer (config_file.rs): strict read, diff,
// fingerprint, dated backup, then the write.

/// Where the user's own status line waits while the relay stands in for it.
pub fn status_line_previous_path() -> PathBuf {
    settings::hook_exe_path().with_file_name("statusline-previous.json")
}

fn read_status_line_previous() -> Option<Value> {
    let bytes = std::fs::read(status_line_previous_path()).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object)
}

/// True when the `statusLine` in settings.json is Lintel's relay.
pub fn plan_relay_installed(settings: &Value) -> bool {
    settings.get("statusLine").is_some_and(status_line_is_ours)
}

/// `statusLine` as it reads after installing or removing the relay. `None`: the
/// key goes. Installing swaps only `command`, so `padding`, `refreshInterval`
/// and the rest of the user's status line stay as they were.
fn status_line_after(existing: Option<&Value>, install: bool, previous: Option<&Value>) -> Option<Value> {
    if install {
        let mut sl = existing.filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        let obj = sl.as_object_mut().expect("an object");
        obj.entry("type").or_insert_with(|| json!("command"));
        obj.insert("command".into(), json!(hook_command("--statusline")));
        Some(sl)
    } else if existing.is_some_and(status_line_is_ours) {
        previous.cloned()
    } else {
        existing.cloned() // not ours any more (the user changed it): leave it alone
    }
}

fn status_line_settings(current: &Value, install: bool, previous: Option<&Value>) -> Result<Value, String> {
    let mut root = current.as_object().cloned().unwrap_or_default();
    // A statusLine that is not an object is something we do not understand:
    // refuse rather than replace it.
    if install && root.get("statusLine").is_some_and(|v| !v.is_object()) {
        return Err(unexpected("\"statusLine\""));
    }
    match status_line_after(root.get("statusLine"), install, previous) {
        Some(sl) => root.insert("statusLine".into(), sl),
        None => root.remove("statusLine"),
    };
    Ok(Value::Object(root))
}

fn status_line_edits(install: bool, previous: Option<Value>) -> Vec<FileEdit<'static>> {
    vec![FileEdit {
        path: settings_path(),
        edit: config_file::json_edit("settings.json".into(), move |current| {
            Ok(Some(status_line_settings(current, install, previous.as_ref())?))
        }),
    }]
}

/// The diff the user has to look at before the relay goes in or out.
pub fn status_line_preview(install: bool) -> Result<HookPreview, String> {
    Ok(plan_to_preview(config_file::preview(&status_line_edits(install, read_status_line_previous()))?))
}

/// Installs or removes the relay, after the same backup and fingerprint checks as
/// the hooks. Installing first saves a status line of the user's own, removing
/// puts it back (or removes the key if there was none).
pub fn status_line_write(install: bool, fingerprint: &str) -> Result<String, String> {
    let before = config_file::read(&settings_path())
        .and_then(|bytes| config_file::parse_json(bytes.as_deref(), "settings.json"))?;
    let previous = read_status_line_previous();
    let own = before.get("statusLine").filter(|v| !status_line_is_ours(v)).and_then(users_own);
    let saved = match (install, own.as_ref()) {
        (true, Some(own)) => {
            save_status_line_previous(own).map_err(|_| {
                crate::i18n::t("Could not save your current status line next to the relay; nothing was changed.")
            })?;
            true
        }
        _ => false,
    };
    let result = config_file::apply(&status_line_edits(install, previous), fingerprint);
    match &result {
        // Back in settings.json: the saved copy has done its job.
        Ok(_) if !install => {
            let _ = std::fs::remove_file(status_line_previous_path());
        }
        // Nothing was written: do not leave a stale copy behind.
        Err(_) if saved => {
            let _ = std::fs::remove_file(status_line_previous_path());
        }
        _ => {}
    }
    let backups = result?;
    Ok(backups.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
}

/// For the uninstaller (`--uninstall-hooks`, lib.rs): takes Lintel's hooks and
/// its status line relay out of Claude Code's settings, when they are there,
/// each through the same preview, fingerprint and dated backup as a click in
/// Settings. What was tried and how it went; nothing to do is an empty list.
pub fn uninstall_all() -> Vec<(&'static str, Result<String, String>)> {
    let mut done = Vec::new();
    let found = status();
    if found.installed {
        done.push(("Claude Code hooks", preview(false, None).and_then(|plan| write(false, &plan.fingerprint, None))));
    }
    if found.plan_relay_installed {
        // Read again: the hooks' removal has just changed the file.
        let result = status_line_preview(false).and_then(|plan| status_line_write(false, &plan.fingerprint));
        done.push(("Claude Code status line", result));
    }
    done
}

fn save_status_line_previous(status_line: &Value) -> std::io::Result<()> {
    let path = status_line_previous_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    config_file::write_like(&path, &path, config_file::pretty(status_line).as_bytes())
}

/// Copies the relay (lintel-hook.exe / lintel-hook) into the local data dir's
/// bin/ on launch. In a bundled install it comes from the app resources; in
/// `tauri dev` it sits next to the app binary in the workspace target directory.
///
/// Every candidate is tried rather than just the first, because getting this
/// wrong is silent and fatal: `resources` used to be a glob, which made NSIS
/// mirror the source path into `_up_\target\release\`, no candidate matched, and
/// the relay was simply never installed. It only looked healthy on a developer
/// machine, where a leftover copy from `tauri dev` was already sitting in bin/.
pub fn ensure_hook_exe(app: &AppHandle) {
    let dest = settings::hook_exe_path();
    let Some(dir) = dest.parent() else { return };
    // Nobody else may swap the relay Claude Code runs: its folder is ours only.
    if platform::ensure_private_dir(&settings::local_dir()).is_err()
        || std::fs::create_dir_all(dir).is_err()
    {
        return;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = app.path().resolve(platform::HOOK_EXE, tauri::path::BaseDirectory::Resource) {
        candidates.push(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // Installed build, then `tauri dev` (target/debug) next to the
            // release hook the pre-build step produces.
            candidates.push(parent.join(platform::HOOK_EXE));
            candidates.push(parent.join("../release").join(platform::HOOK_EXE));
            // Belt and braces: where the old glob form used to land it.
            candidates.push(parent.join("_up_/target/release").join(platform::HOOK_EXE));
        }
    }

    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    let Some(src) = candidates.into_iter().find(|p| p.exists()) else {
        crate::log::line(format!(
            "{} not found — Claude Code hooks cannot work. Looked in: {}",
            platform::HOOK_EXE,
            tried.join(", ")
        ));
        return;
    };
    install_relay(&src, &dest);
    // Where this app is, for a relay installed with the opt-in to start it
    // (hook/src/launch.rs). In the relay's own folder, which only the user can
    // write; the relay starts nothing but a file with the app's own name.
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::write(dest.with_file_name(APP_PATH_FILE), exe.to_string_lossy().as_bytes());
    }
}


fn install_relay(src: &Path, dest: &Path) {
    let same = match (std::fs::metadata(src), std::fs::metadata(dest)) {
        (Ok(a), Ok(b)) => a.len() == b.len() && a.modified().ok() == b.modified().ok(),
        _ => false,
    };
    if same {
        return;
    }
    // A hook may be running right now and hold the file open; keeping the old
    // copy is fine, it is the same relay.
    if let Err(err) = std::fs::copy(src, dest) {
        if !dest.exists() {
            crate::log::line(format!("could not install {}: {err}", platform::HOOK_EXE));
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hooks_leave_the_status_line_alone() {
        let theirs = json!({ "statusLine": { "type": "command", "command": "~/bin/my-line" } });
        assert_eq!(merged(&theirs, false).unwrap()["statusLine"], theirs["statusLine"]);
        assert_eq!(without_ours(&theirs).unwrap()["statusLine"], theirs["statusLine"]);
        assert!(merged(&json!({}), false).unwrap().get("statusLine").is_none());
    }

    #[test]
    fn the_relay_takes_the_status_line_and_keeps_the_users_other_fields() {
        // None yet: ours is added.
        let fresh = status_line_settings(&json!({ "model": "opus" }), true, None).unwrap();
        assert!(status_line_is_ours(&fresh["statusLine"]));
        assert_eq!(fresh["statusLine"]["type"], "command");
        assert_eq!(fresh["model"], "opus");

        // Their own: only the command is swapped; padding and refresh stay.
        let own = json!({ "statusLine": { "type": "command", "command": "~/bin/my-line", "padding": 2, "refreshInterval": 5 } });
        let taken = status_line_settings(&own, true, None).unwrap();
        assert!(status_line_is_ours(&taken["statusLine"]));
        assert_eq!(taken["statusLine"]["padding"], 2);
        assert_eq!(taken["statusLine"]["refreshInterval"], 5);

        // Installing again keeps ours and its extra fields.
        let again = status_line_settings(&taken, true, None).unwrap();
        assert_eq!(again["statusLine"]["padding"], 2);
        assert!(status_line_is_ours(&again["statusLine"]));
    }

    #[test]
    fn removing_the_relay_restores_what_was_there_and_never_touches_anything_else() {
        let previous = json!({ "type": "command", "command": "~/bin/my-line", "padding": 2 });
        let ours = status_line_settings(&json!({}), true, None).unwrap();

        // There was one before: it comes back exactly.
        assert_eq!(status_line_settings(&ours, false, Some(&previous)).unwrap()["statusLine"], previous);
        // There was none: the key goes.
        assert!(status_line_settings(&ours, false, None).unwrap().get("statusLine").is_none());
        // The user changed it since: not ours, so untouched.
        let theirs = json!({ "statusLine": { "type": "command", "command": "~/bin/other" } });
        assert_eq!(status_line_settings(&theirs, false, Some(&previous)).unwrap()["statusLine"], theirs["statusLine"]);
        // A statusLine we do not understand is refused, never replaced.
        assert!(status_line_settings(&json!({ "statusLine": "echo hi" }), true, None).is_err());
    }

    #[test]
    fn another_relay_is_never_kept_as_the_users_status_line() {
        assert_eq!(relay_exe("\"C:/x/notch/bin/notch-hook.exe\" --statusline"), Some("C:/x/notch/bin/notch-hook.exe"));
        assert!(relay_exe("\"C:/x/lintel-hook.exe\" StatusLine").is_some());
        assert!(relay_exe("'/home/me/.local/bin/other-hook' --statusline").is_some());
        assert!(relay_exe("~/bin/my-line --statusline").is_none());
        assert!(relay_exe("\"C:/x/notch-hook.exe\" SessionStart").is_none());

        // What the other relay kept is the user's own, unless it is a relay too.
        let dir = std::env::temp_dir().join(format!("lintel-relay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("notch-hook.exe").to_string_lossy().replace('\\', "/");
        let theirs = json!({ "command": format!("\"{exe}\" --statusline") });
        assert_eq!(users_own(&theirs), None);
        let mine = json!({ "type": "command", "command": "~/bin/my-line" });
        std::fs::write(dir.join("statusline-previous.json"), mine.to_string()).unwrap();
        assert_eq!(users_own(&theirs), Some(mine.clone()));
        std::fs::write(dir.join("statusline-previous.json"), json!({ "command": "\"C:/y/lintel-hook.exe\" --statusline" }).to_string()).unwrap();
        assert_eq!(users_own(&theirs), None);
        assert_eq!(users_own(&mine), Some(mine));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn merging_keeps_every_other_setting_and_every_foreign_hook() {
        let existing = serde_json::json!({
            "model": "claude-opus-5",
            "theme": "dark",
            "enabledPlugins": ["a", "b"],
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "someone-elses-tool.exe" }] }
                ],
                "SomeEventWeDoNotTouch": [
                    { "hooks": [{ "type": "command", "command": "keep-me.exe" }] }
                ]
            }
        });

        let after = merged(&existing, false).unwrap();
        assert_eq!(after["model"], "claude-opus-5");
        assert_eq!(after["theme"], "dark");
        assert_eq!(after["enabledPlugins"], serde_json::json!(["a", "b"]));

        let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            pre.iter().any(|e| serde_json::to_string(e).unwrap().contains("someone-elses-tool.exe")),
            "another tool's hook was dropped"
        );
        assert!(pre.iter().any(entry_is_ours), "our own hook was not added");
        assert!(after["hooks"]["SomeEventWeDoNotTouch"].is_array());

        // Installing twice still leaves one entry of ours per event. (Compared
        // by count: another test points HOME elsewhere meanwhile, which moves
        // the relay path.)
        let twice = merged(&after, false).unwrap();
        assert_eq!(twice["hooks"]["PreToolUse"].as_array().unwrap().iter().filter(|e| entry_is_ours(e)).count(), 1);

        // And removing ours puts it back exactly as it was.
        let cleaned = without_ours(&after).unwrap();
        assert_eq!(cleaned, existing);
    }

    #[test]
    fn values_of_an_unexpected_type_are_refused_not_replaced() {
        for odd in [
            json!({ "hooks": "a string" }),
            json!({ "hooks": [1, 2] }),
            json!({ "hooks": { "PreToolUse": { "not": "a list" } } }),
        ] {
            assert!(merged(&odd, false).is_err(), "{odd}");
        }
        assert!(without_ours(&json!({ "hooks": 3 })).is_err());
        // An event we do not install stays as it is, whatever its shape.
        let kept = json!({ "hooks": { "Custom": "anything" } });
        assert_eq!(without_ours(&kept).unwrap(), kept);
    }

    #[test]
    fn starting_the_app_is_opt_in_and_only_on_the_hooks_that_start_a_turn() {
        let command = |settings: &Value, event: &str| -> String {
            let ours = settings["hooks"][event].as_array().unwrap().iter().find(|e| entry_is_ours(e)).unwrap();
            ours["hooks"][0]["command"].as_str().unwrap().to_string()
        };
        let theirs = json!({ "hooks": { "UserPromptSubmit": [
            { "hooks": [{ "type": "command", "command": "their-tool --launch UserPromptSubmit" }] }
        ] } });

        // Off unless asked for: no command of ours carries the flag, and
        // somebody else's command that happens to say the same is not ours.
        let off = merged(&theirs, false).unwrap();
        assert!(!launches(&off));
        for (event, _) in HOOK_EVENTS {
            assert!(!command(&off, event).contains(LAUNCH_FLAG), "{event}");
        }

        // On: exactly the two hooks that start a turn, and never a permission request.
        let on = merged(&off, true).unwrap();
        assert!(launches(&on));
        for (event, _) in HOOK_EVENTS {
            let flagged = command(&on, event).contains(&format!(" {LAUNCH_FLAG} {event}"));
            assert_eq!(flagged, LAUNCH_EVENTS.contains(event), "{event}");
        }
        assert!(!command(&on, "PermissionRequest").contains(LAUNCH_FLAG));
        // Still one entry of ours per event, the other tool's entry untouched.
        assert_eq!(on["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 2);
        assert_eq!(on["hooks"]["UserPromptSubmit"][0], theirs["hooks"]["UserPromptSubmit"][0]);

        // Off again is exactly the plain install; removing ours leaves what the user had.
        assert_eq!(merged(&on, false).unwrap(), off);
        assert_eq!(without_ours(&on).unwrap(), theirs);
    }

    #[test]
    fn a_moved_claude_folder_is_where_the_settings_are() {
        let home = Path::new("C:/Users/me");
        assert_eq!(claude_dir(None, home), home.join(".claude"));
        assert_eq!(claude_dir(Some("".into()), home), home.join(".claude"));
        assert_eq!(claude_dir(Some("E:/Claude/data".into()), home), PathBuf::from("E:/Claude/data"));
    }

    /// Everything filesystem-shaped lives in one test on purpose: it points
    /// the home directory at a temp directory, and that is process-wide.
    #[test]
    fn writing_backs_up_preserves_and_refuses_a_changed_file() {
        let tmp = std::env::temp_dir().join(format!("lintel-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".claude")).unwrap();
        std::env::set_var(notch_core::win::HOME_VAR, &tmp);
        // A moved Claude folder would point outside the temp directory.
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        let path = settings_path();
        assert!(path.starts_with(&tmp), "the test must not touch the real home");

        // A real-shaped file, written the way PowerShell 5 would: UTF-8 with BOM.
        let original = r#"{"model":"claude-opus-5","theme":"dark","tui":{"x":1},"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"other-tool.exe"}]}]}}"#;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(original.as_bytes());
        std::fs::write(&path, &bytes).unwrap();

        // Install.
        let plan = preview(true, None).expect("a BOM must not stop the preview");
        assert!(plan.diff.contains("lintel-hook"), "the diff must show what changes");
        assert!(!plan.diff.contains(LAUNCH_FLAG), "starting the app is never part of a plain install");
        let backup = write(true, &plan.fingerprint, None).expect("install should succeed");

        // The backup holds the original bytes, BOM and all.
        assert_eq!(std::fs::read(&backup).unwrap(), bytes);

        // Everything else survived, and so did the other tool's hook.
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["model"], "claude-opus-5");
        assert_eq!(after["theme"], "dark");
        assert_eq!(after["tui"]["x"], 1);
        let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(pre.iter().any(|e| serde_json::to_string(e).unwrap().contains("other-tool.exe")));
        assert!(status().installed);
        assert!(!status().auto_launch);

        // The opt-in is a change like any other: its diff shows the flag, and
        // only the write that follows the preview turns it on. Installing again
        // without saying so keeps it; saying no takes it out.
        let opt_in = preview(true, Some(true)).unwrap();
        assert!(opt_in.diff.contains(LAUNCH_FLAG), "the diff must show the flag");
        assert!(!status().auto_launch, "a preview writes nothing");
        write(true, &opt_in.fingerprint, Some(true)).unwrap();
        assert!(status().auto_launch);
        let again = preview(true, None).unwrap();
        write(true, &again.fingerprint, None).unwrap();
        assert!(status().auto_launch);
        let opt_out = preview(true, Some(false)).unwrap();
        write(true, &opt_out.fingerprint, Some(false)).unwrap();
        assert!(!status().auto_launch);

        // A file that moved since the preview is refused, and left alone.
        let stale = preview(false, None).unwrap();
        std::fs::write(&path, br#"{"model":"someone-else-edited-this"}"#).unwrap();
        let err = write(false, &stale.fingerprint, None).unwrap_err();
        assert!(err.contains("changed since the preview"), "got: {err}");
        let untouched: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(untouched["model"], "someone-else-edited-this");

        // Content we cannot parse is refused before anything is written.
        std::fs::write(&path, b"{ broken").unwrap();
        assert!(preview(true, None).is_err());
        assert!(write(true, "whatever", None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ broken");
        // The uninstaller leaves such a file alone too, and has nothing to say.
        assert!(uninstall_all().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ broken");

        // The uninstaller: the hooks go, with a backup, and everything else in
        // the file stays. (The status line relay keeps the user's own line in
        // the app's data folder, which a test must not touch: not run here.)
        std::fs::write(&path, &bytes).unwrap();
        let plan = preview(true, None).unwrap();
        write(true, &plan.fingerprint, None).unwrap();
        assert!(status().installed);
        let installed = std::fs::read(&path).unwrap();

        let done = uninstall_all();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, "Claude Code hooks");
        let backup = done[0].1.clone().expect("hooks removed");
        assert_eq!(std::fs::read(&backup).unwrap(), installed, "the backup holds the file as it was");
        assert!(!status().installed);
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after, serde_json::from_str::<Value>(original).unwrap(), "back to what the user had");
        // A second run finds nothing and writes nothing.
        assert!(uninstall_all().is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
