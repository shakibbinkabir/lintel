// The chat, whoever answers it: the conversation, the system prompt, and which
// provider a turn goes to. Each provider's own wire format lives in its module:
// claude.rs (Anthropic), openai_compat.rs (OpenAI, Google AI, OpenRouter),
// local_chat.rs (Ollama, LM Studio, any OpenAI-compatible server) and
// chat/cli.rs (an agent CLI the user installed, on their own plan).
//
// API keys never leave the credential store and file bytes never cross the IPC
// boundary: the island sends the question and gets the answer's text back.
//
// Two things start a program rather than send a request, and both only ever on
// the user's own Send: the "cli" provider (chat/cli.rs) and the task chip
// (chat/tasks.rs). How a program is started is chat/program.rs's to say.

pub(crate) mod cli;
pub(crate) mod headless;
pub(crate) mod mcp;
pub(crate) mod program;
pub(crate) mod scan;
pub(crate) mod tasks;
pub(crate) mod text;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::i18n::t;
use crate::island::WINDOW_LABEL;
use crate::settings::Settings;
use crate::{claude, local_chat, openai_compat, secrets};

pub use text::brief;

pub const ANTHROPIC: &str = "anthropic";
/// The provider that is an installed agent CLI; which one is its "model".
pub const CLI: &str = "cli";
/// What a turn the user stopped fails with. The chat view knows it stopped
/// the turn and shows nothing, so nobody reads this.
const STOPPED: &str = "stopped";

// The fields are renamed too: the island sends `appName`. (Without it a window
// handed to the chat was refused before it reached any provider.)
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
    /// Not something to talk about: the message itself is handed to a task
    /// that runs in this folder (chat/tasks.rs).
    Task { folder: String },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
    /// The task that was started, when the message went to one: its answer
    /// comes later, as a `chat-task` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<u64>,
}

/// A model a provider offers, for the picker in the chat view.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
}

// ── Conversation ──────────────────────────────────────────────────────────────

/// The conversation is kept twice: in the wire format of the provider that
/// answered last (Claude's carries web search blocks no other provider would
/// understand), and as plain text turns. Switching provider mid-conversation
/// rebuilds the history from the plain turns, so nothing in one provider's
/// format is ever sent to another.
#[derive(Default)]
pub struct Chat {
    inner: Mutex<Conversation>,
}

#[derive(Default)]
struct Conversation {
    /// Bumped by every reset, so an answer that lands after "New chat" is dropped.
    epoch: u64,
    /// The provider `native` belongs to.
    owner: Option<String>,
    native: Vec<Value>,
    /// `{"role", "content": text}` turns, the same whoever answered.
    plain: Vec<Value>,
}

/// What a provider needs to build one turn.
pub struct Turn {
    epoch: u64,
    provider: String,
    /// No earlier turn: the file or window context rides along with this one.
    pub first: bool,
    /// The earlier turns, in this provider's format.
    pub history: Vec<Value>,
}

impl Chat {
    pub fn reset(&self) {
        mcp::deny_all();
        // A CLI still writing its answer is ended: nobody will read it.
        stop_answer();
        let mut c = self.inner.lock().unwrap();
        let epoch = c.epoch + 1;
        *c = Conversation { epoch, ..Default::default() };
    }

    /// Starts a turn with `provider`, converting the history if another
    /// provider answered the previous turns.
    pub fn begin(&self, provider: &str) -> Turn {
        let mut c = self.inner.lock().unwrap();
        if c.owner.as_deref() != Some(provider) {
            c.native = c.plain.clone();
            c.owner = Some(provider.to_string());
        }
        Turn {
            epoch: c.epoch,
            provider: provider.to_string(),
            first: c.plain.is_empty(),
            history: c.native.clone(),
        }
    }

    /// Records a finished turn: the user message and the answer in the
    /// provider's format, and their plain text. Only a successful turn is
    /// recorded, so the history always matches what the model saw.
    pub fn commit(&self, turn: &Turn, user: Value, assistant: Value, user_text: &str, answer: &str) {
        let mut c = self.inner.lock().unwrap();
        if c.epoch != turn.epoch || c.owner.as_deref() != Some(turn.provider.as_str()) {
            return;
        }
        c.native.push(user);
        c.native.push(assistant);
        c.plain.push(json!({ "role": "user", "content": user_text }));
        c.plain.push(json!({ "role": "assistant", "content": answer }));
    }
}

// ── System prompt ─────────────────────────────────────────────────────────────

