//! lintel-hook — the relay Claude Code (and every other agent) runs on each hook
//! event.
//!
//! Reads the hook JSON on stdin, maps the agent's event and field names onto
//! Claude Code's (normalize.rs), adds a little terminal context, and hands it to
//! Lintel over the named pipe `\\.\pipe\lintel-<sid>` .
//!
//! Hard rule (docs/CLAUDE.md): **never block the agent.**
//! * If the pipe does not exist — Lintel is closed — we exit 0 immediately, with
//!   only the "no opinion" reply the agent expects (reply.rs), and the session
//!   carries on untouched.
//! * Every step runs under a deadline enforced by the main thread, so a pipe that
//!   accepts the connection and then stops reading cannot wedge the session
//!   either: we abandon the worker and exit.
//! * Only `PermissionRequest` waits for an answer, because approving from the
//!   island is the whole point. No answer means no decision, and the agent asks
//!   in its terminal exactly as if Lintel were not installed.
//!
//! Usage: `lintel-hook [--launch] [--agent <name>] [<EventName>]` (the event name
//! is also read from the JSON; `--agent` is absent for Claude Code; `--launch`
//! is the opt-in that starts the app when it is closed, see launch.rs), or
//! `lintel-hook --statusline` as Claude Code's status line command (plan usage,
//! see statusline.rs): it passes the plan limits on and runs the status line the
//! user had before, so that keeps working.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

mod launch;
mod normalize;
mod remote;
mod reply;

/// Budget for getting a pipe connection. Beyond this the agent wins, always.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
/// Whole-run budget for an event nobody waits on: connect and write, no more.
const FIRE_AND_FORGET_BUDGET: Duration = Duration::from_secs(2);
/// How long a permission prompt may stay on screen before the terminal takes over.
const DECISION_BUDGET: Duration = Duration::from_secs(110);
/// How often a waiting relay checks that the process that ran it is still there.
const PARENT_POLL: Duration = Duration::from_millis(100);
/// Set by the app's own `claude -p` runs (src-tauri/src/chat/tasks.rs): a
/// headless run somebody watches, which keeps its pill.
const WATCHED: &str = "LINTEL_WATCHED";

/// Fields that are pointless to forward and can be enormous (a whole file read,
/// a full command output). The island never shows them.
const DROPPED_FIELDS: &[&str] = &["tool_response", "tool_output", "transcript_path"];
/// Longest string forwarded for any single field; the island truncates to far
/// less than this anyway.
const MAX_FIELD_LEN: usize = 2_000;
/// A turn's last words (`last_assistant_message` of Stop) are what the finished
/// card shows whole, in a box that scrolls: kept far longer than the rest.
const MAX_ANSWER_LEN: usize = 32 * 1024;

mod statusline;

/// The live diff needs the whole text of a file edit, once it has happened:
/// PostToolUse of these tools keeps its edit strings far longer than the rest.
const DIFF_TOOLS: &[&str] = &[
    "Edit", "MultiEdit", "Write",
    // Codex's patch, Antigravity's file tools, Kiro's file tool.
    "apply_patch", "write_to_file", "replace_file_content", "multi_replace_file_content", "fs_write", "write",
];
/// The `tool_input` keys holding the text being replaced or written (a patch is
/// Codex's `command`).
const DIFF_FIELDS: &[&str] = &[
    "old_string", "new_string", "content", "command",
    "CodeContent", "TargetContent", "ReplacementContent", "targetContent", "replacementContent",
    "old_str", "new_str", "file_text",
];
/// Per edit string. The island stops diffing at 200 KB anyway (DiffEngine).
const MAX_DIFF_FIELD_LEN: usize = 256 * 1024;
/// For all edit strings of one event together, so the line stays well under the
/// 1 MiB the app reads from the pipe even once JSON-escaped.
const MAX_DIFF_TOTAL: usize = 512 * 1024;
/// Stdin comes from the agent: no more than this is ever held. A real payload is
/// kilobytes, and the rest would only be cut away again. (This cap, the line
/// cap and the reply cap are ported from upstream `windows/hook/src/main.rs`,
/// pull request 390.)
const MAX_STDIN: u64 = 16 * 1024 * 1024;
/// The whole line must stay under the 1 MiB the app reads from the pipe. Cutting
/// strings bounds the values; this bounds the line (keys, number of fields).
const MAX_LINE: usize = 900 * 1024;
/// The longest answer is one `{"answers":{…}}` line: a server that keeps
/// sending is not one we go on reading.
const MAX_REPLY: usize = 64 * 1024;

mod win;

/// What the command line says: `--agent <name>`, `--launch` and the event name.
#[derive(Default)]
struct Args {
    agent: String,
    event: String,
    /// The hook was installed with the opt-in that starts the app (launch.rs).
    launch: bool,
}

fn args() -> Args {
    parse_args(std::env::args().skip(1))
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Args {
    let mut args = Args::default();
    while let Some(arg) = it.next() {
        if arg == "--agent" {
            args.agent = it.next().unwrap_or_default();
        } else if arg == "--launch" {
            args.launch = true;
        } else if args.event.is_empty() {
            args.event = arg;
        }
    }
    args
}

/// What became of one event.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Nobody serves the pipe: the app is closed.
    NoServer,
    /// Handed over, with the app's word when one was waited for and given.
    /// `None` is "no decision": the agent asks in its terminal.
    Sent(Option<String>),
}

impl Outcome {
    fn answer(&self) -> Option<&str> {
        match self {
            Outcome::Sent(answer) => answer.as_deref(),
            Outcome::NoServer => None,
        }
    }
}

