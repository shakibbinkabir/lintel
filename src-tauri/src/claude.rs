// Claude API client — the same integration as ClaudeService.swift: multi-turn
// chat with web search, and files sent as document/image/text blocks.
//
// Everything happens here rather than in the island: the API key never leaves
// the credential store, and file bytes never cross the IPC boundary.
//
// LINTEL_ANTHROPIC_BASE_URL points the chat at an Anthropic-compatible gateway
// instead (issue #180). Only that variable is read: Claude Code's own
// ANTHROPIC_BASE_URL may name a proxy the user never meant to hand this key to.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Url;
use tauri::AppHandle;
use serde_json::{json, Value};

use crate::chat::{self, Chat, ChatContext, ChatReply, ModelInfo};
use crate::i18n::{t, tf};
use crate::settings::Settings;
use crate::{net, secrets};

/// Credential store entry of the Anthropic API key.
pub const KEY: &str = "anthropic-api-key";
/// The bearer token of an Anthropic-compatible gateway (Settings → Chat → Claude).
pub const GATEWAY_TOKEN: &str = "anthropic-gateway-token";
const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const BASE_URL_VAR: &str = "LINTEL_ANTHROPIC_BASE_URL";
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;
/// Tool calls one question may chain.
const MAX_TOOL_ROUNDS: usize = 8;
/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;

pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// The Messages endpoint: Anthropic's, or the gateway in LINTEL_ANTHROPIC_BASE_URL.
/// Read once; the gateway's host (never the key) goes to the log once.
pub(crate) fn endpoint() -> Result<Url, String> {
    static ENDPOINT: OnceLock<Result<Url, String>> = OnceLock::new();
    ENDPOINT
        .get_or_init(|| {
            let raw = std::env::var(BASE_URL_VAR).ok().filter(|v| !v.trim().is_empty());
            let resolved = match raw {
                None => Ok(Url::parse(DEFAULT_ENDPOINT).expect("valid default endpoint")),
                Some(raw) => net::anthropic_endpoint(&raw),
            };
            match &resolved {
                Ok(url) if url.as_str() != DEFAULT_ENDPOINT => crate::log::line(format!(
                    "chat: Claude requests go to {} ({BASE_URL_VAR})",
                    net::host_for_log(url)
                )),
                Err(err) => crate::log::line(format!("chat: {err}")),
                _ => {}
            }
            resolved
        })
        .clone()
}

/// The gateway the user typed in Settings (`gatewayUrl` of the chat feature's
/// settings): its Messages endpoint, https or this computer only. None: Anthropic
/// (or LINTEL_ANTHROPIC_BASE_URL, which `endpoint` reads).
pub(crate) fn gateway(settings: &Settings) -> Result<Option<Url>, String> {
    let raw = settings.features.get("chat").and_then(|c| c.get("gatewayUrl")).and_then(Value::as_str).unwrap_or("").trim();
    if raw.is_empty() {
        return Ok(None);
    }
    net::anthropic_endpoint(raw).map(Some).map_err(|e| e.replace(BASE_URL_VAR, "The gateway address"))
}

/// Where a Claude request goes and with what credential. A gateway gets its own
/// bearer token (none is fine for one on this computer), never the Anthropic key.
struct Route {
    url: Url,
    key: Option<String>,
    gateway: bool,
}

fn route(settings: &Settings) -> Result<Route, String> {
    if let Some(url) = gateway(settings)? {
        return Ok(Route { url, key: secrets::get(GATEWAY_TOKEN), gateway: true });
    }
    let key = secrets::get(KEY).ok_or_else(|| t("API key missing. Open settings."))?;
    Ok(Route { url: endpoint()?, key: Some(key), gateway: false })
}

/// The model list endpoint next to the Messages one.
fn models_endpoint(messages: &Url) -> Url {
    let mut url = messages.clone();
    let path = url.path().trim_end_matches("/messages").to_string();
    url.set_path(&format!("{path}/models"));
    url.set_query(Some("limit=100"));
    url
}