/// Mascot's instructions. Greets the user by their first name when the account
/// has one worth using (identity.rs), and only claims web search where the
/// provider runs it (Claude).
pub fn system_prompt(web_search: bool) -> String {
    let mut prompt = system_prompt_for(crate::identity::first_name(), web_search);
    // Opt-in (Settings → Chat → Today's agenda): today's events go to the chat provider.
    if AGENDA.load(Ordering::Relaxed) {
        if let Some(agenda) = notch_feat_calendar::agenda_today() {
            prompt.push_str(&format!("\n\nThe user's calendar today (use it only when asked about their day):\n{agenda}"));
        }
    }
    prompt
}

/// "Today's agenda" is on in the chat's settings; set at the start of every turn.
static AGENDA: AtomicBool = AtomicBool::new(false);

fn system_prompt_for(first_name: Option<&str>, web_search: bool) -> String {
    let opening = match first_name {
        Some(name) => format!("You are {}, {name}'s personal AI assistant living at the top of their screen.", notch_brand::NAME),
        None => format!("You are {}, a personal AI assistant living at the top of the user's screen.", notch_brand::NAME),
    };
    let abilities = if web_search {
        "You have web search access and can help with absolutely anything — research, coding, finding places, recommendations, tasks, questions."
    } else {
        "You can help with absolutely anything — research, coding, recommendations, tasks, questions. You have no web access: say so when something needs current information."
    };
    format!(
        "{opening} {abilities} \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
Use light Markdown when it helps: short paragraphs, bullet lists, **bold**, `inline code` and fenced code blocks. Avoid tables and big headings: the chat window is small."
    )
}

/// The window context line, as ClaudeService.chat() writes it.
pub fn window_line(app_name: &str, title: &str, url: Option<&str>) -> String {
    let mut text = format!("Context — App: {app_name}, Window: {title}");
    if let Some(url) = url {
        text.push_str(&format!(", URL: {url}"));
    }
    text
}

/// The plain-text record of what the user asked, context included.
pub fn plain_question(first: bool, context: Option<&ChatContext>, query: &str) -> String {
    match context.filter(|_| first) {
        Some(ChatContext::File { name, .. }) => format!("File: {name}\n\n{query}"),
        Some(ChatContext::Window { app_name, title, url }) => {
            format!("{}\n\n{query}", window_line(app_name, title, url.as_deref()))
        }
        Some(ChatContext::Task { .. }) | None => query.to_string(),
    }
}

// ── Which provider ────────────────────────────────────────────────────────────

/// The model chosen for `provider`, or its default.
pub fn model_for(settings: &Settings, provider: &str) -> String {
    if provider == ANTHROPIC {
        let m = settings.model.trim();
        return if m.is_empty() { claude::DEFAULT_MODEL.to_string() } else { m.to_string() };
    }
    settings
        .chat_models
        .get(provider)
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .or_else(|| openai_compat::provider(provider).map(|p| p.default_model.to_string()))
        .unwrap_or_default()
}

/// A file rides along only if it is one of Lintel's own copies of a dropped
/// file (files.rs puts them in the inbox). The page names the path, so without
/// this any file the user can read could be sent to a chat provider.
fn checked_context(context: ChatContext) -> Result<ChatContext, String> {
    match context {
        ChatContext::File { name, path } => {
            let inbox = crate::files::inbox_dir();
            if !is_inside(&inbox, std::path::Path::new(&path)) {
                return Err(crate::i18n::t("Only a file dropped on the island can be sent with a question."));
            }
            Ok(ChatContext::File { name, path })
        }
        other => Ok(other),
    }
}

/// True when `path` is a regular file directly inside `dir`, both resolved
/// (no `..`, no symlink pointing out of it).
fn is_inside(dir: &std::path::Path, path: &std::path::Path) -> bool {
    let (Ok(dir), Ok(file)) = (dir.canonicalize(), path.canonicalize()) else { return false };
    file.parent() == Some(dir.as_path())
        && std::fs::symlink_metadata(&file).map(|m| m.is_file()).unwrap_or(false)
}

