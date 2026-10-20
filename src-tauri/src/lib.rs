// Lintel for Windows — app wiring and the commands the island calls.

pub mod watchdog;
mod agent_hooks;
mod agents;
mod chat;
mod claude;
mod codex_plan;
mod features;
mod github;
mod hooks;
mod identity;
mod integrations;
mod island;
mod local_chat;
mod openai_compat;
mod pipe;
mod platform;
mod recap;
mod registry;
mod remote;
mod session_window;
mod settings;
mod shortcuts;
mod sounds;
mod tray;
mod brand_icon;
mod shortcut_rule;
mod webview_drop;
mod webview_permission;
mod winfactory;
mod wsl;

// What moved to notch-core, under the names the app's modules always used.
pub(crate) use notch_core::{config_file, files, i18n, log, net, secrets};

use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_autostart::{ManagerExt, MacosLauncher};

use chat::{Chat, ChatContext, ChatReply, ModelInfo};
use files::DroppedFile;
use hooks::{HookPreview, HookStatus};
use island::{PollGate, ScreenInfo};
use pipe::Pending;
use settings::Settings;

pub struct Shared {
    pub settings: Mutex<Settings>,
    pub gate: Arc<PollGate>,
}

/// `lintel.exe --uninstall-hooks [--home <dir>]`, run by the uninstaller
/// (nsis/hooks.nsh) before it deletes the relay: takes Lintel's entries out of
/// every agent CLI's config, so no agent is left calling a program that is
/// gone (one that fails closed on a broken hook would refuse every tool call).
/// Each file gets the dated backup a click in Settings gives it, and a file
/// that cannot be read, or holds something unexpected, is left alone. Only
/// Lintel's own entries go. (The mode is ported from upstream pull request 445.)
///
/// `--home <dir>` names another home directory than the user's: a test, or a
/// profile that is not the one running the uninstaller. Returns the exit code:
/// 0 unless a file had to be left alone.
pub fn uninstall_hooks(args: &[String]) -> i32 {
    if let Some(i) = args.iter().position(|a| a == "--home") {
        let Some(home) = args.get(i + 1).filter(|dir| std::path::Path::new(dir).is_dir()) else {
            eprintln!("--home needs an existing directory");
            return 2;
        };
        std::env::set_var(notch_core::win::HOME_VAR, home);
    }
    let mut done = hooks::uninstall_all();
    done.extend(agents::uninstall_all());
    let mut failed = false;
    for (what, result) in &done {
        let line = match result {
            Ok(backups) => format!("uninstall: {what}: removed, backup {}", backups.replace('\n', ", ")),
            Err(why) => {
                failed = true;
                format!("uninstall: {what}: left alone: {why}")
            }
        };
        println!("{line}");
        log::line(line);
    }
    if done.is_empty() {
        println!("uninstall: no agent had hooks installed");
    }
    i32::from(failed)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootInfo {
    settings: Settings,
    screen: ScreenInfo,
    version: String,
    hook_path: String,
}

#[tauri::command]
fn boot(app: AppHandle, shared: State<Shared>) -> BootInfo {
    let mut settings = shared.settings.lock().unwrap().clone();
    // The real state of ~/.claude/settings.json wins over whatever we stored.
    let hooks_status = hooks::status();
    settings.hooks_installed = hooks_status.installed;
    settings.plan_relay_installed = hooks_status.plan_relay_installed;
    let screen = island::screen_info(&app, &settings.screen);
    BootInfo {
        settings,
        screen,
        version: env!("CARGO_PKG_VERSION").to_string(),
        hook_path: settings::hook_exe_path().to_string_lossy().to_string(),
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, shared: State<Shared>, settings: Settings) {
    let (screen_changed, autostart_changed, shortcuts_changed) = {
        let mut current = shared.settings.lock().unwrap();
        let screen_changed = current.screen != settings.screen;
        let autostart_changed = current.autostart != settings.autostart;
        let shortcuts_changed = current.shortcuts != settings.shortcuts;
        // The features' own settings and switches are the registry's to say
        // (feature_patch_settings, feature_set_enabled), not a webview's copy.
        let mut settings = settings.clone();
        settings.features = current.features.clone();
        settings.enabled = current.enabled.clone();
        *current = settings;
        (screen_changed, autostart_changed, shortcuts_changed)
    };
    let settings = shared.settings.lock().unwrap().clone();
    if let Err(err) = settings::save(&settings) {
        log::line(format!("could not save settings: {err}"));
    }
    if autostart_changed {
        let manager = app.autolaunch();
        let result = if settings.autostart { manager.enable() } else { manager.disable() };
        if let Err(err) = result {
            eprintln!("[lintel] autostart: {err}");
        }
    }
    if screen_changed {
        let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
        island::apply_geometry(&app, &settings.screen, collapsed);
    }
    integrations::settings_saved(&app, &settings.active_integrations);
    if shortcuts_changed {
        shortcuts::apply(&app, &settings.shortcuts);
    }
    if i18n::set_picked(&settings.language) {
        language_changed(&app);
    }
    // Keep the other window in step (island ⇄ settings window).
    let _ = app.emit("settings-changed", settings);
}

/// The island reports the system's languages at launch, for "System" in
/// Settings → Language (WebView2 and WebKitGTK know them best).
#[tauri::command]
fn set_system_languages(app: AppHandle, languages: Vec<String>) {
    if i18n::set_system(languages) {
        language_changed(&app);
    }
}

/// What Rust labels itself follows the new language: the tray menu and the
/// settings window's title. The webviews switch on their own.
fn language_changed(app: &AppHandle) {
    tray::rebuild(app);
    winfactory::retitle_settings(app);
}

/// Hidden island → shrink the window to the invisible wake strip and park the
/// cursor poll; anything else → full panel and 60 Hz polling.
#[tauri::command]
fn set_collapsed(app: AppHandle, shared: State<Shared>, collapsed: bool) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    shared.gate.collapsed.store(collapsed, Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
    // The wake strip must always take the mouse, and a resize invalidates the flag.
    island::refresh_click_through(&app, &shared.gate);
    shared.gate.set_active(!collapsed);
    if collapsed {
        shared.gate.resting.store(false, Ordering::Relaxed);
        // Hidden, whatever the page did or did not say first: no key of the
        // user's is the island's any more, and it holds no keyboard.
        platform::keyhook::pointer(false);
        island::watch_outside(false);
        if let Some(win) = island::window(&app) {
            platform::give_keyboard_back(&win);
        }
    }
}

/// A tool of the island's Utilities page: the shortcut action of a running
/// feature, run as if its key had been pressed. Only what a running feature's
/// manifest names; the work itself hops off this thread (registry::notify).
#[tauri::command]
fn feature_action(app: AppHandle, feature: String, id: String) {
    if let Some((owner, def)) = registry::shortcut_defs(&app).into_iter().find(|(f, d)| *f == feature && d.id == id) {
        registry::notify(&app, owner, "shortcut", def.id);
    }
}

/// The page says when a press outside the island would fold it: open, and no
/// card waiting. The mouse hook exists only then (island::watch_outside).
#[tauri::command]
fn set_outside_watch(on: bool) {
    island::watch_outside(on);
}

/// The page says the strip rests: the setting keeps it at peek, the pointer is
/// away and nothing moves. The cursor poll then only looks for the pointer
/// coming back, ten times a second, and ends the rest itself when it does
/// (island::spawn_cursor_poll).
#[tauri::command]
fn set_poll_rest(shared: State<Shared>, on: bool) {
    shared.gate.resting.store(on, Ordering::Relaxed);
}

/// The front end pushes the island shape; Rust decides click-through from it.
#[tauri::command]
fn set_island_rect(shared: State<Shared>, x: f64, y: f64, width: f64, height: f64) {
    shared.gate.set_rect(island::IslandRect { x, y, w: width, h: height });
}

/// The island takes the keyboard (a click inside it, a shortcut, a view with a
/// text field) or lets go of it, handing it back to the window it took it
/// from. Answers whether the island holds it now.
///
/// An async command, and the work is then put on the main thread: a plain one
/// runs inside the web view's own request callback, where the focus cannot be
/// moved into the web view (platform::take_keyboard).
#[tauri::command]
async fn focus_window(app: AppHandle, focused: bool) -> bool {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let held = island::window(&handle).is_some_and(|win| {
            if focused {
                platform::take_keyboard(&win)
            } else {
                platform::give_keyboard_back(&win);
                false
            }
        });
        let _ = tx.send(held);
    });
    rx.await.unwrap_or(false)
}