/// The user's content blocks for one turn. File / window context rides along
/// with the first message only, exactly like ClaudeService.chat().
fn user_content(first: bool, context: Option<&ChatContext>, query: &str) -> Vec<Value> {
    let mut content: Vec<Value> = Vec::new();
    if first {
        match context {
            Some(ChatContext::File { name, path }) => {
                if let Some(block) = file_block(path) {
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let line = chat::window_line(app_name, title, url.as_deref());
                content.push(json!({ "type": "text", "text": line }));
            }
            Some(ChatContext::Task { .. }) | None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));
    content
}

/// Towards a gateway: no web search tool and no fallbacks, which only Anthropic's API knows.
fn request_body(model: &str, system: &str, history: &[Value], user: &Value, gateway: bool) -> Value {
    let mut messages = history.to_vec();
    messages.push(user.clone());
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": messages,
    });
    if !gateway {
        body["tools"] = json!([{ "type": "web_search_20260209", "name": "web_search", "max_uses": 5 }]);
        body["fallbacks"] = json!("default");
    }
    body
}

/// The assistant's full text from a Messages API content array — the same as
/// claudeResponseText() on the Mac (#67). Web search answers interleave text
/// with tool blocks, and citations split a sentence across adjacent text
/// blocks: every text block is kept, joined as is, and only the whole trimmed.
pub fn response_text(content: &[Value]) -> Option<String> {
    let text: String = content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The content blocks to keep in the history and the text to show, or why there are none.
fn interpret(response: &Value) -> Result<(Vec<Value>, String), String> {
    // A policy decline comes back as HTTP 200 with stop_reason "refusal".
    if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        let why = response
            .pointer("/stop_details/explanation")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| t("Claude declined this one."));
        return Err(why);
    }
    let blocks = response
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| t("Unexpected API response."))?;
    let text = response_text(&blocks).ok_or_else(|| t("No response text."))?;
    Ok((blocks, text))
}

/// One chat turn. Returns the assistant's text, or a message the island shows
/// in the note view.
pub async fn send(
    app: &AppHandle,
    chat: &Chat,
    settings: &Settings,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let route = route(settings)?;

    let turn = chat.begin(chat::ANTHROPIC);
    let user = json!({ "role": "user", "content": user_content(turn.first, context.as_ref(), &query) });
    let mut body = request_body(model, &chat::system_prompt(!route.gateway), &turn.history, &user, route.gateway);

    // Tools of the MCP servers the user turned on: started now, stopped when idle.
    let on = chat::mcp::enabled(settings);
    let tools = if on.is_empty() { Vec::new() } else { tauri::async_runtime::spawn_blocking(move || chat::mcp::tools_for(&on)).await.unwrap_or_default() };
    if !tools.is_empty() {
        let list = body["tools"].as_array_mut().map(std::mem::take).unwrap_or_default();
        body["tools"] = Value::Array(list.into_iter().chain(tools.iter().map(|t| t.def.clone())).collect());
    }

    let mut response = call(&route, &body).await?;
    // The model asks for a tool: one click per call, then it goes on with the result.
    for _ in 0..MAX_TOOL_ROUNDS {
        if tools.is_empty() || response.get("stop_reason").and_then(Value::as_str) != Some("tool_use") {
            break;
        }
        let content = response["content"].clone();
        let mut results = Vec::new();
        for b in content.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use") {
            results.push(run_tool(app, &tools, b).await);
        }
        let messages = body["messages"].as_array_mut().expect("messages");
        messages.push(json!({ "role": "assistant", "content": content }));
        messages.push(json!({ "role": "user", "content": results }));
        response = call(&route, &body).await?;
    }
    if !tools.is_empty() {
        chat::mcp::idle_later();
    }
    let (blocks, text) = interpret(&response)?;

    // Store the whole content — tool_use / tool_result blocks included — so the
    // next turn has the right context.
    let plain = chat::plain_question(turn.first, context.as_ref(), &query);
    chat.commit(&turn, user, json!({ "role": "assistant", "content": blocks }), &plain, &text);
    Ok(ChatReply { text, task: None })
}

/// One `tool_use` block of the reply, answered with a `tool_result`: after the user's click.
async fn run_tool(app: &AppHandle, tools: &[chat::mcp::Tool], block: &Value) -> Value {
    let id = block["id"].clone();
    let result = |text: String, error: bool| json!({ "type": "tool_result", "tool_use_id": id, "content": text, "is_error": error });
    let Some(tool) = block["name"].as_str().and_then(|n| tools.iter().find(|t| t.api_name == n)) else {
        return result("Unknown tool.".into(), true);
    };
    let input = block["input"].clone();
    if !chat::mcp::approve(app, &tool.server, &tool.name, &input).await {
        return result("The user did not allow this call.".into(), true);
    }
    let (server, name) = (tool.server.clone(), tool.name.clone());
    match tauri::async_runtime::spawn_blocking(move || chat::mcp::call(&server, &name, &input)).await {
        Ok(Ok(text)) => result(text, false),
        Ok(Err(why)) => result(why, true),
        Err(e) => result(e.to_string(), true),
    }
}