/// One chat turn with the provider chosen in the settings.
pub async fn send(
    app: &AppHandle,
    chat: &Chat,
    settings: &Settings,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    if let Some(ChatContext::Task { folder }) = &context {
        return start_task(app, query, folder);
    }
    let context = context.map(checked_context).transpose()?;
    let agenda = settings.features.get("chat").and_then(|c| c.get("calendarAgenda")).and_then(Value::as_bool);
    AGENDA.store(agenda == Some(true), Ordering::Relaxed);
    let provider = settings.chat_provider.as_str();
    if provider == CLI {
        return ask_cli(app, chat, settings, query, context).await;
    }
    let model = model_for(settings, provider);
    if provider == ANTHROPIC || provider.is_empty() {
        return claude::send(app, chat, settings, &model, query, context).await;
    }
    if let Some(p) = openai_compat::provider(provider) {
        return openai_compat::send(chat, p, &model, query, context).await;
    }
    if let Some(server) = local_chat::server(settings, provider) {
        return local_chat::send(app, chat, &server, &model, query, context).await;
    }
    Err(format!("Unknown chat provider: {provider}"))
}

/// The models `provider` offers. Asked only when the user opens the picker on
/// that provider, and only once it has a key (or, for a local server, an
/// address): nothing is sent anywhere before that.
pub async fn models(settings: &Settings, provider: &str) -> Result<Vec<ModelInfo>, String> {
    let no_key = || crate::i18n::t("No API key — add it in Settings.");
    // The CLIs found on this computer stand where a provider's models do.
    if provider == CLI {
        let found = cli::installed();
        if found.is_empty() {
            return Err(t("No agent CLI was found on this computer."));
        }
        return Ok(found.iter().map(|c| ModelInfo { id: c.id.into(), label: c.name.into() }).collect());
    }
    if provider == ANTHROPIC {
        if claude::gateway(settings)?.is_none() && secrets::get(claude::KEY).is_none() {
            return Err(no_key());
        }
        return claude::models(settings).await;
    }
    if let Some(p) = openai_compat::provider(provider) {
        let key = secrets::get(p.key).ok_or_else(no_key)?;
        return openai_compat::models(p, &key).await;
    }
    if let Some(server) = local_chat::server(settings, provider) {
        return local_chat::models(&server).await;
    }
    Err(format!("Unknown chat provider: {provider}"))
}

// ── An installed CLI answers ──────────────────────────────────────────────────

/// The run that is writing an answer right now, so Stop can end it.
static ANSWERING: Mutex<Option<Arc<program::Handle>>> = Mutex::new(None);

/// Stop, from the chat view: ends the CLI that is answering, if one is.
pub fn stop_answer() {
    mcp::deny_all();
    if let Some(run) = ANSWERING.lock().unwrap().as_ref() {
        run.cancel();
    }
}

/// The CLIs a turn may go to, in order: the one picked in the chat view, then
/// the ones the user turned on as fallbacks in Settings (`cliOrder` in the
/// chat feature's settings). A CLI the user chose nowhere is never started.
fn cli_order(settings: &Settings) -> Vec<&'static cli::Cli> {
    let picked = settings.chat_models.get(CLI).map(String::as_str);
    let fallbacks = settings
        .features
        .get("chat")
        .and_then(|chat| chat.get("cliOrder"))
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut order: Vec<&'static cli::Cli> = Vec::new();
    for found in picked.into_iter().chain(fallbacks).filter_map(cli::find) {
        if !order.iter().any(|c| c.id == found.id) {
            order.push(found);
        }
    }
    order
}

/// The conversation as text, for a CLI that is started afresh for each turn
/// and keeps nothing: every turn so far, then the new one.
fn transcript(history: &[Value], user: &str) -> String {
    let mut out = String::new();
    for turn in history {
        let who = if turn["role"] == "assistant" { "Assistant" } else { "User" };
        out.push_str(&format!("{who}: {}\n\n", turn["content"].as_str().unwrap_or("")));
    }
    out.push_str(&format!("User: {user}"));
    out
}

