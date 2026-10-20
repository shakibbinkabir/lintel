// Relay server for lintel-hook.
//
// The named pipe `\\.\pipe\lintel-<sid>`, one instance per connection.
// Every hook event is
// forwarded to the island as a `hook` event. `PermissionRequest` is the only one
// that keeps its connection open: it waits for the island's decision and writes
// it back on the same connection, which is how approving from the island works.
//
// Claude Code is never blocked by us. Three things guarantee it:
//   * lintel-hook gives the connection 300 ms and exits cleanly if we are closed;
//   * we only wait for a human once the island has *confirmed* the card is on
//     screen, so a paused island or a webview that is not listening costs a few
//     hundred milliseconds, not two minutes;
//   * whatever happens we drop the connection after the decision timeout, and
//     the terminal takes over.
//
// What we write back is the bare word `allow` or `deny` (or a plan card's
// `plan_auto`, `plan_manual`, `plan_change`) — or, when the request
// was Claude Code asking a question, `{"answers":{…}}` with what was picked on
// the island. Turning either into the documented hookSpecificOutput JSON is
// lintel-hook's job, so the wire format Claude Code expects lives in exactly
// one place.
//
// Who may talk to it (ported from upstream `windows/src-tauri/src/pipe.rs`,
// pull requests 426 and 390): the pipe is open to the current user and SYSTEM
// only, a client gets 5 s to send its request, and an Allow / Deny card is only
// raised for the relay the app deployed, under an id nobody can guess. Programs
// running as the same user stay inside the trust boundary, as before.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::task::Poll;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::island::WINDOW_LABEL;
use crate::log;
use crate::platform::PipeSecurity;
use crate::session_window;

/// Slightly under lintel-hook's own 110 s wait, so we always answer first.
const DECISION_TIMEOUT: Duration = Duration::from_secs(108);
/// How long the island gets to say "the card is up". This is the whole of B4:
/// without it, an island that is paused, hidden behind a crashed webview or
/// simply not listening would leave Claude Code staring at a prompt nobody can
/// see for nearly two minutes.
const ACK_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_PAYLOAD: usize = 1 << 20;
/// Cap on reading a client's request. lintel-hook writes it in one go as soon as
/// it connects, and its whole run fits in 2 s when nobody waits for an answer:
/// a silent connection, or one that trickles bytes in, must not hold a pipe
/// instance. Only the read is bounded here; waiting for a decision has its own
/// timeouts (ACK_TIMEOUT, DECISION_TIMEOUT).
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause after an accept error before listening again, so a lasting failure
/// does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(200);
/// Pause between two attempts to take the pipe name, once the quick ones failed.
const FIRST_INSTANCE_RETRY: Duration = Duration::from_secs(2);

/// What the island can say about a permission request.
pub enum Reply {
    /// The card is on screen and a human can act on it.
    Ack,
    /// A human clicked: `allow` or `deny`.
    Decision(String),
    /// Nobody can act on it — paused, or another request already holds the card.
    Decline,
}

/// Permission requests the island has been told about.
#[derive(Default)]
pub struct Pending(pub Mutex<HashMap<String, mpsc::Sender<Reply>>>);

/// `\\.\pipe\lintel-<sid>` — must match lintel-hook's `pipe_path()` exactly.
pub fn pipe_name() -> String {
    let key = crate::platform::current_user_sid()
        .unwrap_or_else(|| std::env::var("USERNAME").unwrap_or_else(|_| "user".into()));
    format!(r"\\.\pipe\lintel-{key}")
}

/// The task serving the pipe while the agents feature runs.
static SERVER: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);

/// Serves the pipe until `stop`. A second call while it is being served does nothing.
pub fn start(app: AppHandle) {
    let mut slot = SERVER.lock().unwrap();
    if slot.as_ref().is_some_and(|task| !task.inner().is_finished()) {
        return;
    }
    *slot = Some(tauri::async_runtime::spawn(serve(app)));
    // The tunnels of the SSH hosts listed in Settings, if any.
    crate::remote::start();
}

/// Stops serving. Whoever still waits for an answer gets none, and their agent
/// asks in its terminal, exactly as if Lintel were closed.
pub fn stop(app: &AppHandle) {
    if let Some(task) = SERVER.lock().unwrap().take() {
        task.abort();
    }
    crate::remote::stop();
    app.state::<Pending>().0.lock().unwrap().clear();
}