/// One event, ready to forward.
struct Event {
    /// The payload as one line of JSON.
    line: String,
    /// The canonical event name.
    name: String,
    /// For Claude Code's AskUserQuestion, the question as it was asked.
    question: Option<Value>,
    /// Claude Code's own `permission_suggestions`, kept whole for the Always button.
    suggestions: Option<Value>,
    /// For Claude Code's ExitPlanMode, its input as it was: an allow sends it back.
    plan: Option<Value>,
}

fn main() {
    if std::env::args().skip(1).any(|a| a == "--statusline") {
        statusline::run();
    }
    let args = args();
    let mut raw = Vec::new();
    let _ = std::io::stdin().take(MAX_STDIN).read_to_end(&mut raw);
    // An SSH host's event, run by the app for its tunnel (remote.rs).
    let remote = remote::host();
    if let Some(host) = &remote {
        let Some(tagged) = remote::tag(&raw, host) else { std::process::exit(0) };
        raw = tagged;
    }
    let env = |key: &str| std::env::var(key).ok();
    // Nobody watches it: Claude Code's "no opinion" (silence), and nothing sent.
    if headless(&args.agent, &env) {
        std::process::exit(0);
    }
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    // A host's session has no window here: the app runs us, not its terminal.
    let ancestors = if remote.is_some() { Vec::new() } else { win::ancestors() };

    let Some(event) = prepare(&raw, &args, &env, &cwd, &ancestors) else {
        // Nothing we could forward. An agent that needs JSON still gets its
        // "no opinion" — Copilot is fail-closed and would deny without it.
        let name = normalize::event(&args.event);
        print(reply::stdout(&args.agent, name, None, None));
        std::process::exit(0);
    };

    // Only an agent whose decisions the island can give waits for one; any other
    // would be held for nothing, its decision being thrown away (reply.rs).
    let waits_for_answer = event.name == "PermissionRequest" && reply::takes_decisions(&args.agent);
    let budget = if waits_for_answer { DECISION_BUDGET } else { FIRE_AND_FORGET_BUDGET };

    // A prompt answered in the terminal: Claude Code ends the shell that ran us
    // (Git Bash), and its going ends our wait, so the app sees us hang up and
    // takes the card down. Only a request that waits watches anything.
    let parent = ancestors.first().filter(|_| waits_for_answer).and_then(|pid| win::Parent::of(*pid));
    let gone = || parent.as_ref().is_some_and(win::Parent::gone);
    let outcome = exchange(&win::pipe_path(), event.line.clone(), waits_for_answer, budget, &gone);
    let reply = match (&event.plan, outcome.answer()) {
        (Some(plan), Some(decision)) if reply::is_claude_code(&args.agent) => reply::plan_json(decision, plan),
        _ => reply::stdout_with(&args.agent, &event.name, outcome.answer(), event.question.as_ref(), event.suggestions.as_ref()),
    };
    print(reply);
    if let Some((message, code)) = reply::exit_status(&args.agent, &event.name, outcome.answer()) {
        eprintln!("{message}");
        std::process::exit(code);
    }
    // The app is closed and the user opted in: it is started for the turn that
    // begins, once the agent has its reply, and never waited for.
    if outcome == Outcome::NoServer && args.launch && launch::starts(&event.name) {
        launch::app();
    }
    std::process::exit(0);
}

/// Hands `line` to the app on `pipe` and, when `waits`, waits for its word:
/// never past `budget`. The worker owns every blocking call. If it overruns
/// the budget we simply stop listening and the caller exits: the process dying
/// takes the pipe handle with it. Running out of time is "no decision", exactly
/// like an app that answered nothing. (No catch_unwind here — the release
/// profile is panic = "abort", so it would be dead code. `talk_on` is written
/// to have nothing to panic on instead.) `gone` is asked every PARENT_POLL:
/// true ends the wait at once, as no decision (the process that ran us left).
fn exchange(pipe: &str, line: String, waits: bool, budget: Duration, gone: &dyn Fn() -> bool) -> Outcome {
    let (tx, rx) = mpsc::channel::<Outcome>();
    let pipe = pipe.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(talk_on(&pipe, &line, waits));
    });
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(PARENT_POLL)) {
            Ok(outcome) => return outcome,
            Err(mpsc::RecvTimeoutError::Timeout) if left > PARENT_POLL && !gone() => {}
            Err(_) => return Outcome::Sent(None),
        }
    }
}