/// The key combinations that are the island's own while the pointer is over it
/// and it does not hold the keyboard (platform/keyhook.rs). Empty: none.
#[tauri::command]
fn set_island_keys(keys: Vec<u32>) {
    platform::keyhook::set_keys(keys);
}

#[tauri::command]
fn reposition(app: AppHandle, shared: State<Shared>) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
}

/// The page reports its pixel ratio; the web view's zoom is corrected when it
/// differs from the window's scale. True when the zoom changed.
#[tauri::command]
fn fit_zoom(app: AppHandle, dpr: f64) -> bool {
    static ZOOM: Mutex<f64> = Mutex::new(1.0);
    let Some(win) = island::window(&app) else { return false };
    let scale = win.scale_factor().unwrap_or(1.0);
    let mut zoom = ZOOM.lock().unwrap();
    match island::zoom_correction(*zoom, scale, dpr) {
        Some(z) => {
            *zoom = z;
            let _ = win.set_zoom(z);
            true
        }
        None => false,
    }
}

/// The displays, for the page's placement rules (src/island/place.ts).
#[tauri::command]
fn island_displays(app: AppHandle) -> Vec<island::DisplayInfo> {
    island::displays(&app)
}

/// The page says where the island lives: a spot it was dragged to, or nowhere
/// of its own (the top centre of the chosen display).
#[tauri::command]
fn island_place(app: AppHandle, shared: State<Shared>, place: Option<island::Place>) {
    island::set_place(place);
    let pref = shared.settings.lock().unwrap().screen.clone();
    island::apply_geometry(&app, &pref, shared.gate.collapsed.load(Ordering::Relaxed));
    island::refresh_click_through(&app, &shared.gate);
}