/// One chat turn through an installed CLI: the first of `cli_order` that
/// answers. One that fails (not signed in, out of credits, gone) hands over
/// to the next; the user stopping the turn ends it there.
async fn ask_cli(
    app: &AppHandle,
    chat: &Chat,
    settings: &Settings,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let order = cli_order(settings);
    if order.is_empty() {
        return Err(t("Pick a CLI above the chat box first."));
    }
    let turn = chat.begin(CLI);
    let user = local_chat::user_text(turn.first, context.as_ref(), &query);
    let conversation = transcript(&turn.history, &user);
    let system = system_prompt(false);

    let run = Arc::new(program::Handle::default());
    *ANSWERING.lock().unwrap() = Some(run.clone());
    let (app, handle) = (app.clone(), run.clone());
    let answer = tauri::async_runtime::spawn_blocking(move || {
        let mut last = String::new();
        // The island hears about new text at most 15 times a second.
        let mut told = Instant::now();
        for cli in order {
            let step = cli::ask(cli, &system, &conversation, &handle, |visible| {
                if told.elapsed() >= Duration::from_millis(66) {
                    told = Instant::now();
                    let _ = app.emit_to(WINDOW_LABEL, "chat-delta", visible);
                }
            });
            match step {
                Ok(text) => return Ok(text),
                Err(cli::Failure::Cancelled) => return Err(STOPPED.to_string()),
                Err(cli::Failure::Other(why)) => last = why,
            }
        }
        Err(last)
    })
    .await;
    {
        let mut answering = ANSWERING.lock().unwrap();
        if answering.as_ref().is_some_and(|current| Arc::ptr_eq(current, &run)) {
            *answering = None;
        }
    }
    let answer = answer.map_err(|err| err.to_string())??;

    let plain = plain_question(turn.first, context.as_ref(), &query);
    let (asked, answered) = (json!({ "role": "user", "content": user }), json!({ "role": "assistant", "content": answer }));
    chat.commit(&turn, asked, answered, &plain, &answer);
    Ok(ChatReply { text: answer, task: None })
}

// ── The task chip ─────────────────────────────────────────────────────────────

/// Hands `prompt` to a task in `folder` and answers with its id at once. The
/// island hears how it ended as a `chat-task` event.
fn start_task(app: &AppHandle, prompt: String, folder: &str) -> Result<ChatReply, String> {
    let app = app.clone();
    let id = tasks::start(prompt, folder, move |id, ended| {
        let event = json!({ "id": id, "ok": ended.ok, "cancelled": ended.cancelled, "text": ended.text });
        let _ = app.emit_to(WINDOW_LABEL, "chat-task", event);
    })?;
    Ok(ChatReply { text: String::new(), task: Some(id) })
}

// ── Questions that have no command of their own ───────────────────────────────

