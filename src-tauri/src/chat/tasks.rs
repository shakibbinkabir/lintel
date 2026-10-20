// The chat's task chip: the next message is handed to Claude Code itself,
// `claude -p`, headless in a folder the user chose. No terminal, no API key:
// the task runs on the user's own Claude plan, with their own Claude Code
// settings, so their hooks see it like any other session.
//
// Here the process is started, kept hold of so it can be cancelled, limited to
// thirty minutes, and its final answer handed back. At most three run at once.
//
// The idea and the limits are upstream pull request 406's
// (`windows/src-tauri/src/task_runner.rs`). Two things differ: the message
// goes to claude's input and never onto its command line, and no tool is
// allowed ahead of time. A headless claude cannot ask, so whatever needs a
// permission the user's own settings do not give is refused: a task can fail,
// but it never acts without the user having allowed it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notch_core::i18n::{t, tf};
use serde_json::Value;

use super::cli;
use super::program::{self, Handle, Output, Spec, Stop};
use super::text::first_words;

/// Tasks running at once.
pub const MAX_RUNNING: usize = 3;
/// A task that outlives this is presumed stuck and ended.
const TIME_LIMIT: Duration = Duration::from_secs(30 * 60);
/// `--output-format json` prints one object at the end, the answer in it.
const MAX_OUTPUT: usize = 8 * 1024 * 1024;

/// What every task is told besides the task itself.
const CHARTER: &str = "You were started from a small chat box, not a terminal: \
the user reads only your final message, in a small card. Write it as plain \
Markdown and keep it short: what was done, what changed, and where any files are.";

/// The relay keeps headless runs (`claude -p`, the Agent SDK, scheduled jobs)
/// off the island unless they say they are watched: a task is, its pill is the
/// point. Handed through Claude Code's own settings, whose `env` its hooks
/// inherit. The name is the relay's (hook/src/main.rs, WATCHED).
const WATCHED: &str = r#"{"env":{"LINTEL_WATCHED":"1"}}"#;

/// The running tasks, so Cancel can reach them.
static RUNNING: Mutex<BTreeMap<u64, Arc<Handle>>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

/// How a task ended. `text` is the answer (Markdown), or why there is none.
pub struct Ended {
    pub ok: bool,
    pub cancelled: bool,
    pub text: String,
}

/// Starts the task and answers with its id right away. `on_end` is called
/// once, from the task's own thread, when it is over.
pub fn start(prompt: String, folder: &str, on_end: impl FnOnce(u64, Ended) + Send + 'static) -> Result<u64, String> {
    let folder = Path::new(folder);
    if !folder.is_absolute() || !folder.is_dir() {
        return Err(t("That folder does not exist."));
    }
    let claude = cli::find("claude")
        .and_then(cli::Cli::program)
        .ok_or_else(|| tf("{name} is not installed.", &[("name", "Claude Code")]))?;
    let handle = Arc::new(Handle::default());
    let id = {
        let mut running = RUNNING.lock().unwrap();
        if running.len() >= MAX_RUNNING {
            return Err(t("Three tasks are already running. Let one finish first."));
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        running.insert(id, handle.clone());
        id
    };
    let spec = Spec {
        args: ["-p", "--output-format", "json", "--append-system-prompt", CHARTER, "--settings", WATCHED]
            .map(String::from)
            .to_vec(),
        stdin: Some(prompt),
        cwd: folder.to_path_buf(),
        timeout: TIME_LIMIT,
        max_output: MAX_OUTPUT,
    };
    std::thread::spawn(move || {
        let mut result = None;
        let run = program::run(&claude, &spec, &handle, |line| {
            if let Some(found) = read_result(line) {
                result = Some(found);
            }
        });
        RUNNING.lock().unwrap().remove(&id);
        on_end(id, ended(run, result));
    });
    Ok(id)
}

/// Cancel, from the task's bubble in the chat. Its `on_end` still comes.
pub fn cancel(id: u64) {
    if let Some(handle) = RUNNING.lock().unwrap().get(&id) {
        handle.cancel();
    }
}

/// claude's last word: `{"type":"result","is_error":false,"result":"…"}`.
fn read_result(line: &str) -> Option<(bool, String)> {
    let event: Value = serde_json::from_str(line).ok()?;
    if event.get("type")?.as_str()? != "result" {
        return None;
    }
    let failed = event.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let text = event.get("result").and_then(Value::as_str).unwrap_or("").trim().to_string();
    Some((!failed, text))
}

fn ended(run: Result<Output, Stop>, result: Option<(bool, String)>) -> Ended {
    let failed = |text: String| Ended { ok: false, cancelled: false, text };
    let output = match run {
        Ok(output) => output,
        Err(Stop::Cancelled) => return Ended { ok: false, cancelled: true, text: String::new() },
        Err(Stop::TimedOut) => return failed(t("The task ran for half an hour and was ended.")),
        Err(Stop::TooLarge) => return failed(t("The answer is too large.")),
        Err(Stop::NotStarted(why)) => return failed(format!("Claude Code: {why}")),
    };
    match result {
        Some((ok, text)) if !text.is_empty() => Ended { ok, cancelled: false, text },
        Some((true, _)) => Ended { ok: true, cancelled: false, text: t("Done.") },
        _ => match first_words(&output.stderr) {
            words if words.is_empty() => failed(t("The task ended without an answer.")),
            words => failed(format!("Claude Code: {words}")),
        },
    }
}