/// A press on the strip or the header has become a drag: the cursor poll
/// carries the window from here until the button is let go (`island-drop`).
/// `lift`: the island moved down by this much inside its window as it took its
/// floating shape, so the window goes up by as much and it stays under the pointer.
#[tauri::command]
fn island_drag(app: AppHandle, shared: State<Shared>, lift: f64) {
    let Some(win) = island::window(&app) else { return };
    let (Some((cx, cy)), Ok(at)) = (platform::cursor_physical(), win.outer_position()) else { return };
    let lift = (lift * win.scale_factor().unwrap_or(1.0)).round() as i32;
    *shared.gate.drag.lock().unwrap() = Some((cx as i32 - at.x, cy as i32 - at.y + lift));
}

/// The displays the island can be pinned to, for Settings.
#[tauri::command]
fn list_monitors(app: AppHandle) -> Vec<island::MonitorChoice> {
    island::monitor_choices(&app)
}

#[tauri::command]
fn open_url(url: String) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    platform::open_url(&url);
}

/// "Open terminal" opens the working folder in VS Code when `code` is on PATH,
/// and falls back to the file manager otherwise.
#[tauri::command]
fn open_in_vscode(path: Option<String>) -> bool {
    // No shell anywhere near this. The path is a project folder chosen by
    // whoever is using Claude Code, and a shell would happily read `&`, `^`, `%`
    // or `$` in a folder name as syntax. Finding the launcher ourselves and
    // handing the path over as a separate argument keeps it a path.
    let path = path.filter(|p| !p.is_empty());
    // A session in WSL reports a Linux path: it opens from the distro's share (wsl.rs).
    if let Some(linux) = path.as_deref().filter(|p| p.starts_with('/')) {
        return wsl::open_in_editor(linux, true);
    }
    // It arrives in a hook payload: only an existing folder, given by its full
    // path, goes any further. `code` would read `--something` as an option, and
    // xdg-open would launch a file with whatever handles its type.
    if let Some(p) = path.as_deref() {
        let p = std::path::Path::new(p);
        if !(p.is_absolute() && p.is_dir()) {
            return false;
        }
    }
    if let Some(code) = platform::find_on_path("code") {
        let mut cmd = Command::new(code);
        if let Some(p) = path.as_deref() {
            cmd.arg(p);
        }
        if platform::no_console(&mut cmd).spawn().is_ok() {
            return true;
        }
    }
    if let Some(p) = path.as_deref() {
        platform::reveal_folder(p);
    }
    false
}