// The key rides in x-api-key, a header reqwest would keep on a redirect to
// another host, and the address may come from LINTEL_ANTHROPIC_BASE_URL: no
// redirect is followed (net::keyed_client).
async fn call(route: &Route, body: &Value) -> Result<Value, String> {
    let mut request = net::keyed_client(&route.url, Duration::from_secs(90))?
        .post(route.url.clone())
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json");
    request = authorised(request, route);
    if !route.gateway {
        request = request.header("anthropic-beta", FALLBACK_BETA);
    }
    let response = request
        .json(body)
        .send()
        .await
        .map_err(|e| tf("Network error: {error}", &[("error", &e.to_string())]))?;

    let status = response.status();
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        let body = net::read_capped(response, net::MAX_ERROR_BODY).await.unwrap_or_default();
        return Err(format!("Claude API {status}: {}", chat::brief(&net::error_detail(&body))));
    }
    let bytes = net::read_capped(response, net::MAX_BODY).await?;
    serde_json::from_slice(&bytes).map_err(|e| tf("Bad API response: {error}", &[("error", &e.to_string())]))
}

/// Anthropic's x-api-key; a gateway's bearer token.
fn authorised(request: reqwest::RequestBuilder, route: &Route) -> reqwest::RequestBuilder {
    match (&route.key, route.gateway) {
        (Some(key), true) => request.bearer_auth(key),
        (Some(key), false) => request.header("x-api-key", key),
        (None, _) => request,
    }
}

/// The models on the user's Anthropic account (or gateway), newest first, as the API lists them.
pub async fn models(settings: &Settings) -> Result<Vec<ModelInfo>, String> {
    let route = route(settings)?;
    let url = models_endpoint(&route.url);
    let response = authorised(
        net::keyed_client(&url, Duration::from_secs(10))?
            .get(url.clone())
            .header("anthropic-version", ANTHROPIC_VERSION),
        &route,
    )
    .send()
        .await
        .map_err(|e| tf("Network error: {error}", &[("error", &e.to_string())]))?;
    let status = response.status();
    if !status.is_success() {
        let body = net::read_capped(response, net::MAX_ERROR_BODY).await.unwrap_or_default();
        return Err(format!("Claude API {status}: {}", chat::brief(&net::error_detail(&body))));
    }
    let bytes = net::read_capped(response, net::MAX_BODY).await?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| t("Unexpected API response."))?;
    Ok(parse_models(&json))
}

fn parse_models(json: &Value) -> Vec<ModelInfo> {
    json.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let label = m.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some(ModelInfo { id, label })
        })
        .collect()
}

/// PDF → document block, image → image block, text/code → inline text.
/// Mirrors readFileAsBlock() in ClaudeService.swift.
fn file_block(path: &str) -> Option<Value> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "pdf" => Some(("document", "application/pdf")),
        "jpg" | "jpeg" => Some(("image", "image/jpeg")),
        "png" => Some(("image", "image/png")),
        "gif" => Some(("image", "image/gif")),
        "webp" => Some(("image", "image/webp")),
        _ => None,
    };

    if let Some((block_type, media)) = media_type {
        let bytes = std::fs::read(path).ok()?;
        return Some(json!({
            "type": block_type,
            "source": { "type": "base64", "media_type": media, "data": base64(&bytes) },
        }));
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(json!({ "type": "text", "text": format!("File contents:\n{text}") }))
}