async fn serve(app: AppHandle) {
    let name = pipe_name();
    // Never fall back to the default security descriptor. Without a SID,
    // lintel-hook refuses to talk to a server it cannot check anyway.
    let Some(security) = crate::platform::current_user_sid().and_then(|sid| PipeSecurity::for_user(&sid)) else {
        log::line("relay pipe not opened: cannot restrict it to the current user");
        return;
    };
    let mut server = first_instance(&name, &security).await;
    // An error is logged once per streak: repeated every 200 ms, it would fill the log.
    let mut failing = false;
    loop {
        // Ok at once for an instance a client is already on (a retry below).
        let accepted = server.connect().await;
        // The next instance is created before this one is handed off, so the
        // pipe name is never free, not even for a moment.
        let next = match create_instance(&name, &security, false) {
            Ok(next) => next,
            Err(err) => {
                if !std::mem::replace(&mut failing, true) {
                    log::line(format!("relay pipe: cannot create a new instance ({err}), retrying"));
                }
                tokio::time::sleep(ACCEPT_RETRY).await;
                continue;
            }
        };
        // Hand the connected instance to a task and listen on the fresh one.
        let instance = std::mem::replace(&mut server, next);
        match accepted {
            Ok(()) => {
                failing = false;
                let app = app.clone();
                tauri::async_runtime::spawn(async move { handle(app, instance).await });
            }
            // Any error discards the instance, and listening resumes on the fresh one.
            Err(err) => {
                if !std::mem::replace(&mut failing, true) {
                    log::line(format!("relay pipe: a connection failed ({err}), listening again"));
                }
                drop(instance);
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
}

/// The first instance, with FILE_FLAG_FIRST_PIPE_INSTANCE: we refuse to join a
/// pipe somebody else already owns under our name, rather than serving on top
/// of it. The name may also still be held by a connection of the previous run
/// (switched off and on again, a restart, an update): we retry, quickly at
/// first, instead of leaving the relay off for the whole session.
async fn first_instance(name: &str, security: &PipeSecurity) -> NamedPipeServer {
    let mut attempts = 0u32;
    loop {
        match create_instance(name, security, true) {
            Ok(server) => {
                if attempts >= 5 {
                    log::line("relay pipe opened");
                }
                return server;
            }
            Err(err) => {
                attempts += 1;
                if attempts == 5 {
                    // Not necessarily a name already taken: any CreateNamedPipeW error ends up here.
                    log::line(format!("relay pipe: first instance failed ({err}), retrying every 2 s"));
                }
                tokio::time::sleep(if attempts < 5 { ACCEPT_RETRY } else { FIRST_INSTANCE_RETRY }).await;
            }
        }
    }
}

/// One instance of the pipe, open to the current user and SYSTEM only.
fn create_instance(name: &str, security: &PipeSecurity, first: bool) -> std::io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    // reject_remote_clients is already tokio's default; set here so we do not
    // depend on it: a client from another machine (SMB) has no business here.
    options.first_pipe_instance(first).reject_remote_clients(true);
    // SAFETY: the pointer refers to a valid SECURITY_ATTRIBUTES for the whole
    // call, and CreateNamedPipeW does not keep it afterwards.
    security.with_attributes(|attributes| unsafe { options.create_with_security_attributes_raw(name, attributes) })
}

/// One accepted relay connection, whatever carries it.
trait Relay: AsyncRead + AsyncWrite + Unpin {
    /// Ends the conversation once everything has been written.
    fn finish(&mut self) {}
    /// The relay process on the other end, where the OS says.
    fn client_pid(&self) -> Option<u32> {
        None
    }
}

impl Relay for NamedPipeServer {
    fn finish(&mut self) {
        let _ = self.disconnect();
    }
    fn client_pid(&self) -> Option<u32> {
        use std::os::windows::io::AsRawHandle;
        crate::platform::pipe_client_pid(self.as_raw_handle())
    }
}

async fn handle(app: AppHandle, mut pipe: impl Relay) {
    let Some(line) = read_request(&mut pipe, REQUEST_READ_TIMEOUT).await else { return };
    let Ok(mut payload) = serde_json::from_slice::<Value>(&line) else { return };
    if !payload.is_object() {
        return;
    }

    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Any program of the user may send events: they are only shown. An Allow /
    // Deny card is not display, so a permission request is only taken from the
    // relay the app deployed. Dropped without a word: the sender reads no
    // decision, and its agent asks in the terminal.
    if event == "PermissionRequest" && !pipe.client_pid().is_some_and(crate::platform::is_deployed_relay) {
        log::line("refused a permission request that did not come from the deployed relay");
        return;
    }

    // A session on an SSH host (remote.rs) has no window here: our own relay ran for it.
    if payload.get("lintel_remote").is_some() {
        crate::remote::note(&payload, &event);
    } else {
        note_session_window(&pipe, &payload, &event);
    // Claude Code's own one-line summary of the session, from its terminal's title.
    if event != "StatusLine" {
        note_window_title(&mut payload);
    }
    }
    // A finished turn stays quiet when its terminal is the window in front.
    if event == "Stop"
        && payload.get("session_id").and_then(Value::as_str).and_then(session_window::lookup).is_some_and(|owners| crate::platform::in_front_of(&owners))
    {
        payload["lintel_front"] = json!(true);
    }
    // Counts for the weekly recap — never the command, path or prompt itself.
    crate::recap::observe(&app, &payload);

    if event != "PermissionRequest" {
        // The status line relay calls in with every Claude Code update: not log-worthy.
        if event != "StatusLine" {
            log::line(format!("hook {event}"));
        }
        let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
        pipe.finish();
        return;
    }

    // The id is what the approval commands are driven with: random, so it
    // cannot be worked out from another one. No randomness, no card.
    let Some(id) = crate::platform::random_token() else {
        log::line("no request id could be made, terminal takes over");
        return;
    };
    let (tx, mut rx) = mpsc::channel::<Reply>(4);
    {
        let pending = app.state::<Pending>();
        pending.0.lock().unwrap().insert(id.clone(), tx);
    }
    crate::recap::note_request(&app, &id, &payload);
    payload["request_id"] = json!(id);
    log::line(format!("hook PermissionRequest id={}", short(&id)));
    let _ = app.emit_to(WINDOW_LABEL, "hook", payload);

    // Answered in the terminal instead: Claude Code ends the relay (or the shell
    // it ran in, and the relay follows), its end of the pipe closes, and the
    // card goes at once rather than offering a question already settled.
    // (After upstream pull request 740.)
    let decision = {
        let mut decided = std::pin::pin!(wait_for_decision(&id, &mut rx));
        let mut hung_up = std::pin::pin!(client_gone(&mut pipe));
        std::future::poll_fn(|cx| match decided.as_mut().poll(cx) {
            Poll::Ready(d) => Poll::Ready(Ok(d)),
            Poll::Pending => hung_up.as_mut().poll(cx).map(Err),
        })
        .await
    };
    let decision = decision.unwrap_or_else(|()| {
        log::line(format!("hook id={} answered elsewhere, the relay hung up", short(&id)));
        crate::recap::forget_request(&app, &id);
        let _ = app.emit_to(WINDOW_LABEL, "hook-resolved", &id);
        None
    });
    app.state::<Pending>().0.lock().unwrap().remove(&id);

    // No decision: say nothing at all. lintel-hook then writes nothing to stdout
    // and Claude Code asks in the terminal, exactly as if Lintel were closed.
    if let Some(d) = decision {
        let _ = pipe.write_all(format!("{d}\n").as_bytes()).await;
        let _ = pipe.flush().await;
    }
    pipe.finish();
}

/// Resolves once the relay has gone (end of stream or a broken pipe). It sends
/// nothing after its one line, so whatever a read brings is skipped.
async fn client_gone(pipe: &mut (impl AsyncRead + Unpin)) {
    let mut probe = [0u8; 64];
    while matches!(pipe.read(&mut probe).await, Ok(n) if n > 0) {}
}

/// The client's first line without its newline, or everything it sent before
/// closing. Reading stops past MAX_PAYLOAD, and a request cut there does not
/// parse. None on a read error, or if nothing complete arrived within `limit`.
async fn read_request(pipe: &mut (impl AsyncRead + Unpin), limit: Duration) -> Option<Vec<u8>> {
    let read = async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) => return Some(buf),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    // Only the bytes just read are searched: a client that
                    // dribbles them in must not cost a scan of the whole buffer each time.
                    if let Some(i) = chunk[..n].iter().position(|b| *b == b'\n') {
                        buf.truncate(buf.len() - n + i);
                        return Some(buf);
                    }
                    if buf.len() > MAX_PAYLOAD {
                        return Some(buf);
                    }
                }
                Err(_) => return None,
            }
        }
    };
    tokio::time::timeout(limit, read).await.ok().flatten()
}

