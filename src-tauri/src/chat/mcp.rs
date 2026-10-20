// Tools of MCP servers the user already set up in other apps (Claude Desktop,
// Cursor, VS Code, Claude Code), for the Claude chat.
//
// - The other apps' config files are only read, never written.
// - A server is used only when the user turned it on (Settings → Chat, off by
//   default): nothing is started for the rest.
// - A server (a stdio program, started without a shell where it can be) is
//   started by the first turn that asks for its tools and stopped after
//   IDLE without use.
// - Every tool call waits for one click in the chat view (`chat-mcp-approve`
//   event, answered through chat.rs `ask("chat:mcp-answer")`); no answer in
//   five minutes is a no.
//
// debt: stdio servers only (no http/sse ones), and the tools are offered to
// the Claude provider only; a command that is not found as a program runs
// through cmd.exe /C with the arguments of the user's own config.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use notch_core::win::{home_dir, no_console};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

use super::program;
use crate::island::WINDOW_LABEL;
use crate::settings::Settings;

const IDLE: Duration = Duration::from_secs(120);
const START_WAIT: Duration = Duration::from_secs(25);
const CALL_WAIT: Duration = Duration::from_secs(60);
const ANSWER_WAIT: Duration = Duration::from_secs(300);
const MAX_RESULT: usize = 20_000;

// ── What other apps configured ────────────────────────────────────────────────

#[derive(Clone)]
pub struct Server {
    pub id: String,
    pub source: &'static str,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

/// For Settings: a server that was found (whether it is on is the page's to know).
#[derive(Serialize)]
pub struct Found {
    pub id: String,
    pub source: &'static str,
    pub command: String,
}

fn files() -> Vec<(&'static str, PathBuf, &'static str)> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    let home = home_dir();
    vec![
        ("Claude Desktop", appdata.join("Claude").join("claude_desktop_config.json"), "mcpServers"),
        ("Cursor", home.join(".cursor").join("mcp.json"), "mcpServers"),
        ("VS Code", appdata.join("Code").join("User").join("mcp.json"), "servers"),
        ("Claude Code", home.join(".claude.json"), "mcpServers"),
    ]
}

fn parse(source: &'static str, json: &Value, key: &str) -> Vec<Server> {
    let Some(map) = json.get(key).and_then(Value::as_object) else { return Vec::new() };
    map.iter()
        .filter_map(|(name, def)| {
            let command = def.get("command")?.as_str()?.trim().to_string();
            if command.is_empty() {
                return None;
            }
            let args = def.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
            let env = def
                .get("env")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect();
            Some(Server { id: name.clone(), source, command, args, env })
        })
        .collect()
}

/// Every stdio server the files name; the first app to name one wins.
pub fn discover() -> Vec<Server> {
    let mut out: Vec<Server> = Vec::new();
    for (source, path, key) in files() {
        let Some(json) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
        else {
            continue;
        };
        for s in parse(source, &json, key) {
            if !out.iter().any(|o| o.id == s.id) {
                out.push(s);
            }
        }
    }
    out
}