/// "Open terminal", "Open Codex", "Open Claude"…: brings a session's home
/// forward. First the window the session was found to run in (the relay's own
/// ancestor, see session_window.rs), down to its tab in Windows Terminal; then,
/// for a session that lives in an app, that app (`owner`, one of the names
/// `platform::open_owner` knows, anything else is ignored); otherwise the
/// folder in VS Code, as before. Nothing an agent wrote is ever started: a
/// window we found ourselves, a program from a fixed list, or the editor on a
/// folder that exists.
///
/// Async, and its work on a blocking thread: the tab is chosen through UI
/// Automation, which must not run inside the page's own request.
#[tauri::command]
async fn open_session(app: AppHandle, session_id: Option<String>, path: Option<String>, owner: Option<String>) -> bool {
    tauri::async_runtime::spawn_blocking(move || jump_to_session(&app, session_id, path, owner))
        .await
        .unwrap_or(false)
}

fn jump_to_session(app: &AppHandle, session_id: Option<String>, path: Option<String>, owner: Option<String>) -> bool {
    // A session on an SSH host: nothing here is its window (remote.rs).
    if let Some(opened) = remote::open_session(session_id.as_deref(), path.as_deref()) {
        return opened;
    }
    let folder = path.as_deref().map(session_window::folder_name).unwrap_or_default().to_string();
    let found = session_id
        .as_deref()
        .and_then(session_window::lookup)
        .and_then(|owners| platform::focus_session_window(&owners, &folder));
    if let Some(hwnd) = found {
        // In Windows Terminal the session is one tab among several: the one
        // titled after its folder is selected (misc-late; an error, the feature
        // being off or another kind of window all mean "leave it as it is").
        let _ = registry::call(
            app,
            "misc-late",
            "terminal_tab",
            serde_json::json!({ "cwd": path.as_deref().unwrap_or_default(), "title": folder, "hwnd": hwnd }),
        );
        return true;
    }
    match owner.as_deref() {
        Some(owner) if platform::is_owner(owner) => platform::open_owner(owner, path.as_deref()),
        _ => open_in_vscode(path),
    }
}

/// The relay started this run because an agent began to work while the app was
/// closed (the opt-in of Settings › Agents): the page then also quits once the
/// agents are quiet. Never true for a start of the user's own.
#[tauri::command]
fn auto_launched() -> bool {
    std::env::args().any(|a| a == AUTO_LAUNCHED)
}

/// What the relay starts the app with (hook/src/launch.rs).
const AUTO_LAUNCHED: &str = "--auto-launched";

/// The file behind a live diff, if it may be handed to the editor: an existing
/// regular file given by its full path. Anything else — a relative path, a
/// folder, a path `code` could read as an option — goes no further.
fn diff_file(path: &str) -> Option<&std::path::Path> {
    let p = std::path::Path::new(path);
    (p.is_absolute() && p.is_file()).then_some(p)
}