fn print(line: Option<String>) {
    if let Some(line) = line {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// The payload to forward, from the raw stdin bytes. `env` reads an environment
/// variable, `cwd` is the working directory and `ancestors` the processes we
/// run under (win::ancestors), so tests stay pure.
fn prepare(
    raw: &[u8],
    args: &Args,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &str,
    ancestors: &[u32],
) -> Option<Event> {
    // Some shells hand us a UTF-8 BOM; serde_json would choke on it.
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let mut payload = serde_json::from_slice::<Value>(raw).ok()?;
    let map = payload.as_object_mut()?;

    // Cursor also runs the Claude Code hooks of ~/.claude/settings.json, with its
    // own payload. Its own hooks (--agent cursor) already report the same events
    // on its pill; here they would show up as a Claude Code session.
    if args.agent.is_empty() && map.contains_key("cursor_version") {
        return None;
    }

    // Which agent this hook was installed for, so the app routes it to the right
    // pill. Absent means Claude Code, so existing hook commands keep working
    // unchanged; invalid names are discarded by the app, not here. A Claude Code
    // session started from the Claude desktop app is tagged `claude-desktop`.
    if let Some(tag) = agent_tag(&args.agent, env) {
        map.insert("lintel_agent".into(), Value::String(tag));
    }
    // Claude Code in Cursor's terminal goes on the Cursor pill (Mac #120).
    if !map.contains_key("term_editor") {
        if let Some(editor) = term_editor(env) {
            map.insert("term_editor".into(), Value::String(editor.into()));
        }
    }

    let raw_event = map
        .get("hook_event_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| args.event.clone());
    normalize::fields(map, env);
    let name = normalize::for_agent(&args.agent, normalize::refine(normalize::event(&raw_event), map), map);
    map.insert("hook_event_name".into(), Value::String(name.clone()));

    for field in DROPPED_FIELDS {
        map.remove(*field);
    }

    if map.get("cwd").and_then(Value::as_str).map(str::is_empty).unwrap_or(true) && !cwd.is_empty() {
        map.insert("cwd".into(), Value::String(cwd.to_string()));
    }

    add_terminal_context(map, env);

    // The pill's name: Claude Code's own record of the session holds the folder it
    // started in and a /rename title. Read only on the events that can name a pill.
    if !map.contains_key("lintel_agent") && ["SessionStart", "UserPromptSubmit", "Stop"].contains(&name.as_str()) {
        let id = map.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
        if let Some((root, title)) = session_record(env, &id) {
            map.insert("session_root".into(), Value::String(root));
            map.insert("session_title".into(), Value::String(title));
        }
    }

    // Where the app finds the session's window ("Open terminal"): ours, never
    // the agent's word for it.
    if ancestors.is_empty() {
        map.remove("lintel_ancestors");
    } else {
        map.insert("lintel_ancestors".into(), ancestors.iter().copied().map(Value::from).collect());
    }

    // Kept whole: what goes back to Claude Code must be its own input, not the
    // shortened copy the island is shown.
    let tool = map.get("tool_name").and_then(Value::as_str);
    let kept = |name: &str| (tool == Some(name)).then(|| map.get("tool_input").cloned()).flatten();
    let question = kept("AskUserQuestion");
    let plan = kept("ExitPlanMode");

    // Antigravity's allow carries the grant for exactly this call (reply.rs).
    let suggestions = map.get("permission_suggestions").cloned().or_else(|| {
        (args.agent == "antigravity").then(|| serde_json::json!({ "name": tool, "args": map.get("tool_input") }))
    });

    // Every string is capped, except the edit text of a finished Edit /
    // MultiEdit / Write, which the live diff needs whole (within its own limits).
    truncate_payload(&mut payload, &name);

    let mut line = payload.to_string();
    if line.len() > MAX_LINE {
        // Keys and the number of fields are not cut by truncate_payload, so the
        // line can still be too long. Send what the island needs rather than a
        // line the app would cut in the middle and drop as invalid JSON.
        let mut minimal = Map::new();
        for key in ["hook_event_name", "session_id", "tool_name", "lintel_agent", "cwd", "lintel_ancestors"] {
            if let Some(v) = payload.get(key) {
                minimal.insert(key.into(), v.clone());
            }
        }
        minimal.insert("lintel_truncated".into(), Value::Bool(true));
        line = Value::Object(minimal).to_string();
    }
    line.push('\n');
    Some(Event { line, name, question, suggestions, plan })
}

/// The folder a Claude Code session started in and its /rename title ("" when it was never
/// renamed: a name Claude Code made up itself is `derived`), from `<config dir>/sessions/<pid>.json`.
/// Bounded: small files only, and a few hundred of them at most.
fn session_record(env: &dyn Fn(&str) -> Option<String>, id: &str) -> Option<(String, String)> {
    if id.is_empty() {
        return None;
    }
    let base = env("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| env("USERPROFILE").map(|h| std::path::PathBuf::from(h).join(".claude")))?;
    for entry in std::fs::read_dir(base.join("sessions")).ok()?.take(300).flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") || entry.metadata().ok()?.len() > 8192 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if v["sessionId"].as_str() != Some(id) {
            continue;
        }
        let root = v["cwd"].as_str().unwrap_or_default().to_string();
        let title = if v["nameSource"].as_str() == Some("derived") { "" } else { v["name"].as_str().unwrap_or_default() };
        return Some((root, title.to_string()));
    }
    None
}

/// `claude -p`, the Agent SDK and scheduled jobs: Claude Code says so with an
/// `sdk-…` entrypoint, and nobody watches them, so they make no pill and no
/// card. The app's own task chip marks its runs (WATCHED) and keeps its pill.
fn headless(agent: &str, env: &dyn Fn(&str) -> Option<String>) -> bool {
    reply::is_claude_code(agent)
        && env("CLAUDE_CODE_ENTRYPOINT").is_some_and(|e| e.starts_with("sdk-"))
        && env(WATCHED).as_deref() != Some("1")
}

/// Which terminal the session runs in. Unlike macOS, Lintel here accepts events
/// from every terminal, so this is context only — never a filter.
fn add_terminal_context(map: &mut Map<String, Value>, env: &dyn Fn(&str) -> Option<String>) {
    for (key, var) in [
        ("term_program", "TERM_PROGRAM"),
        ("wt_session", "WT_SESSION"),
        ("term_session_id", "TERM_SESSION_ID"),
        ("vscode_pid", "VSCODE_PID"),
        ("session_pid", "CLAUDE_CODE_SSE_PORT"),
    ] {
        if !map.contains_key(key) {
            map.insert(key.into(), Value::String(env(var).unwrap_or_default()));
        }
    }
}

/// The `lintel_agent` tag: `--agent` when given, otherwise `claude-desktop` for
/// a Claude Code session started from the Claude desktop app, which says so in
/// CLAUDE_CODE_ENTRYPOINT — the same rule as the Mac's relay (#191). Nothing
/// for a plain Claude Code session.
fn agent_tag(arg: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if !arg.is_empty() {
        return Some(arg.to_string());
    }
    (env("CLAUDE_CODE_ENTRYPOINT").as_deref() == Some("claude-desktop"))
        .then(|| "claude-desktop".to_string())
}

/// `cursor` when the session runs in Cursor's integrated terminal. Cursor sets
/// TERM_PROGRAM=vscode like VS Code does, so it is told apart by its own trace
/// variable, or by its executable behind VS Code's git helper.
fn term_editor(env: &dyn Fn(&str) -> Option<String>) -> Option<&'static str> {
    if env("CURSOR_TRACE_ID").is_some_and(|v| !v.is_empty()) {
        return Some("cursor");
    }
    // A JetBrains IDE's terminal (JediTerm) says so in TERMINAL_EMULATOR.
    if env("TERMINAL_EMULATOR").is_some_and(|v| v.starts_with("JetBrains")) {
        return Some("jetbrains");
    }
    let helper = env("VSCODE_GIT_ASKPASS_NODE").unwrap_or_default();
    let exe = helper.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase();
    exe.starts_with("cursor").then_some("cursor")
}

/// Caps the strings of a payload: every field to MAX_FIELD_LEN, except the edit
/// strings of a finished Edit / MultiEdit / Write, which the live diff needs
/// whole. If even those had to be cut, `lintel_diff_truncated` tells the island
/// not to show counts it cannot trust.
fn truncate_payload(payload: &mut serde_json::Value, event: &str) {
    let keeps_diff = event == "PostToolUse"
        && payload
            .get("tool_name")
            .and_then(|v| v.as_str())
            .is_some_and(|tool| DIFF_TOOLS.contains(&tool));
    let input = if keeps_diff {
        payload.as_object_mut().and_then(|map| map.remove("tool_input"))
    } else {
        None
    };
    let answer = if event == "Stop" {
        payload.as_object_mut().and_then(|map| map.remove("last_assistant_message"))
    } else {
        None
    };

    truncate_strings(payload);

    if let Some(mut answer) = answer {
        match &mut answer {
            Value::String(s) => {
                cut(s, MAX_ANSWER_LEN);
            }
            other => truncate_strings(other),
        }
        if let Some(map) = payload.as_object_mut() {
            map.insert("last_assistant_message".into(), answer);
        }
    }

    if let Some(mut input) = input {
        let mut budget = MAX_DIFF_TOTAL;
        let mut cut_any = false;
        cap_diff_strings(&mut input, &mut budget, &mut cut_any);
        if let Some(map) = payload.as_object_mut() {
            map.insert("tool_input".into(), input);
            if cut_any {
                map.insert("lintel_diff_truncated".into(), serde_json::Value::Bool(true));
            }
        }
    }
}

/// `tool_input` of a diff tool: edit strings share MAX_DIFF_TOTAL, each capped at
/// MAX_DIFF_FIELD_LEN; any other string gets the ordinary cap.
fn cap_diff_strings(value: &mut serde_json::Value, budget: &mut usize, cut_any: &mut bool) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                match v {
                    serde_json::Value::String(s) if DIFF_FIELDS.contains(&key.as_str()) => {
                        let limit = MAX_DIFF_FIELD_LEN.min(*budget);
                        if cut(s, limit) {
                            *cut_any = true;
                        }
                        *budget = budget.saturating_sub(s.len());
                    }
                    _ => cap_diff_strings(v, budget, cut_any),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                cap_diff_strings(item, budget, cut_any);
            }
        }
        serde_json::Value::String(s) => {
            cut(s, MAX_FIELD_LEN);
        }
        _ => {}
    }
}