/// Finds, once per session, the window it runs in — see session_window.rs.
///
/// Only while the session is unknown, so the process snapshot is not taken on
/// every event. The relay lists the processes it runs under (`lintel_ancestors`):
/// it has usually exited by now, only a permission request waits for us. An
/// older relay, or another program, is walked up from by its PID instead,
/// which only works while it still runs; else a later event tries again.
fn note_session_window(pipe: &impl Relay, payload: &Value, event: &str) {
    let Some(session) = payload.get("session_id").and_then(Value::as_str) else { return };
    if event == "SessionEnd" {
        session_window::forget(session);
        return;
    }
    if session_window::known(session) {
        return;
    }
    let listed: Vec<u32> = payload
        .get("lintel_ancestors")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_u64).filter_map(|p| u32::try_from(p).ok()).collect())
        .unwrap_or_default();
    let ancestors = if listed.is_empty() {
        let Some(relay) = pipe.client_pid() else { return };
        crate::platform::process_ancestors(relay)
    } else {
        crate::platform::below_tree_tops(&listed)
    };
    // No ancestors: the relay was already gone, so try again next time. Some,
    // but none with a window (a classic console): settled, VS Code it is.
    if !ancestors.is_empty() {
        session_window::remember(session, crate::platform::window_owners(&ancestors));
    }
}