/// The diff card's ↗: opens the edited file in VS Code when `code` is on PATH,
/// otherwise shows its folder. The file itself is never opened by its type —
/// xdg-open or Explorer would run a script that Claude just wrote.
#[tauri::command]
fn open_file_in_vscode(path: String) -> bool {
    if path.starts_with('/') {
        return wsl::open_in_editor(&path, false);
    }
    let Some(file) = diff_file(&path) else { return false };
    if let Some(code) = platform::find_on_path("code") {
        let mut cmd = Command::new(code);
        cmd.arg(file);
        if platform::no_console(&mut cmd).spawn().is_ok() {
            return true;
        }
    }
    if let Some(folder) = file.parent().filter(|d| d.is_dir()) {
        platform::reveal_folder(&folder.to_string_lossy());
    }
    false
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Tray → Pause. Paused means paused: the pollers stop talking to the network,
/// not just the island stopping showing things. Features read it as `Ctx::paused`.
#[tauri::command]
fn set_paused(app: tauri::AppHandle, paused: bool) {
    notch_core::set_paused(paused);
    // The voice assistant's guards: everything of it stops, or comes back.
    if registry::is_running(&app, "voice") {
        registry::notify(&app, "voice", "wake.sync", "");
    }
}

// ── Claude Code hooks ─────────────────────────────────────────────────────────

#[tauri::command]
fn hooks_status() -> HookStatus {
    hooks::status()
}

/// The session code view: a few lines of a file the session changed, inside its folder only.
#[tauri::command]
async fn session_file_lines(cwd: String, path: String, from: usize, count: usize) -> Option<Vec<String>> {
    tauri::async_runtime::spawn_blocking(move || session_window::file_lines(&cwd, &path, from, count)).await.ok().flatten()
}

/// Claude Code sessions already running: (session id, folder).
#[tauri::command]
async fn running_sessions() -> Vec<(String, String)> {
    tauri::async_runtime::spawn_blocking(hooks::running_sessions).await.unwrap_or_default()
}

/// Pill ID → whether that agent's hooks reach Lintel. Read-only.
#[tauri::command]
fn agent_hooks_status() -> std::collections::HashMap<String, bool> {
    agent_hooks::status()
}

/// Returns the diff the user has to look at before anything is written.
/// `launch`: the hooks that start a turn also start the app when it is closed
/// (the opt-in); left out, that stays as it is installed.
#[tauri::command]
fn hooks_preview(install: bool, launch: Option<bool>) -> Result<HookPreview, String> {
    hooks::preview(install, launch)
}

/// Only ever called from an explicit click in the settings window, with the
/// same `launch` the preview was asked with.
#[tauri::command]
fn hooks_apply(
    app: AppHandle,
    shared: State<Shared>,
    install: bool,
    fingerprint: String,
    launch: Option<bool>,
) -> Result<String, String> {
    // The fingerprint comes from the preview the user actually looked at, so a
    // settings.json that changed in between is refused rather than overwritten.
    let backup = hooks::write(install, &fingerprint, launch)?;
    let updated = {
        let mut current = shared.settings.lock().unwrap();
        current.hooks_installed = install;
        if let Err(err) = settings::save(&current) {
            log::line(format!("could not save settings: {err}"));
        }
        current.clone()
    };
    let _ = app.emit("settings-changed", updated);
    Ok(backup)
}

// ── Other agents' hooks and plugins ──────────────────────────────────────────

#[tauri::command]
fn agent_hooks_list() -> Vec<agents::AgentStatus> {
    agents::list()
}

/// The diff the user has to look at before anything is written.
#[tauri::command]
fn agent_hooks_preview(agent: String, install: bool) -> Result<config_file::Plan, String> {
    agents::preview(&agent, install)
}

/// Only ever called from an explicit click in the settings window, with the
/// fingerprint of the preview the user looked at.
#[tauri::command]
fn agent_hooks_apply(agent: String, install: bool, fingerprint: String) -> Result<String, String> {
    let backups = agents::apply(&agent, install, &fingerprint)?;
    let done = if install { "installed" } else { "removed" };
    log::line(format!("agent hooks {done} for {agent}"));
    Ok(backups)
}

// ── Plan usage ────────────────────────────────────────────────────────────────

/// The diff of putting the plan usage relay into (or taking it out of) the
/// status line, before anything is written.
#[tauri::command]
fn status_line_preview(install: bool) -> Result<HookPreview, String> {
    hooks::status_line_preview(install)
}

/// Only ever called from an explicit click in the settings window.
#[tauri::command]
fn status_line_apply(
    app: AppHandle,
    shared: State<Shared>,
    install: bool,
    fingerprint: String,
) -> Result<String, String> {
    let backup = hooks::status_line_write(install, &fingerprint)?;
    let updated = {
        let mut current = shared.settings.lock().unwrap();
        current.plan_relay_installed = install;
        // As on the Mac: taking the relay out turns the pill off with it.
        if !install {
            current.show_plan_in_notch = false;
        }
        if let Err(err) = settings::save(&current) {
            log::line(format!("could not save settings: {err}"));
        }
        current.clone()
    };
    let _ = app.emit("settings-changed", updated);
    Ok(backup)
}

/// Codex plan usage, asked of the Codex CLI (`codex app-server`) when its pill
/// shows. Off the main thread: it can take a few seconds.
#[tauri::command]
async fn codex_plan_usage() -> Option<serde_json::Value> {
    tauri::async_runtime::spawn_blocking(codex_plan::read).await.ok().flatten()
}

/// Only the island may drive an approval: the buttons live there, and no other
/// window of the app (a feature's page, Settings) has any business answering
/// for the user.
fn from_island(window: &tauri::WebviewWindow) -> bool {
    window.label() == island::WINDOW_LABEL
}

#[tauri::command]
fn approval_decision(window: tauri::WebviewWindow, app: AppHandle, request_id: String, decision: String) {
    if !from_island(&window) {
        return;
    }
    recap::record_decision(&app, &request_id, &decision);
    pipe::answer(&app, &request_id, &decision);
}

/// An option picked on the island for a question Claude Code asked.
#[tauri::command]
fn approval_answer(
    window: tauri::WebviewWindow,
    app: AppHandle,
    request_id: String,
    answers: std::collections::HashMap<String, serde_json::Value>,
    other: Option<Vec<String>>,
) {
    if !from_island(&window) {
        return;
    }
    // An answered question is not an Allow / Deny: nothing for the recap.
    recap::forget_request(&app, &request_id);
    pipe::answer_question(&app, &request_id, &answers, &other.unwrap_or_default());
}

/// The island has the card on screen, so the long wait for a human may begin.
/// Until this arrives the relay only waits a few hundred milliseconds, which is
/// what stops a paused or unresponsive island from freezing Claude Code.
#[tauri::command]
fn approval_ack(window: tauri::WebviewWindow, app: AppHandle, request_id: String) {
    if !from_island(&window) {
        return;
    }
    pipe::acknowledge(&app, &request_id);
}

/// Nobody can act on this request — the island is paused, or another card is
/// already up. Claude Code falls back to asking in the terminal immediately.
#[tauri::command]
fn approval_decline(window: tauri::WebviewWindow, app: AppHandle, request_id: String) {
    if !from_island(&window) {
        return;
    }
    recap::forget_request(&app, &request_id);
    pipe::decline(&app, &request_id);
}

// ── Chat, files and secrets ───────────────────────────────────────────────────

/// One chat turn with the provider picked in the chat view. API keys and any
/// file bytes stay on the Rust side.
#[tauri::command]
async fn chat_send(
    app: AppHandle,
    shared: State<'_, Shared>,
    chat: State<'_, Chat>,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    registry::require(&app, "chat")?;
    let settings = shared.settings.lock().unwrap().clone();
    chat::send(&app, &chat, &settings, query, context).await
}

/// The models a provider offers, for the picker in the chat view. Only asked
/// once the user picked that provider, and only with its key or address.
#[tauri::command]
async fn chat_models(app: AppHandle, shared: State<'_, Shared>, provider: String) -> Result<Vec<ModelInfo>, String> {
    registry::require(&app, "chat")?;
    let settings = shared.settings.lock().unwrap().clone();
    chat::models(&settings, &provider).await
}

/// Settings → Local models → Connect: does the server answer, and with which models?
#[tauri::command]
async fn local_connect(provider: String, url: String) -> Result<local_chat::Connected, String> {
    local_chat::connect(&provider, &url).await
}

/// Stores the custom server's key for the address typed next to it; it is only
/// ever sent to that address.
#[tauri::command]
fn local_set_key(url: String, key: String) -> Result<(), String> {
    local_chat::set_custom_key(&url, &key)
}

#[tauri::command]
fn chat_reset(chat: State<Chat>) {
    chat.reset();
}

/// Copies a dropped file into the inbox and reports its name back.
#[tauri::command]
fn ingest_file(app: AppHandle, path: String) -> Result<DroppedFile, String> {
    registry::require(&app, "drop")?;
    files::ingest(&path)
}

/// The island may only ask whether a key exists — never read it.
#[tauri::command]
fn secret_present(key: String) -> bool {
    secrets::present(&key)
}

#[tauri::command]
fn secret_set(app: AppHandle, key: String, value: String) -> Result<(), String> {
    // Bound to its server's address: only local_set_key may store it.
    if key == local_chat::CUSTOM_KEY {
        return Err("use local_set_key".into());
    }
    // An address a key would not be sent to is not stored either (net.rs).
    if (key == "n8n-url" || key == "forge-url") && !value.trim().is_empty() {
        net::keyed_url(&value)?;
    }
    let before = (key == "github-token").then(|| secrets::get(&key));
    secrets::set(&key, &value)?;
    if let Some(before) = before {
        if secrets::get(&key) != before {
            integrations::github_token_changed(&app);
        }
    }
    Ok(())
}

#[tauri::command]
fn secret_clear(app: AppHandle, key: String) -> Result<(), String> {
    let had = key == "github-token" && secrets::present(&key);
    secrets::clear(&key)?;
    if had {
        integrations::github_token_changed(&app);
    }
    Ok(())
}

/// Opens the configured n8n instance — the URL lives in the Credential Manager.
#[tauri::command]
fn open_n8n() {
    let Some(raw) = secrets::get("n8n-url") else { return };
    let Ok(mut url) = reqwest::Url::parse(raw.trim()) else { return };
    // Without its user name and password: the launcher would carry them on its
    // command line, which any program of the session can read, and from there
    // they would land in the browser's history. The browser asks if it needs them.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    open_url(url.to_string());
}

/// Refresh buttons in the integration cards.
#[tauri::command]
async fn refresh_integration(app: AppHandle, id: String) {
    if registry::is_running(&app, "integrations") {
        integrations::poll_once(app, &id).await;
    }
}

/// The GitHub card was opened: refetch its `section` ("pulse" or "activity")
/// if it is stale. The pollers still decline while the pill is off or paused.
#[tauri::command]
fn github_refresh(app: AppHandle, section: String, dirs: Option<Vec<String>>) {
    if let Some(dirs) = dirs {
        github::set_session_dirs(dirs);
    }
    if registry::is_running(&app, "integrations") {
        integrations::github_refresh_if_stale(&section);
    }
}

/// Lets the island write to the same log as the Rust side.
#[tauri::command]
fn log_line(message: String) {
    log::line(format!("ui  {message}"));
}

// ── Global shortcuts ──────────────────────────────────────────────────────────

/// How each global shortcut went: registered, taken by another app, and so on.
#[tauri::command]
fn shortcuts_status(app: AppHandle) -> shortcuts::Report {
    shortcuts::status(&app)
}

/// Settings is recording a new combination: let go of ours meanwhile, so the
/// keys reach the recorder instead of running an action. `false` takes them back.
#[tauri::command]
fn shortcuts_suspend(app: AppHandle, shared: State<Shared>, hooks: State<HooksHeld>, suspended: bool) {
    // The input hooks too (remaps, snippets, the wheel), so a remapped key can be recorded.
    *hooks.0.lock().unwrap() = suspended.then(|| svc_input::suspend_all("recorder"));
    if suspended {
        shortcuts::suspend(&app);
    } else {
        let stored = shared.settings.lock().unwrap().shortcuts.clone();
        shortcuts::apply(&app, &stored);
    }
}

/// The input hooks stand still while Settings records a shortcut.
#[derive(Default)]
struct HooksHeld(Mutex<Option<svc_input::Suspended>>);

/// `Lintel --shelf-add <paths>`: what the shelf's opt-in Explorer menu and Send To entry run.
fn shelf_add(app: &AppHandle, args: &[String]) -> bool {
    let Some(at) = args.iter().position(|a| a == "--shelf-add") else { return false };
    let (app, paths) = (app.clone(), args[at + 1..].to_vec());
    tauri::async_runtime::spawn_blocking(move || {
        let _ = registry::call(&app, "shelf", "added", serde_json::json!({ "paths": paths }));
    });
    true
}

// ── Settings window ───────────────────────────────────────────────────────────

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    winfactory::show_settings(&app);
}