/// Caps every string in the payload. A single Write can carry a whole file.
fn truncate_strings(value: &mut Value) {
    match value {
        Value::String(s) => {
            cut(s, MAX_FIELD_LEN);
        }
        Value::Array(items) => items.iter_mut().for_each(truncate_strings),
        Value::Object(map) => map.values_mut().for_each(truncate_strings),
        _ => {}
    }
}

/// Shortens `s` to at most `max` bytes plus an ellipsis; true if it was cut.
fn cut(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    // Cut on a char boundary; a lone byte index can split UTF-8.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push('…');
    true
}

/// Hands one line to the app, for the status line relay: nothing is waited for
/// unless asked.
fn talk(payload: &str, waits_for_answer: bool) -> Option<String> {
    talk_on(&win::pipe_path(), payload, waits_for_answer).answer().map(str::to_string)
}

/// Connect, send, and — for a permission request — wait for the island's word.
fn talk_on(pipe: &str, payload: &str, waits_for_answer: bool) -> Outcome {
    let Some(mut pipe) = win::connect_to(pipe) else { return Outcome::NoServer };

    if pipe.write_all(payload.as_bytes()).is_err() {
        return Outcome::Sent(None);
    }
    let _ = pipe.flush();

    if !waits_for_answer {
        return Outcome::Sent(None);
    }

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if chunk[..n].contains(&b'\n') || buf.len() > MAX_REPLY {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let answer = String::from_utf8_lossy(&buf).trim().to_string();
    Outcome::Sent((!answer.is_empty()).then_some(answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(raw: &str, agent: &str, event: &str) -> (Value, Event) {
        let args = Args { agent: agent.into(), event: event.into(), launch: false };
        let ev = prepare(raw.as_bytes(), &args, &|_| None, "/home/me/here", &[]).expect("forwarded");
        let v = serde_json::from_str(ev.line.trim_end()).unwrap();
        (v, ev)
    }

    fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn claude_desktop_sessions_are_tagged_and_an_explicit_agent_wins() {
        let desktop = env_of(&[("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")]);
        assert_eq!(agent_tag("", &desktop).as_deref(), Some("claude-desktop"));
        assert_eq!(agent_tag("gemini", &desktop).as_deref(), Some("gemini"));
        assert_eq!(agent_tag("", &env_of(&[("CLAUDE_CODE_ENTRYPOINT", "cli")])), None);
        assert_eq!(agent_tag("", &env_of(&[])), None);
    }

    #[test]
    fn headless_runs_stay_off_the_island_unless_the_app_watches_them() {
        let headless_with = |agent: &str, vars: &'static [(&'static str, &'static str)]| headless(agent, &env_of(vars));
        assert!(headless_with("", &[("CLAUDE_CODE_ENTRYPOINT", "sdk-cli")]));
        assert!(headless_with("", &[("CLAUDE_CODE_ENTRYPOINT", "sdk-py")]));
        assert!(!headless_with("", &[("CLAUDE_CODE_ENTRYPOINT", "sdk-cli"), ("LINTEL_WATCHED", "1")]));
        assert!(!headless_with("", &[("CLAUDE_CODE_ENTRYPOINT", "cli")]));
        assert!(!headless_with("", &[("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")]));
        assert!(!headless_with("", &[]));
        // Only Claude Code says how it was started: another agent is never skipped.
        assert!(!headless_with("codex", &[("CLAUDE_CODE_ENTRYPOINT", "sdk-cli")]));
    }

    #[test]
    fn the_processes_we_run_under_go_with_the_event_and_only_ours() {
        let args = Args::default();
        let raw = br#"{"hook_event_name":"Stop","session_id":"s","lintel_ancestors":[1,2]}"#;
        let ev = prepare(raw, &args, &|_| None, "/p", &[40, 30, 20]).unwrap();
        let v: Value = serde_json::from_str(ev.line.trim_end()).unwrap();
        assert_eq!(v["lintel_ancestors"], serde_json::json!([40, 30, 20]));
        let ev = prepare(raw, &args, &|_| None, "/p", &[]).unwrap();
        let v: Value = serde_json::from_str(ev.line.trim_end()).unwrap();
        assert!(v.get("lintel_ancestors").is_none());
        // The real walk: something runs us, and never ourselves.
        let up = win::ancestors();
        assert!(!up.is_empty() && !up.contains(&std::process::id()));
    }

    #[test]
    fn a_plan_is_kept_whole_for_the_allow() {
        let long = "p".repeat(3000);
        let raw = format!(r#"{{"hook_event_name":"PermissionRequest","tool_name":"ExitPlanMode","tool_input":{{"plan":"{long}"}}}}"#);
        let (v, ev) = run(&raw, "", "");
        assert_eq!(ev.plan.unwrap()["plan"].as_str().unwrap().len(), 3000);
        assert!(v["tool_input"]["plan"].as_str().unwrap().ends_with('…'));
        assert!(ev.question.is_none());
    }

    #[test]
    fn a_relay_whose_parent_left_stops_waiting_at_once() {
        let name = own_pipe("orphan");
        let (got, done) = stand_in(&name, Then::Silence);
        let checks = std::cell::Cell::new(0);
        let gone = || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        };
        let started = std::time::Instant::now();
        let outcome = exchange(&name, format!("{REQUEST}\n"), true, Duration::from_secs(20), &gone);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).as_deref(), Ok(REQUEST));
        assert_no_decision(&outcome);
        let _ = done.send(());
    }

    #[test]
    fn cursor_running_claude_code_hooks_is_dropped() {
        let raw = r#"{"hook_event_name":"preToolUse","tool_name":"Shell","cursor_version":"2026.10.14","conversation_id":"c"}"#;
        let args = Args { agent: String::new(), event: "PreToolUse".into(), launch: false };
        assert!(prepare(raw.as_bytes(), &args, &|_| None, "/p", &[]).is_none());
        let (v, _) = run(raw, "cursor", "");
        assert_eq!(v["lintel_agent"], "cursor");
    }

    #[test]
    fn a_jetbrains_terminal_is_told_apart() {
        assert_eq!(term_editor(&env_of(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")])), Some("jetbrains"));
        assert_eq!(term_editor(&env_of(&[("TERMINAL_EMULATOR", "other")])), None);
    }

    #[test]
    fn antigravity_keeps_the_call_for_its_exact_grant() {
        let raw = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"git status"}},"conversationId":"c"}"#;
        let (v, ev) = run(raw, "antigravity", "PreToolUse");
        assert_eq!(v["hook_event_name"], "PermissionRequest");
        assert_eq!(ev.suggestions.unwrap()["args"]["command"], "git status");
    }

    #[test]
    fn cursor_is_told_apart_from_vs_code() {
        assert_eq!(term_editor(&env_of(&[("CURSOR_TRACE_ID", "abc")])), Some("cursor"));
        assert_eq!(
            term_editor(&env_of(&[(
                "VSCODE_GIT_ASKPASS_NODE",
                r"C:\Users\me\AppData\Local\Programs\cursor\Cursor.exe"
            )])),
            Some("cursor")
        );
        assert_eq!(
            term_editor(&env_of(&[(
                "VSCODE_GIT_ASKPASS_NODE",
                r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe"
            )])),
            None
        );
        assert_eq!(term_editor(&env_of(&[("CURSOR_TRACE_ID", "")])), None);
        assert_eq!(term_editor(&env_of(&[("TERM_PROGRAM", "vscode")])), None);
    }

    #[test]
    fn long_strings_are_cut_on_a_char_boundary() {
        let mut v = serde_json::json!({ "tool_input": { "content": "é".repeat(4000) } });
        truncate_strings(&mut v);
        let s = v["tool_input"]["content"].as_str().unwrap();
        assert!(s.len() <= MAX_FIELD_LEN + 4);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn claude_code_payloads_are_forwarded_as_they_are() {
        let (v, ev) = run(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":"/p"}"#, "", "PreToolUse");
        assert_eq!(ev.name, "PreToolUse");
        assert!(v.get("lintel_agent").is_none());
        assert_eq!(v["cwd"], "/p");
        assert_eq!(v["tool_name"], "Bash");
    }

    #[test]
    fn a_line_that_is_still_too_long_becomes_a_minimal_event() {
        // A key is not cut by truncate_payload: with a giant one the event still
        // goes through, as a small well-formed line rather than one the app drops.
        let mut payload = Map::new();
        payload.insert("hook_event_name".into(), serde_json::json!("PermissionRequest"));
        payload.insert("session_id".into(), serde_json::json!("s1"));
        payload.insert("tool_name".into(), serde_json::json!("Bash"));
        payload.insert("k".repeat(MAX_LINE), serde_json::json!("v"));
        let raw = serde_json::to_vec(&payload).unwrap();
        let args = Args { agent: "codex".into(), event: String::new(), launch: false };
        let ev = prepare(&raw, &args, &|_| None, "/tmp", &[]).expect("forwarded");
        assert!(ev.line.len() < 2_000);
        let v: Value = serde_json::from_str(ev.line.trim_end()).unwrap();
        assert_eq!(v["hook_event_name"], "PermissionRequest");
        assert_eq!(v["tool_name"], "Bash");
        assert_eq!(v["lintel_agent"], "codex");
        assert_eq!(v["lintel_truncated"], true);
    }

    #[test]
    fn an_agents_event_is_tagged_and_renamed() {
        // Gemini CLI: its own name in the payload, ours on the command line.
        let (v, ev) = run(r#"{"hook_event_name":"BeforeTool","toolCall":{"name":"shell","args":{"CommandLine":"ls"}}}"#, "gemini", "PreToolUse");
        assert_eq!(ev.name, "PreToolUse");
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["lintel_agent"], "gemini");
        assert_eq!(v["tool_input"]["command"], "ls");
        assert_eq!(v["cwd"], "/home/me/here");

        // Copilot CLI sends no event name: the command line's camelCase one is used.
        let (_, ev) = run(r#"{"toolName":"bash"}"#, "copilot", "permissionRequest");
        assert_eq!(ev.name, "PermissionRequest");

        // Cursor: a stop that failed, and its tool output left behind.
        let (v, ev) = run(r#"{"hook_event_name":"stop","status":"error","tool_output":"huge"}"#, "cursor", "");
        assert_eq!(ev.name, "StopFailure");
        assert!(v.get("tool_output").is_none());
    }

    #[test]
    fn a_question_is_kept_whole_and_only_for_ask_user_question() {
        let long = "x".repeat(3000);
        let raw = format!(r#"{{"hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion","tool_input":{{"questions":[{{"question":"{long}"}}]}}}}"#);
        let (v, ev) = run(&raw, "", "");
        assert_eq!(ev.question.unwrap()["questions"][0]["question"].as_str().unwrap().len(), 3000);
        assert!(v["tool_input"]["questions"][0]["question"].as_str().unwrap().ends_with('…'));
        let (_, ev) = run(r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"}}"#, "", "");
        assert!(ev.question.is_none());
    }

    #[test]
    fn what_cannot_be_read_is_not_forwarded() {
        let args = Args { agent: "copilot".into(), event: "preToolUse".into(), launch: false };
        for raw in ["", "not json", "[1,2]"] {
            assert!(prepare(raw.as_bytes(), &args, &|_| None, "/", &[]).is_none());
        }
        // A BOM is not a reason to drop the event.
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(br#"{"hook_event_name":"Stop"}"#);
        assert!(prepare(&bom, &args, &|_| None, "/", &[]).is_some());
    }

    #[test]
    fn the_live_diff_rule_holds_through_the_normalised_relay() {
        let big = "z".repeat(10_000);
        // Claude Code's finished Edit: kept whole.
        let raw = format!(r#"{{"hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{{"old_string":"{big}","new_string":"a"}}}}"#);
        let (v, _) = run(&raw, "", "");
        assert_eq!(v["tool_input"]["old_string"].as_str().unwrap().len(), big.len());
        // The same edit before it happens, or an agent's event renamed onto
        // PreToolUse: the ordinary cap.
        let raw = format!(r#"{{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{{"old_string":"{big}"}}}}"#);
        let (v, _) = run(&raw, "", "");
        assert!(v["tool_input"]["old_string"].as_str().unwrap().len() <= MAX_FIELD_LEN + 4);
        let raw = format!(r#"{{"hook_event_name":"BeforeTool","tool_name":"Edit","tool_input":{{"content":"{big}"}}}}"#);
        let (v, ev) = run(&raw, "gemini", "");
        assert_eq!(ev.name, "PreToolUse");
        assert!(v["tool_input"]["content"].as_str().unwrap().len() <= MAX_FIELD_LEN + 4);
    }

    #[test]
    fn a_finished_edit_keeps_its_text_whole_for_the_live_diff() {
        let big = "line\n".repeat(4_000); // 20 KB, well past MAX_FIELD_LEN
        let mut v = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "/p/a.ts", "old_string": big, "new_string": big },
            "cwd": "x".repeat(4_000),
        });
        truncate_payload(&mut v, "PostToolUse");
        assert_eq!(v["tool_input"]["old_string"].as_str().unwrap().len(), big.len());
        assert_eq!(v["tool_input"]["new_string"].as_str().unwrap().len(), big.len());
        // Everything else keeps the ordinary cap, and nothing says "cut".
        assert!(v["cwd"].as_str().unwrap().len() <= MAX_FIELD_LEN + 4);
        assert!(v.get("lintel_diff_truncated").is_none());

        let mut multi = serde_json::json!({
            "tool_name": "MultiEdit",
            "tool_input": { "edits": [{ "old_string": big, "new_string": "x" }] },
        });
        truncate_payload(&mut multi, "PostToolUse");
        assert_eq!(multi["tool_input"]["edits"][0]["old_string"].as_str().unwrap().len(), big.len());
    }

    #[test]
    fn edits_are_still_capped_before_they_happen_and_for_other_tools() {
        let big = "é".repeat(4_000);
        for (event, tool) in [("PreToolUse", "Edit"), ("PermissionRequest", "Write"), ("PostToolUse", "Bash")] {
            let mut v = serde_json::json!({ "tool_name": tool, "tool_input": { "content": big } });
            truncate_payload(&mut v, event);
            assert!(v["tool_input"]["content"].as_str().unwrap().len() <= MAX_FIELD_LEN + 4, "{event} {tool}");
        }
    }

    #[test]
    fn a_turns_last_words_are_kept_for_the_finished_card_and_nothing_else_grows() {
        let long = "w".repeat(20_000);
        let raw = format!(r#"{{"hook_event_name":"Stop","last_assistant_message":"{long}","message":"{long}"}}"#);
        let (v, _) = run(&raw, "", "");
        assert_eq!(v["last_assistant_message"].as_str().unwrap().len(), long.len());
        assert!(v["message"].as_str().unwrap().len() <= MAX_FIELD_LEN + 4);
        // Past its own cap it is cut like anything else, and only a Stop carries it whole.
        let huge = "w".repeat(MAX_ANSWER_LEN + 100);
        let raw = format!(r#"{{"hook_event_name":"Stop","last_assistant_message":"{huge}"}}"#);
        let (v, _) = run(&raw, "", "");
        assert!(v["last_assistant_message"].as_str().unwrap().len() <= MAX_ANSWER_LEN + 4);
        let raw = format!(r#"{{"hook_event_name":"PreToolUse","last_assistant_message":"{long}"}}"#);
        let (v, _) = run(&raw, "", "");
        assert!(v["last_assistant_message"].as_str().unwrap().len() <= MAX_FIELD_LEN + 4);
    }

    #[test]
    fn the_launch_flag_is_read_from_the_command_line_and_nowhere_else() {
        let parse = |words: &[&str]| parse_args(words.iter().map(|w| w.to_string()));
        let plain = parse(&["UserPromptSubmit"]);
        assert!(!plain.launch);
        assert_eq!(plain.event, "UserPromptSubmit");
        let opted = parse(&["--launch", "--agent", "codex", "SessionStart"]);
        assert!(opted.launch);
        assert_eq!((opted.agent.as_str(), opted.event.as_str()), ("codex", "SessionStart"));
        // A payload cannot ask for it: the flag is not a field.
        let (v, _) = run(r#"{"hook_event_name":"UserPromptSubmit","launch":true,"--launch":true}"#, "", "");
        assert_eq!(v["hook_event_name"], "UserPromptSubmit");
        // Only the start of a turn starts the app: never a permission request.
        assert!(launch::starts("SessionStart") && launch::starts("UserPromptSubmit"));
        for other in ["PermissionRequest", "PreToolUse", "PostToolUse", "Stop", "SessionEnd", "Notification", ""] {
            assert!(!launch::starts(other), "{other}");
        }
    }

    // ── The relay and a pipe ─────────────────────────────────────────────────
    //
    // A stand-in for the app on a pipe name of the test's own (never the app's
    // pipe): it takes one connection, reads the request, and then does what the
    // test says. What is checked is the rule of CLAUDE.md: the relay never
    // blocks an agent, and never invents an answer.

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(
            name: *const u16, open_mode: u32, pipe_mode: u32, max_instances: u32, out_size: u32, in_size: u32,
            default_timeout: u32, security: *const core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn ConnectNamedPipe(pipe: *mut core::ffi::c_void, overlapped: *mut core::ffi::c_void) -> i32;
    }

    /// What the stand-in does once it has read the request.
    enum Then {
        /// Holds the connection and says nothing: a card nobody answers.
        Silence,
        /// Goes away: the app quit, or released the request.
        Close,
        /// Writes this line: a click on the card.
        Say(&'static str),
    }

    /// A pipe name of this test's own.
    fn own_pipe(tag: &str) -> String {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!(r"\\.\pipe\{}-hook-test-{tag}-{}-{nanos}", notch_brand::SLUG, std::process::id())
    }

    /// Serves one connection on `name`. Returns what the relay sent, and a
    /// handle that lets a silent stand-in go once the test is done with it.
    fn stand_in(name: &str, then: Then) -> (mpsc::Receiver<String>, mpsc::Sender<()>) {
        use std::os::windows::io::FromRawHandle;
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // PIPE_ACCESS_DUPLEX, byte mode, blocking, one instance.
        let pipe = unsafe { CreateNamedPipeW(wide.as_ptr(), 3, 0, 1, 4096, 4096, 0, std::ptr::null()) };
        assert!(!pipe.is_null() && pipe as isize != -1, "the stand-in pipe");
        let pipe = pipe as usize;
        let (got_tx, got_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let handle = pipe as *mut core::ffi::c_void;
            unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while let Ok(1) = file.read(&mut byte) {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            let _ = got_tx.send(String::from_utf8_lossy(&line).to_string());
            match then {
                Then::Silence => {
                    let _ = done_rx.recv_timeout(Duration::from_secs(20));
                }
                Then::Close => {}
                Then::Say(word) => {
                    let _ = file.write_all(format!("{word}\n").as_bytes());
                    let _ = file.flush();
                    let _ = done_rx.recv_timeout(Duration::from_secs(20));
                }
            }
        });
        (got_rx, done_tx)
    }

    const REQUEST: &str = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;

    /// What each agent is told when nobody decided: nothing that allows anything.
    fn assert_no_decision(outcome: &Outcome) {
        assert_eq!(outcome.answer(), None);
        for agent in ["", "codex", "muse"] {
            assert_eq!(reply::stdout_with(agent, "PermissionRequest", outcome.answer(), None, None), None, "{agent:?}");
        }
        // Copilot is fail-closed and must read JSON: an explicit "ask".
        assert_eq!(
            reply::stdout_with("copilot", "PermissionRequest", outcome.answer(), None, None).as_deref(),
            Some(r#"{"permissionDecision":"ask"}"#)
        );
    }

    #[test]
    fn with_the_app_closed_the_relay_is_done_at_once() {
        let started = std::time::Instant::now();
        // The long budget of a permission request: none of it may be waited out.
        let outcome = exchange(&own_pipe("closed"), format!("{REQUEST}\n"), true, DECISION_BUDGET, &|| false);
        assert_eq!(outcome, Outcome::NoServer);
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        assert_no_decision(&outcome);
    }

    #[test]
    fn a_card_nobody_answers_ends_the_wait_with_no_decision() {
        let name = own_pipe("silent");
        let (got, done) = stand_in(&name, Then::Silence);
        let budget = Duration::from_millis(600);
        let started = std::time::Instant::now();
        let outcome = exchange(&name, format!("{REQUEST}\n"), true, budget, &|| false);
        let waited = started.elapsed();
        // The request arrived, the wait lasted its budget and no longer, and
        // what comes out of it is no decision: nothing answers for the user.
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).as_deref(), Ok(REQUEST));
        assert!(waited >= budget && waited < budget + Duration::from_secs(2), "{waited:?}");
        assert_eq!(outcome, Outcome::Sent(None));
        assert_no_decision(&outcome);
        let _ = done.send(());
    }

    #[test]
    fn an_app_that_lets_go_of_the_request_releases_the_relay_before_its_time() {
        let name = own_pipe("gone");
        let (got, _done) = stand_in(&name, Then::Close);
        let started = std::time::Instant::now();
        let outcome = exchange(&name, format!("{REQUEST}\n"), true, Duration::from_secs(30), &|| false);
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).as_deref(), Ok(REQUEST));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_no_decision(&outcome);
    }

    #[test]
    fn a_click_is_the_only_thing_that_comes_back_as_a_decision() {
        let name = own_pipe("deny");
        let (_got, done) = stand_in(&name, Then::Say("deny"));
        let outcome = exchange(&name, format!("{REQUEST}\n"), true, Duration::from_secs(30), &|| false);
        assert_eq!(outcome, Outcome::Sent(Some("deny".into())));
        let _ = done.send(());
        // An event nobody waits on reads no answer, whatever the other end writes.
        let name = own_pipe("ignored");
        let (_got, done) = stand_in(&name, Then::Say("allow"));
        let outcome = exchange(&name, "{\"hook_event_name\":\"PreToolUse\"}\n".into(), false, FIRE_AND_FORGET_BUDGET, &|| false);
        assert_eq!(outcome, Outcome::Sent(None));
        let _ = done.send(());
    }

    #[test]
    fn a_claude_desktop_session_is_answered_as_claude_code() {
        // The relay runs for it without `--agent`: the tag is for the island's
        // pill, and the reply is Claude Code's own.
        let desktop = env_of(&[("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")]);
        let args = Args::default();
        let ev = prepare(REQUEST.as_bytes(), &args, &desktop, "/p", &[]).expect("forwarded");
        let v: Value = serde_json::from_str(ev.line.trim_end()).unwrap();
        assert_eq!(v["lintel_agent"], "claude-desktop");
        assert!(reply::takes_decisions(&args.agent));
        assert!(reply::stdout_with(&args.agent, &ev.name, Some("deny"), None, None).unwrap().contains(r#""behavior":"deny""#));
        assert_eq!(reply::stdout_with(&args.agent, &ev.name, None, None, None), None);
    }

    #[test]
    fn an_edit_beyond_the_budget_is_cut_and_flagged() {
        let huge = "x".repeat(MAX_DIFF_FIELD_LEN + 10);
        let mut v = serde_json::json!({
            "tool_name": "Write",
            "tool_input": { "file_path": "/p/big.txt", "content": huge },
        });
        truncate_payload(&mut v, "PostToolUse");
        assert!(v["tool_input"]["content"].as_str().unwrap().len() <= MAX_DIFF_FIELD_LEN + 4);
        assert_eq!(v["lintel_diff_truncated"], serde_json::Value::Bool(true));

        // Together, the edit strings never pass the shared budget.
        let half = "y".repeat(MAX_DIFF_FIELD_LEN - 1);
        let edits: Vec<_> = (0..4)
            .map(|_| serde_json::json!({ "old_string": half, "new_string": half }))
            .collect();
        let mut multi = serde_json::json!({ "tool_name": "MultiEdit", "tool_input": { "edits": edits } });
        truncate_payload(&mut multi, "PostToolUse");
        assert!(multi.to_string().len() < MAX_DIFF_TOTAL + 64 * 1024);
        assert_eq!(multi["lintel_diff_truncated"], serde_json::Value::Bool(true));
    }
}