/// The title of the terminal window a Claude Code session runs in, for the island to name the
/// pill with. Only for a session whose window was found: one window listing per event.
fn note_window_title(payload: &mut Value) {
    let agent = payload.get("lintel_agent").and_then(Value::as_str).unwrap_or_default();
    if !agent.is_empty() && !agent.starts_with("claude-") {
        return;
    }
    let Some(owners) = payload.get("session_id").and_then(Value::as_str).and_then(session_window::lookup) else { return };
    let folder = payload.get("cwd").and_then(Value::as_str).map(session_window::folder_name).unwrap_or_default().to_string();
    if let Some(title) = crate::platform::session_window_title(&owners, &folder) {
        payload["lintel_window_title"] = json!(title);
    }
}

/// Two waits: a short one for "the card is up", then the long one for a human.
async fn wait_for_decision(id: &str, rx: &mut mpsc::Receiver<Reply>) -> Option<String> {
    match tokio::time::timeout(ACK_TIMEOUT, rx.recv()).await {
        Ok(Some(Reply::Ack)) => {}
        // A click that beats the ack is still a click.
        Ok(Some(Reply::Decision(d))) => {
            log::line(format!("hook id={} answered {}", short(id), loggable(&d)));
            return Some(d);
        }
        Ok(Some(Reply::Decline)) => {
            log::line(format!("hook id={} not shown — terminal takes over", short(id)));
            return None;
        }
        Ok(None) => return None,
        Err(_) => {
            log::line(format!("hook id={} island never acknowledged — terminal takes over", short(id)));
            return None;
        }
    }

    // A second ack (the island drew the card again, the page reloaded) is not a
    // decision and must not end the wait; the deadline stays where it was.
    let deadline = tokio::time::Instant::now() + DECISION_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Reply::Decision(d))) => {
                log::line(format!("hook id={} answered {}", short(id), loggable(&d)));
                return Some(d);
            }
            Ok(Some(Reply::Decline)) => {
                log::line(format!("hook id={} released without a decision", short(id)));
                return None;
            }
            Ok(Some(Reply::Ack)) => continue,
            _ => {
                log::line(format!("hook id={} timed out — terminal takes over", short(id)));
                return None;
            }
        }
    }
}

/// Enough of a request id to follow it through the log, never the whole of it.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// The log says a question was answered, never with what.
fn loggable(decision: &str) -> &str {
    if decision.starts_with('{') { "a question" } else { decision }
}

fn send(app: &AppHandle, request_id: &str, reply: Reply, keep: bool) {
    let sender = {
        let pending = app.state::<Pending>();
        let mut map = pending.0.lock().unwrap();
        if keep { map.get(request_id).cloned() } else { map.remove(request_id) }
    };
    match sender {
        Some(tx) => {
            let _ = tx.try_send(reply);
        }
        None => log::line(format!("reply for id={} — no pending request", short(request_id))),
    }
}

/// The island has the card on screen; the long wait may begin.
pub fn acknowledge(app: &AppHandle, request_id: &str) {
    send(app, request_id, Reply::Ack, true);
}

/// Nobody can act on this one — paused, or another card already holds the view.
pub fn decline(app: &AppHandle, request_id: &str) {
    log::line(format!("decline id={}", short(request_id)));
    send(app, request_id, Reply::Decline, false);
}