pub fn run() {
    // One-time: the first start after the code-named build (crates/core/src/migrate.rs). A later version may drop it.
    let migrated = notch_core::migrate::from_code_name();
    let loaded = settings::load();
    i18n::set_picked(&loaded.language);
    let gate = Arc::new(PollGate::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // `Lintel --shortcut <action>`: what a desktop's own keyboard
            // settings run where we can't listen for keys ourselves (Wayland).
            if shelf_add(app, &argv) {
                return;
            }
            // The relay tried to start an app that already runs: nothing to do,
            // and the island is not opened for it.
            if argv.iter().any(|a| a == AUTO_LAUNCHED) {
                return;
            }
            match shortcuts::from_args(&argv) {
                Some(action) => shortcuts::dispatch(app, action),
                None => {
                    let _ = app.emit_to(island::WINDOW_LABEL, "tray", "open".to_string());
                }
            }
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(shortcuts::plugin())
        .manage(Shared {
            settings: Mutex::new(loaded.clone()),
            gate: gate.clone(),
        })
        .manage(Pending::default())
        .manage(Chat::default())
        .manage(shortcuts::Registry::default())
        .manage(HooksHeld::default())
        .manage(recap::load())
        .manage(registry::Registry::new())
        .manage(brand_icon::WindowIcon::default())
        .invoke_handler(tauri::generate_handler![
            boot,
            save_settings,
            set_system_languages,
            set_collapsed,
            set_island_rect,
            island_displays,
            island_place,
            island_drag,
            focus_window,
            set_island_keys,
            set_outside_watch,
            set_poll_rest,
            feature_action,
            reposition,
            fit_zoom,
            list_monitors,
            open_url,
            open_in_vscode,
            open_session,
            auto_launched,
            running_sessions,
            session_file_lines,
            open_file_in_vscode,
            quit_app,
            hooks_status,
            agent_hooks_status,
            hooks_preview,
            hooks_apply,
            agent_hooks_list,
            agent_hooks_preview,
            agent_hooks_apply,
            wsl::wsl_hooks_list,
            wsl::wsl_hooks_preview,
            wsl::wsl_hooks_apply,
            remote::ssh_hosts,
            remote::ssh_hosts_set,
            remote::ssh_hooks_preview,
            remote::ssh_hooks_apply,
            status_line_preview,
            status_line_apply,
            codex_plan_usage,
            approval_decision,
            approval_answer,
            approval_ack,
            approval_decline,
            log_line,
            chat_send,
            chat_models,
            local_connect,
            local_set_key,
            chat_reset,
            ingest_file,
            secret_present,
            secret_set,
            secret_clear,
            refresh_integration,
            github_refresh,
            open_n8n,
            open_settings_window,
            set_paused,
            tray::tray_icon,
            brand_icon::window_icon,
            brand_icon::shortcut_icon,
            shortcuts_status,
            shortcuts_suspend,
            recap::recap_history,
            recap::recap_prefs,
            recap::recap_set_enabled,
            recap::recap_set_hide_projects,
            recap::recap_mark_shown,
            recap::recap_clear,
            recap::recap_save_png,
            recap::recap_reveal_saved,
            registry::features_list,
            registry::feature_call,
            registry::feature_settings,
            registry::feature_patch_settings,
            registry::feature_set_enabled,
            sounds::custom_sounds,
            sounds::custom_sound,
            sounds::set_custom_sound,
            sounds::reset_custom_sound,
            sounds::reveal_sounds_folder,
            sounds::reload_sounds,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            log::line(format!("--- Lintel {} started ---", env!("CARGO_PKG_VERSION")));
            if let Some(what) = &migrated {
                log::line(what);
            }
            watchdog::start();
            winfactory::create_settings(&handle);
            // The Run entry names the exe it was written for: a moved or reinstalled app is
            // written again with the path it runs from now.
            if loaded.autostart {
                let _ = handle.autolaunch().enable();
            }
            // Every feature that is switched on, before the island shows: the
            // relay pipe, the pollers, the drop handler. A disabled one starts
            // nothing. The tray menu and the shortcuts are made from what runs.
            webview_drop::install(&handle);
            webview_permission::install(&handle);
            // Two starts in a row that did not last twenty seconds are a crash loop: this
            // one comes up with the features off, so the tray and Settings can be reached.
            // Counted here, not in `run`: a second launch that only hands over its
            // arguments never gets this far.
            let config = notch_core::win::config_dir();
            let unfinished = notch_feat_cleaner_uninstaller::startup::begin(&config);
            if unfinished >= 2 {
                log::line(format!("the last {unfinished} starts did not finish: the features stay off for this run"));
            } else {
                registry::start_enabled(&handle);
                shelf_add(&handle, &std::env::args().collect::<Vec<_>>());
            }
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(20));
                notch_feat_cleaner_uninstaller::startup::settled(&config);
            });
            tray::build(&handle)?;

            if let Some(win) = island::window(&handle) {
                platform::island_ready(&handle, &win);
                island::apply_geometry(&handle, &loaded.screen, false);
                let _ = win.show();
            }
            gate.collapsed.store(false, Ordering::Relaxed);
            gate.set_active(true);
            island::spawn_cursor_poll(handle.clone(), gate.clone());

            shortcuts::apply(&handle, &loaded.shortcuts);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running Lintel")
        .run(|app, event| {
            // Quit: every feature gets its `stop`, and the keyboard hook goes.
            if let tauri::RunEvent::Exit = event {
                platform::keyhook::remove();
                registry::stop_all(app);
                notch_feat_cleaner_uninstaller::startup::settled(&notch_core::win::config_dir());
            }
        });
}

#[cfg(test)]
mod tests {
    use super::diff_file;

    #[test]
    fn only_an_existing_file_by_its_full_path_reaches_the_editor() {
        let dir = std::env::temp_dir().join(format!("lintel-diff-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("edited.ts");
        std::fs::write(&file, "x").unwrap();

        assert!(diff_file(&file.to_string_lossy()).is_some());
        // A folder, a missing file, a relative path or an option never pass.
        assert!(diff_file(&dir.to_string_lossy()).is_none());
        assert!(diff_file(&dir.join("missing.ts").to_string_lossy()).is_none());
        assert!(diff_file("edited.ts").is_none());
        assert!(diff_file("--help").is_none());
        assert!(diff_file("").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