/// Small standalone base64 encoder — not worth another dependency.
/// Also used for Stripe's basic auth and the images sent to other providers.
pub(crate) fn base64_for(bytes: &[u8]) -> String {
    base64(bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    fn texts(parts: &[&str]) -> Vec<Value> {
        parts.iter().map(|t| json!({ "type": "text", "text": t })).collect()
    }

    #[test]
    fn every_text_block_is_kept_in_order_without_added_separators() {
        assert_eq!(response_text(&texts(&["Simple answer."])).as_deref(), Some("Simple answer."));
        let content = vec![
            json!({"type":"text","text":"I'll look that up.\n"}),
            json!({"type":"server_tool_use","id":"srvtoolu_01","name":"web_search","input":{"query":"test"}}),
            json!({"type":"web_search_tool_result","tool_use_id":"srvtoolu_01","content":[]}),
            json!({"type":"text","text":"The answer is 42."}),
        ];
        assert_eq!(response_text(&content).as_deref(), Some("I'll look that up.\nThe answer is 42."));
        // An empty leading block does not hide the answer.
        let content = vec![
            json!({"type":"text","text":""}),
            json!({"type":"web_search_tool_result","tool_use_id":"b","content":[]}),
            json!({"type":"text","text":"Here is the actual answer."}),
        ];
        assert_eq!(response_text(&content).as_deref(), Some("Here is the actual answer."));
        // Citations split a sentence across blocks: no newline is inserted.
        assert_eq!(
            response_text(&texts(&["Paris is the ", "capital", " of France."])).as_deref(),
            Some("Paris is the capital of France.")
        );
        // Search result text never leaks into the answer.
        let content = vec![
            json!({"type":"text","text":"Answer."}),
            json!({"type":"web_search_tool_result","tool_use_id":"a","content":[{"type":"web_search_result","title":"Page","text":"Leak"}]}),
        ];
        assert_eq!(response_text(&content).as_deref(), Some("Answer."));
        assert_eq!(response_text(&[json!({"type":"server_tool_use","id":"x"})]), None);
        assert_eq!(response_text(&texts(&["", "  \n"])), None);
    }

    #[test]
    fn a_refusal_or_an_empty_answer_is_an_error() {
        let refusal = json!({"stop_reason":"refusal","stop_details":{"explanation":"Not this."},"content":[]});
        assert_eq!(interpret(&refusal).unwrap_err(), "Not this.");
        assert_eq!(interpret(&json!({"stop_reason":"refusal"})).unwrap_err(), "Claude declined this one.");
        assert_eq!(interpret(&json!({"id":"x"})).unwrap_err(), "Unexpected API response.");
        assert_eq!(interpret(&json!({"content":[]})).unwrap_err(), "No response text.");
        let (blocks, text) = interpret(&json!({"content":[{"type":"text","text":" Hi "}]})).unwrap();
        assert_eq!(text, "Hi");
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn the_request_carries_history_web_search_and_the_new_turn_last() {
        let history = vec![json!({"role":"user","content":"a"}), json!({"role":"assistant","content":"b"})];
        let user = json!({"role":"user","content":[{"type":"text","text":"c"}]});
        let body = request_body("claude-x", "sys", &history, &user, false);
        assert!(request_body("claude-x", "sys", &history, &user, true).get("tools").is_none());
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["system"], "sys");
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["tools"][0]["name"], "web_search");
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][2], user);
    }

    #[test]
    fn context_is_sent_with_the_first_turn_only() {
        let ctx = ChatContext::Window { app_name: "Edge".into(), title: "Docs".into(), url: Some("https://x.dev".into()) };
        let first = user_content(true, Some(&ctx), "q");
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["text"], "Context — App: Edge, Window: Docs, URL: https://x.dev");
        assert_eq!(user_content(false, Some(&ctx), "q"), vec![json!({"type":"text","text":"q"})]);
        // A file that cannot be read still names itself.
        let ctx = ChatContext::File { name: "gone.pdf".into(), path: "/no/such/gone.pdf".into() };
        assert_eq!(user_content(true, Some(&ctx), "q")[0]["text"], "File: gone.pdf");
    }

    #[test]
    fn the_model_list_sits_next_to_the_messages_endpoint() {
        let url = Url::parse(DEFAULT_ENDPOINT).unwrap();
        assert_eq!(models_endpoint(&url).as_str(), "https://api.anthropic.com/v1/models?limit=100");
        let url = net::anthropic_endpoint("https://gw.example.com/anthropic").unwrap();
        assert_eq!(models_endpoint(&url).as_str(), "https://gw.example.com/anthropic/v1/models?limit=100");
        let list = json!({"data":[{"id":"claude-opus-5","display_name":"Claude Opus 5"},{"id":"claude-x"},{"nope":1}]});
        assert_eq!(
            parse_models(&list),
            vec![
                ModelInfo { id: "claude-opus-5".into(), label: "Claude Opus 5".into() },
                ModelInfo { id: "claude-x".into(), label: "claude-x".into() },
            ]
        );
    }
}