/// Called by the island's Allow / Deny buttons. Only ever a bare word: turning
/// it into Claude Code's JSON is lintel-hook's job.
pub fn answer(app: &AppHandle, request_id: &str, decision: &str) {
    // Always (chat-plus): the Allow click that follows it goes to the relay as "always".
    let always = notch_feat_chat_plus::take_always(request_id);
    let word = match decision {
        "allow" | "always" => if always { "always" } else { "allow" },
        // The plan card's (Claude Code's ExitPlanMode): the relay sends them only for a plan.
        "plan_auto" | "plan_manual" | "plan_change" => decision,
        _ => "deny",
    };
    log::line(format!("decision id={} {word}", short(request_id)));
    send(app, request_id, Reply::Decision(word.to_string()), false);
}

/// Called when an option is picked for a question Claude Code asked. `answers`
/// maps each question's text to the chosen label, which is the shape
/// AskUserQuestion takes them in. `other` lists the questions answered in words
/// under "Other…", which the relay then accepts as free text (and only those).
pub fn answer_question(
    app: &AppHandle,
    request_id: &str,
    answers: &HashMap<String, serde_json::Value>,
    other: &[String],
) {
    log::line(format!("decision id={} answered a question", short(request_id)));
    // One line: the relay reads up to the first newline (a newline typed in a
    // free-text answer is escaped by the JSON writer).
    let line = json!({ "answers": answers, "other": other }).to_string();
    send(app, request_id, Reply::Decision(line), false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    #[test]
    fn the_request_is_the_first_line() {
        block_on(async {
            let (mut client, mut server) = tokio::io::duplex(64);
            client.write_all(b"{\"a\":1}\n{\"b\":2}\n").await.unwrap();
            let line = read_request(&mut server, Duration::from_secs(1)).await;
            assert_eq!(line.as_deref(), Some(&b"{\"a\":1}"[..]));
        });
    }

    #[test]
    fn a_client_that_closes_without_a_newline_is_still_read() {
        block_on(async {
            let (mut client, mut server) = tokio::io::duplex(64);
            client.write_all(b"{\"a\":1}").await.unwrap();
            drop(client);
            let line = read_request(&mut server, Duration::from_secs(1)).await;
            assert_eq!(line.as_deref(), Some(&b"{\"a\":1}"[..]));
        });
    }

    #[test]
    fn a_relay_that_hangs_up_is_seen_at_once_and_a_waiting_one_is_not() {
        block_on(async {
            let (mut ours, theirs) = tokio::io::duplex(64);
            let waiting = tokio::time::timeout(Duration::from_millis(50), client_gone(&mut ours)).await;
            assert!(waiting.is_err(), "a live relay must not count as gone");
            drop(theirs);
            let gone = tokio::time::timeout(Duration::from_millis(50), client_gone(&mut ours)).await;
            assert!(gone.is_ok(), "a closed relay must be seen at once");
        });
    }

    #[test]
    fn a_silent_client_is_dropped_after_the_limit() {
        block_on(async {
            let (_client, mut server) = tokio::io::duplex(64);
            let started = Instant::now();
            assert_eq!(read_request(&mut server, Duration::from_millis(100)).await, None);
            assert!(started.elapsed() < Duration::from_secs(2));
        });
    }

    #[test]
    fn a_client_that_never_ends_its_line_is_dropped_after_the_limit() {
        block_on(async {
            let (mut client, mut server) = tokio::io::duplex(64);
            // One byte every 10 ms, never a newline.
            let drip = tokio::spawn(async move {
                while client.write_all(b"x").await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            let started = Instant::now();
            assert_eq!(read_request(&mut server, Duration::from_millis(150)).await, None);
            assert!(started.elapsed() < Duration::from_secs(2));
            drop(server);
            let _ = drip.await;
        });
    }

    /// A pipe created the way the relay creates it: a DACL holding only the user
    /// and SYSTEM, the name taken exclusively, and the same user's relay still
    /// gets through.
    #[test]
    fn the_relay_pipe_is_reserved_to_the_user_and_still_serves_them() {
        use std::io::Write;
        use std::os::windows::io::AsRawHandle;

        let sid = crate::platform::current_user_sid().expect("the current user's SID");
        let security = PipeSecurity::for_user(&sid).expect("a security descriptor");
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        // A name of its own, never the live relay's.
        let name = format!(r"\\.\pipe\{}-test-{}-{nanos}", notch_brand::SLUG, std::process::id());

        block_on(async {
            let mut server = create_instance(&name, &security, true).expect("first instance");

            let dacl = crate::platform::dacl_sddl(server.as_raw_handle()).expect("the pipe's DACL");
            assert!(dacl.starts_with("D:P"), "{dacl}");
            // Not `sid` as is: Windows writes some accounts as an alias (`LA` for
            // the built-in Administrator, `SY` for SYSTEM).
            let me = crate::platform::sddl_trustee(&sid).expect("the SID as SDDL writes it");
            assert!(dacl.contains(&format!(";;;{me})")), "{dacl}");
            assert!(dacl.contains(";;;SY)"), "{dacl}");
            assert_eq!(dacl.matches("(A;").count(), 2, "{dacl}");
            for nobody in [";;;WD)", ";;;AN)", ";;;BA)"] {
                assert!(!dacl.contains(nobody), "{dacl}");
            }

            // Like lintel-hook: opened for reading and writing, one line. Before
            // any other instance exists, so that `server` is the one that gets it.
            let mut client = std::fs::OpenOptions::new().read(true).write(true).open(&name).expect("client");
            client.write_all(b"{\"hook_event_name\":\"Stop\"}\n").unwrap();

            // The name is ours: another "first" instance is refused, a further
            // instance is accepted.
            assert!(create_instance(&name, &security, true).is_err());
            let _next = create_instance(&name, &security, false).expect("next instance");

            server.connect().await.expect("connect");
            // This test binary is not the deployed relay: it could not raise a card.
            assert_eq!(server.client_pid(), Some(std::process::id()));
            assert!(!server.client_pid().is_some_and(crate::platform::is_deployed_relay));
            let line = read_request(&mut server, Duration::from_secs(1)).await;
            assert_eq!(line.as_deref(), Some(&b"{\"hook_event_name\":\"Stop\"}"[..]));
        });
    }

    fn wait(rx: &mut mpsc::Receiver<Reply>) -> Option<String> {
        block_on(wait_for_decision("t", rx))
    }

    #[test]
    fn an_ack_then_a_click_answers() {
        let (tx, mut rx) = mpsc::channel::<Reply>(4);
        tx.try_send(Reply::Ack).ok().unwrap();
        tx.try_send(Reply::Decision("allow".into())).ok().unwrap();
        assert_eq!(wait(&mut rx).as_deref(), Some("allow"));
    }

    #[test]
    fn a_second_ack_does_not_end_the_wait() {
        // The island acknowledging again (the card drawn again, the page
        // reloaded) used to end the request as a timeout: the click that
        // followed then went nowhere.
        let (tx, mut rx) = mpsc::channel::<Reply>(4);
        tx.try_send(Reply::Ack).ok().unwrap();
        tx.try_send(Reply::Ack).ok().unwrap();
        tx.try_send(Reply::Decision("deny".into())).ok().unwrap();
        assert_eq!(wait(&mut rx).as_deref(), Some("deny"));
    }

    #[test]
    fn a_click_that_beats_the_ack_still_counts() {
        let (tx, mut rx) = mpsc::channel::<Reply>(4);
        tx.try_send(Reply::Decision("allow".into())).ok().unwrap();
        assert_eq!(wait(&mut rx).as_deref(), Some("allow"));
    }

    #[test]
    fn a_decline_or_an_island_that_went_away_means_the_terminal_asks() {
        let (tx, mut rx) = mpsc::channel::<Reply>(4);
        tx.try_send(Reply::Decline).ok().unwrap();
        assert_eq!(wait(&mut rx), None);
        // Acknowledged, then released without a click.
        let (tx, mut rx) = mpsc::channel::<Reply>(4);
        tx.try_send(Reply::Ack).ok().unwrap();
        tx.try_send(Reply::Decline).ok().unwrap();
        assert_eq!(wait(&mut rx), None);
        // The sender dropped: the wait ends at once.
        let (tx, mut rx) = mpsc::channel::<Reply>(4);
        drop(tx);
        assert_eq!(wait(&mut rx), None);
    }

    #[test]
    fn request_ids_are_long_random_and_never_whole_in_the_log() {
        let a = crate::platform::random_token().expect("a token");
        let b = crate::platform::random_token().expect("a token");
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(short(&a).len(), 8);
        assert_eq!(short("abc"), "abc");
        // Not a char boundary at 8: the whole text rather than a panic.
        assert_eq!(short("aééééé"), "aééééé");
    }
}