/// What the chat view and Settings ask besides a chat turn: which model
/// servers run on this computer, which CLIs are installed, stop that answer,
/// cancel that task, and the GitHub card's pull requests through `gh`.
///
/// debt: these ride the `local_connect` command, under names no provider has
/// (local_chat.rs hands them over), because lib.rs was not this task's to
/// change. Each wants a command of its own there, and the `chat:` ones want
/// the `registry::require(&app, "chat")` the other chat commands have.
pub async fn ask(what: &str, arg: &str) -> Option<Result<Value, String>> {
    Some(match what {
        "chat:scan" => Ok(json!(scan::scan().await)),
        "chat:clis" => Ok(cli::installed().iter().map(|c| json!({ "id": c.id, "name": c.name })).collect()),
        "chat:stop" => {
            stop_answer();
            Ok(Value::Null)
        }
        "chat:task-cancel" => {
            if let Ok(id) = arg.parse() {
                tasks::cancel(id);
            }
            Ok(Value::Null)
        }
        "chat:mcp-list" => Ok(json!(mcp::list())),
        "chat:mcp-answer" => {
            mcp::answer(arg);
            Ok(Value::Null)
        }
        "github:pulse" => crate::github::pulse_through_gh().await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_texts(history: &[Value]) -> Vec<(String, Value)> {
        history
            .iter()
            .map(|m| (m["role"].as_str().unwrap().to_string(), m["content"].clone()))
            .collect()
    }

    #[test]
    fn a_turn_is_recorded_only_once_it_succeeds() {
        let chat = Chat::default();
        let t = chat.begin("anthropic");
        assert!(t.first);
        assert!(t.history.is_empty());
        // A failed turn records nothing: the next one is still the first.
        let t = chat.begin("anthropic");
        assert!(t.first);
        chat.commit(&t, json!({"role":"user","content":[{"type":"text","text":"hi"}]}), json!({"role":"assistant","content":[{"type":"text","text":"hello"}]}), "hi", "hello");
        let t = chat.begin("anthropic");
        assert!(!t.first);
        assert_eq!(t.history.len(), 2);
        assert!(t.history[0]["content"].is_array());
    }

    #[test]
    fn switching_provider_never_sends_claude_blocks_to_another_one() {
        let chat = Chat::default();
        let t = chat.begin("anthropic");
        let blocks = json!([
            {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"x"}},
            {"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[]},
            {"type":"text","text":"Found it."}
        ]);
        chat.commit(&t, json!({"role":"user","content":[{"type":"text","text":"look"}]}), json!({"role":"assistant","content":blocks}), "look", "Found it.");

        let t = chat.begin("openai");
        assert!(!t.first);
        assert_eq!(
            turn_texts(&t.history),
            vec![("user".into(), json!("look")), ("assistant".into(), json!("Found it."))]
        );
        let raw = serde_json::to_string(&t.history).unwrap();
        assert!(!raw.contains("web_search"), "{raw}");

        // And back: Claude gets the plain turns too, including OpenAI's answer.
        chat.commit(&t, json!({"role":"user","content":"more"}), json!({"role":"assistant","content":"Sure."}), "more", "Sure.");
        let t = chat.begin("anthropic");
        assert_eq!(t.history.len(), 4);
        assert_eq!(t.history[3], json!({"role":"assistant","content":"Sure."}));
        assert!(!serde_json::to_string(&t.history).unwrap().contains("web_search"));
    }

    #[test]
    fn an_answer_that_lands_after_a_reset_or_a_switch_is_dropped() {
        let chat = Chat::default();
        let t = chat.begin("openai");
        chat.reset();
        chat.commit(&t, json!({"role":"user","content":"q"}), json!({"role":"assistant","content":"a"}), "q", "a");
        assert!(chat.begin("openai").first);

        let t = chat.begin("openai");
        let _other = chat.begin("google");
        chat.commit(&t, json!({"role":"user","content":"q"}), json!({"role":"assistant","content":"a"}), "q", "a");
        assert!(chat.begin("google").first);
    }

    #[test]
    fn the_prompt_greets_by_first_name_and_claims_web_search_only_for_claude() {
        let p = system_prompt_for(Some("Louis"), true);
        assert!(p.starts_with("You are Lintel, Louis's personal AI assistant living at the top of their screen."));
        assert!(p.contains("web search access"));
        assert!(p.contains("light Markdown"));
        let p = system_prompt_for(None, false);
        assert!(p.starts_with("You are Lintel, a personal AI assistant living at the top of the user's screen."));
        assert!(!p.contains("web search"));
        assert!(p.contains("no web access"));
    }

    #[test]
    fn context_goes_with_the_first_question_only() {
        let file = ChatContext::File { name: "a.txt".into(), path: "/x/a.txt".into() };
        assert_eq!(plain_question(true, Some(&file), "why?"), "File: a.txt\n\nwhy?");
        assert_eq!(plain_question(false, Some(&file), "why?"), "why?");
        let win = ChatContext::Window { app_name: "Code".into(), title: "main.rs".into(), url: None };
        assert_eq!(plain_question(true, Some(&win), "q"), "Context — App: Code, Window: main.rs\n\nq");
        assert_eq!(window_line("Edge", "Docs", Some("https://x.dev")), "Context — App: Edge, Window: Docs, URL: https://x.dev");
    }

    #[test]
    fn the_model_comes_from_the_settings_or_the_provider_default() {
        let mut s = Settings::default();
        assert_eq!(model_for(&s, "anthropic"), claude::DEFAULT_MODEL);
        assert_eq!(model_for(&s, "openai"), openai_compat::provider("openai").unwrap().default_model);
        assert_eq!(model_for(&s, "ollama"), "");
        s.chat_models.insert("openai".into(), " gpt-x ".into());
        s.chat_models.insert("ollama".into(), "llama3.2".into());
        s.model = "claude-haiku-4-5".into();
        assert_eq!(model_for(&s, "openai"), "gpt-x");
        assert_eq!(model_for(&s, "ollama"), "llama3.2");
        assert_eq!(model_for(&s, "anthropic"), "claude-haiku-4-5");
    }

    #[test]
    fn only_files_in_the_inbox_ride_along() {
        let base = std::env::temp_dir().join(format!("lintel-chat-ctx-{}", std::process::id()));
        let inbox = base.join("inbox");
        std::fs::create_dir_all(inbox.join("sub")).unwrap();
        std::fs::write(inbox.join("a.txt"), b"a").unwrap();
        std::fs::write(base.join("secret.txt"), b"s").unwrap();
        std::fs::write(inbox.join("sub").join("b.txt"), b"b").unwrap();
        assert!(is_inside(&inbox, &inbox.join("a.txt")));
        assert!(!is_inside(&inbox, &base.join("secret.txt")));
        assert!(!is_inside(&inbox, &inbox.join("..").join("secret.txt")));
        assert!(!is_inside(&inbox, &inbox.join("sub").join("b.txt")));
        assert!(!is_inside(&inbox, &inbox.join("missing.txt")));
        assert!(!is_inside(&inbox, &inbox));
        let _ = std::fs::remove_dir_all(&base);
    }
}