/// The servers turned on in the chat's settings (`mcpServers`).
pub fn enabled(settings: &Settings) -> Vec<String> {
    settings
        .features
        .get("chat")
        .and_then(|c| c.get("mcpServers"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

pub fn list() -> Vec<Found> {
    discover().into_iter().map(|s| Found { command: s.command, source: s.source, id: s.id }).collect()
}

// ── A running server ──────────────────────────────────────────────────────────

struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next: u64,
    tools: Vec<Value>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    fn start(s: &Server) -> Result<Client, String> {
        let mut cmd = match program::resolve(&[&s.command]) {
            Some(p) => {
                let mut c = Command::new(&p.exe);
                c.args(&p.lead);
                c
            }
            None => {
                let mut c = Command::new("cmd.exe");
                c.arg("/C").arg(&s.command);
                c
            }
        };
        cmd.args(&s.args).envs(s.env.iter().cloned()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        no_console(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err("no pipes".into());
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        // Dropped on any early return below: the child is killed.
        let mut c = Client { child, stdin, rx, next: 1, tools: Vec::new() };
        c.request(
            "initialize",
            json!({ "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": { "name": notch_brand::NAME, "version": env!("CARGO_PKG_VERSION") } }),
            START_WAIT,
        )?;
        c.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        let listed = c.request("tools/list", json!({}), START_WAIT)?;
        c.tools = listed.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(c)
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{v}").and_then(|_| self.stdin.flush()).map_err(|e| e.to_string())
    }

    fn request(&mut self, method: &str, params: Value, wait: Duration) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let end = Instant::now() + wait;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let msg = self.rx.recv_timeout(left).map_err(|_| "the MCP server did not answer".to_string())?;
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // a notification or a request of the server's: not answered
            }
            if let Some(err) = msg.get("error") {
                return Err(err.get("message").and_then(Value::as_str).unwrap_or("MCP error").to_string());
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

static POOL: Mutex<Option<HashMap<String, Client>>> = Mutex::new(None);
static LAST_USE: Mutex<Option<Instant>> = Mutex::new(None);

/// A tool as Claude is told about it.
pub struct Tool {
    pub api_name: String,
    pub server: String,
    pub name: String,
    pub def: Value,
}

fn api_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>();
    let mut n = format!("mcp__{}__{}", clean(server), clean(tool));
    n.truncate(64);
    n
}

/// Starts the servers turned on (the ones not already running) and lists their
/// tools. Blocking. A server that does not start is skipped.
pub fn tools_for(on: &[String]) -> Vec<Tool> {
    let mut guard = POOL.lock().unwrap();
    let pool = guard.get_or_insert_with(HashMap::new);
    let mut out = Vec::new();
    for s in discover().into_iter().filter(|s| on.contains(&s.id)) {
        if !pool.contains_key(&s.id) {
            match Client::start(&s) {
                Ok(c) => {
                    pool.insert(s.id.clone(), c);
                }
                Err(e) => {
                    crate::log::line(format!("chat: MCP server {} did not start: {e}", s.id));
                    continue;
                }
            }
        }
        for t in &pool[&s.id].tools {
            let Some(name) = t.get("name").and_then(Value::as_str) else { continue };
            let api = api_name(&s.id, name);
            let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
            out.push(Tool {
                def: json!({
                    "name": api,
                    "description": format!("{desc} (tool of the MCP server {})", s.id),
                    "input_schema": t.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                }),
                api_name: api,
                server: s.id.clone(),
                name: name.to_string(),
            });
        }
    }
    *LAST_USE.lock().unwrap() = Some(Instant::now());
    out
}

/// Runs one tool the user approved. Blocking. The text it returns, or why it failed.
pub fn call(server: &str, tool: &str, args: &Value) -> Result<String, String> {
    let mut guard = POOL.lock().unwrap();
    let client = guard.as_mut().and_then(|p| p.get_mut(server)).ok_or("the MCP server is not running")?;
    let result = client.request("tools/call", json!({ "name": tool, "arguments": args }), CALL_WAIT)?;
    *LAST_USE.lock().unwrap() = Some(Instant::now());
    let text: String = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let text: String = text.chars().take(MAX_RESULT).collect();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(text);
    }
    Ok(text)
}

/// After a turn: stops every server once none has been used for IDLE.
pub fn idle_later() {
    tauri::async_runtime::spawn(async {
        tokio::time::sleep(IDLE + Duration::from_secs(1)).await;
        let idle = LAST_USE.lock().unwrap().is_none_or(|t| t.elapsed() >= IDLE);
        if idle {
            let _ = tauri::async_runtime::spawn_blocking(|| POOL.lock().unwrap().take()).await;
        }
    });
}

// ── One click per call ────────────────────────────────────────────────────────

static NEXT_ASK: AtomicU64 = AtomicU64::new(1);
static WAITING: Mutex<Vec<(u64, oneshot::Sender<bool>)>> = Mutex::new(Vec::new());

/// Shows the call in the chat view and waits for Allow or Deny.
pub async fn approve(app: &AppHandle, server: &str, tool: &str, args: &Value) -> bool {
    let id = NEXT_ASK.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    WAITING.lock().unwrap().push((id, tx));
    let _ = app.emit_to(WINDOW_LABEL, "chat-mcp-approve", json!({ "id": id, "server": server, "tool": tool, "args": args }));
    let allowed = tokio::time::timeout(ANSWER_WAIT, rx).await.ok().and_then(Result::ok).unwrap_or(false);
    WAITING.lock().unwrap().retain(|(i, _)| *i != id);
    allowed
}

/// The click: `arg` is "<id>:1" (allow) or "<id>:0" (deny).
pub fn answer(arg: &str) {
    let Some((id, yes)) = arg.split_once(':').and_then(|(i, y)| Some((i.parse::<u64>().ok()?, y == "1"))) else { return };
    let mut waiting = WAITING.lock().unwrap();
    if let Some(at) = waiting.iter().position(|(i, _)| *i == id) {
        let _ = waiting.remove(at).1.send(yes);
    }
}

/// Stop or New chat: every call still waiting is a no.
pub fn deny_all() {
    for (_, tx) in WAITING.lock().unwrap().drain(..) {
        let _ = tx.send(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_servers_are_read_and_the_rest_skipped() {
        let json = json!({ "mcpServers": {
            "fs": { "command": "npx", "args": ["-y", "x", "C:\\a"], "env": { "K": "v" } },
            "web": { "url": "https://x.example/mcp" },
            "empty": { "command": " " }
        }});
        let s = parse("Cursor", &json, "mcpServers");
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].id.as_str(), s[0].args.len(), s[0].env.len()), ("fs", 3, 1));
        assert_eq!(api_name("my server", "read.file"), "mcp__my_server__read_file");
    }
}
