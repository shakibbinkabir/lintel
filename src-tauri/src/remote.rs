// Claude Code sessions on SSH hosts. After upstream pull request 510 (its Linux
// tunnel), rebuilt on Windows' own OpenSSH client.
//
//   host:  claude → ~/.lintel/lintel-hook.py → ~/.lintel/lintel.sock
//                                                 │ ssh -R (the user's own ssh, keys and ~/.ssh/config)
//   here:  127.0.0.1:<port> → lintel-hook.exe --remote <host> → the relay pipe → island
//
// * The host needs python3: the relay there is a short Python script that hands
//   the event over and prints the answer. Installing shows its whole text and the
//   settings.json diff; the settings get a dated backup there, and nothing is
//   written before the click (Settings › Agents › SSH hosts).
// * Here, each event is run through this machine's own relay (`--remote`), so
//   the replies Claude Code reads, Allow / Deny and questions, are the very same
//   code as for local sessions (hook/src/reply.rs).
// * Only our own ssh may talk to the port: a connection from any other process
//   is dropped (its owner is looked up in the TCP table). The socket on the host
//   is the user's alone (sshd binds it 0600 in a 0700 folder). SSH is the only
//   authentication and nothing listens beyond loopback.
// * A host is trusted less than this machine: its events never find a window or
//   open anything here; Open terminal opens the folder on the host in VS Code's
//   Remote - SSH instead. Never the plan gauge, never `lintel_` fields of its own
//   (hook/src/remote.rs).
// * The tunnel ends with Lintel, even when Lintel is killed: ssh runs `cat` on
//   the host, fed by a pipe only Lintel holds, and goes when that pipe closes.
// * Nothing runs without a host listed. A tunnel that drops is tried again,
//   5 s at first and at most every 5 minutes.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config_file;
use crate::hooks::HookPreview;
use crate::platform;

/// Where Lintel keeps its things on a host, relative to $HOME.
const DIR: &str = ".lintel";
const RELAY_NAME: &str = "lintel-hook.py";

/// The relay installed on a host, shown whole before it is written.
const RELAY: &str = r#"#!/usr/bin/env python3
# Lintel's relay on this host. Claude Code runs it on each hook event: it hands
# the event to Lintel on your PC through the socket Lintel's ssh tunnel forwards
# (~/.lintel/lintel.sock) and prints Lintel's answer. With no tunnel it exits at
# once and prints nothing, so Claude Code carries on as usual.
# Installed from Lintel's Settings > Agents > SSH hosts; uninstalling there removes it.
import json, os, socket, sys

def main():
    event = sys.argv[1] if len(sys.argv) > 1 else ""
    data = sys.stdin.buffer.read(8 << 20)
    env = {k: os.environ[k] for k in ("CLAUDE_CODE_ENTRYPOINT",) if k in os.environ}
    line = json.dumps({"args": [event], "env": env, "stdin": data.decode("utf-8", "replace")}) + "\n"
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(0.5)
    s.connect(os.path.expanduser("~/.lintel/lintel.sock"))
    s.settimeout(115 if event == "PermissionRequest" else 5)
    s.sendall(line.encode())
    out = b""
    while len(out) < 65536:
        chunk = s.recv(4096)
        if not chunk:
            break
        out += chunk
    sys.stdout.buffer.write(out)

try:
    main()
except Exception:
    pass
"#;

// Scripts run on the host through `sh -c '…'`: none holds `'` or `\`, so any
// login shell (sh, bash, zsh, fish) hands them over unchanged.

/// After a marker line (a login shell may print something first): $HOME,
/// whether python3 is there, the sizes of settings.json and the relay ("-":
/// none), then both files' bytes.
const READ: &str = r#"echo lintel-read; echo "$HOME"; if command -v python3 >/dev/null 2>&1; then echo py; else echo nopy; fi
for f in "$HOME/.claude/settings.json" "$HOME/.lintel/lintel-hook.py"; do if [ -f "$f" ]; then wc -c < "$f"; else echo -; fi; done
cat "$HOME/.claude/settings.json" "$HOME/.lintel/lintel-hook.py" 2>/dev/null; true"#;

/// The relay on stdin, put in place.
const PUT_RELAY: &str = r#"set -e; d=$HOME/.lintel; mkdir -p "$d"; chmod 700 "$d"; t=$d/lintel-hook.py.new
cleanup() { rm -f "$t"; }; trap cleanup EXIT
cat > "$t"; chmod 700 "$t"; mv -f "$t" "$d/lintel-hook.py""#;

const REMOVE_RELAY: &str = r#"rm -f "$HOME/.lintel/lintel-hook.py""#;

/// The folder the tunnel's socket goes in, without a socket a dead tunnel left
/// (sshd will not bind over it). Prints $HOME.
const PREP: &str = r#"d=$HOME/.lintel; mkdir -p "$d"; chmod 700 "$d"; rm -f "$d/lintel.sock"; echo "$HOME""#;

/// The new settings.json on stdin. Backs the old one up first (and prints where),
/// writes through a symlink, and keeps the file's mode.
fn put_settings(stamp: &str) -> String {
    format!(
        r#"set -e; f=$HOME/.claude/settings.json; if [ -L "$f" ]; then f=$(readlink -f "$f"); fi; mkdir -p "$(dirname "$f")"
t=$f.lintel-new; cleanup() {{ rm -f "$t"; }}; trap cleanup EXIT
if [ -e "$f" ]; then b=$f.bak-{stamp}; n=0; while [ -e "$b" ]; do n=$((n+1)); b=$f.bak-{stamp}-$n; done; cp -p "$f" "$b"; cp -p "$f" "$t"; echo "$b"; fi
cat > "$t"; mv -f "$t" "$f""#
    )
}

// ── Hosts ─────────────────────────────────────────────────────────────────────

/// A destination as typed after `ssh` (an alias from ~/.ssh/config, or
/// user@host), and never an option. A port goes in ~/.ssh/config.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host.chars().all(|c| c.is_ascii_alphanumeric() || "@._-".contains(c))
}

fn hosts_path() -> std::path::PathBuf {
    platform::config_dir().join("ssh-hosts.json")
}

fn load() -> Vec<String> {
    let text = std::fs::read(hosts_path()).unwrap_or_default();
    let hosts: Vec<String> = serde_json::from_slice(&text).unwrap_or_default();
    hosts.into_iter().filter(|h| valid_host(h)).collect()
}

/// What Settings shows next to a host.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub host: String,
    /// "off" (the agents feature is off), "connecting", "connected" or "error".
    pub state: String,
    /// ssh's own words when it failed.
    pub error: String,
}

#[tauri::command]
pub fn ssh_hosts() -> Vec<HostStatus> {
    let tunnels = TUNNELS.lock().unwrap();
    load()
        .into_iter()
        .map(|host| match tunnels.get(&host) {
            Some(t) => t.status(),
            None => HostStatus { host, state: "off".into(), error: String::new() },
        })
        .collect()
}

/// Saves the list of hosts and starts or stops their tunnels.
#[tauri::command]
pub fn ssh_hosts_set(hosts: Vec<String>) -> Result<Vec<HostStatus>, String> {
    let mut list: Vec<String> = Vec::new();
    for host in hosts.iter().map(|h| h.trim()).filter(|h| !h.is_empty()) {
        if !valid_host(host) {
            return Err(crate::i18n::tf("{host} is not a host ssh takes (an alias or user@host; a port goes in ~/.ssh/config).", &[("host", host)]));
        }
        if !list.iter().any(|h| h == host) {
            list.push(host.to_string());
        }
    }
    platform::ensure_private_dir(&platform::config_dir()).map_err(|e| e.to_string())?;
    std::fs::write(hosts_path(), serde_json::to_vec(&list).unwrap_or_default()).map_err(|e| e.to_string())?;
    sync();
    Ok(ssh_hosts())
}

// ── Running ssh ───────────────────────────────────────────────────────────────

fn ssh() -> Result<Command, String> {
    let exe = platform::find_on_path("ssh").ok_or_else(|| crate::i18n::t("ssh is not installed (Windows' OpenSSH client)."))?;
    let mut cmd = Command::new(exe);
    // Never a question: an unknown host key or a password fails instead.
    cmd.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15"]);
    platform::no_console(&mut cmd);
    Ok(cmd)
}

/// Runs `script` on `host` with `input` on its stdin; its stdout, or ssh's own
/// words when it failed.
fn run(host: &str, script: &str, input: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = ssh()?
        .args(["--", host, &format!("sh -c '{script}'")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().map_err(|e| format!("ssh: {e}"))?;
    let _ = writer.join();
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(last_line(&out.stderr).unwrap_or_else(|| format!("ssh exited with {code}")));
    }
    Ok(out.stdout)
}

fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes).lines().rev().map(str::trim).find(|l| !l.is_empty()).map(String::from)
}

// ── Installing the hooks on a host ────────────────────────────────────────────

/// What is on a host now.
struct Found {
    home: String,
    python: bool,
    settings: Option<Vec<u8>>,
    relay: Option<Vec<u8>>,
}

fn read_host(host: &str) -> Result<Found, String> {
    if !valid_host(host) {
        return Err(crate::i18n::tf("{host} is not a host ssh takes (an alias or user@host; a port goes in ~/.ssh/config).", &[("host", host)]));
    }
    parse_found(&run(host, READ, b"")?).ok_or_else(|| crate::i18n::tf("{host} did not say where its home folder is.", &[("host", host)]))
}

fn parse_found(out: &[u8]) -> Option<Found> {
    const MARK: &[u8] = b"lintel-read\n";
    let mut rest = &out[out.windows(MARK.len()).position(|w| w == MARK)? + MARK.len()..];
    let mut line = || -> Option<String> {
        let end = rest.iter().position(|b| *b == b'\n')?;
        let text = String::from_utf8_lossy(&rest[..end]).trim().to_string();
        rest = &rest[end + 1..];
        Some(text)
    };
    let home = line()?;
    let python = line()? == "py";
    let sizes = [line()?, line()?];
    if !home.starts_with('/') {
        return None;
    }
    let mut take = |size: &str| -> Option<Option<Vec<u8>>> {
        if size == "-" {
            return Some(None);
        }
        let n: usize = size.parse().ok()?;
        let bytes = rest.get(..n)?.to_vec();
        rest = &rest[n..];
        Some(Some(bytes))
    };
    let settings = take(&sizes[0])?;
    let relay = take(&sizes[1])?;
    Some(Found { home, python, settings, relay })
}

fn fingerprint(found: &Found) -> String {
    let one = |b: &Option<Vec<u8>>| b.as_deref().map(config_file::fingerprint).unwrap_or_else(|| "none".into());
    format!("{}:{}", one(&found.settings), one(&found.relay))
}

fn relay_path(home: &str) -> String {
    format!("{home}/{DIR}/{RELAY_NAME}")
}

/// The hook command on a host. Without python3, or with the relay gone, `|| true`
/// keeps Claude Code from reporting a failing hook.
fn relay_command(relay: &str, event: &str) -> String {
    format!("python3 '{}' {event} 2>/dev/null || true", relay.replace('\'', r"'\''"))
}

/// settings.json before and after, as the diff shows them.
fn settings_change(found: &Found, install: bool) -> Result<(String, String), String> {
    let current = config_file::parse_json(found.settings.as_deref(), "settings.json")?;
    let relay = relay_path(&found.home);
    let next = if install {
        crate::hooks::merged_with(&current, &|event| relay_command(&relay, event))?
    } else {
        crate::hooks::without_ours(&current)?
    };
    let before = if found.settings.is_some() { config_file::pretty(&current) } else { String::new() };
    Ok((before, config_file::pretty(&next)))
}

fn preview_of(host: &str, found: &Found, install: bool) -> Result<HookPreview, String> {
    if install && !found.python {
        return Err(crate::i18n::tf("{host} has no python3, which the relay there needs.", &[("host", host)]));
    }
    let (before, after) = settings_change(found, install)?;
    let relay_before = String::from_utf8_lossy(found.relay.as_deref().unwrap_or_default()).to_string();
    let relay_after = if install { RELAY } else { "" };
    let settings = format!("{host}:~/.claude/settings.json");
    let diff = format!(
        "── {host}:~/{DIR}/{RELAY_NAME}\n{}\n── {settings}\n{}",
        config_file::unified_diff(&relay_before, relay_after),
        config_file::unified_diff(&before, &after),
    );
    Ok(HookPreview {
        diff,
        backup: if found.settings.is_some() { format!("{settings}.bak-{}", notch_core::win::stamp()) } else { String::new() },
        settings_path: settings,
        fingerprint: fingerprint(found),
    })
}

#[tauri::command]
pub async fn ssh_hooks_preview(host: String, install: bool) -> Result<HookPreview, String> {
    tauri::async_runtime::spawn_blocking(move || preview_of(&host, &read_host(&host)?, install))
        .await
        .map_err(|e| e.to_string())?
}

/// Only ever from an explicit click, for the diff that was shown: puts the relay
/// on the host (or takes it away) and writes settings.json, unless either file
/// changed since the preview. Returns where the backup went on the host.
#[tauri::command]
pub async fn ssh_hooks_apply(host: String, install: bool, fingerprint: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || apply(&host, install, &fingerprint))
        .await
        .map_err(|e| e.to_string())?
}

fn apply(host: &str, install: bool, expected: &str) -> Result<String, String> {
    let found = read_host(host)?;
    if fingerprint(&found) != expected {
        return Err(crate::i18n::tf(
            "{path} changed since the preview. Nothing was written — review the new diff.",
            &[("path", &format!("{host}:~/.claude/settings.json"))],
        ));
    }
    preview_of(host, &found, install)?;
    let (_, mut after) = settings_change(&found, install)?;
    after.push('\n');
    // debt: a file could change in the seconds between this read and the write; a lock on the host if that ever matters.
    // The relay goes in before the hooks that run it, and out after them.
    if install {
        run(host, PUT_RELAY, RELAY.as_bytes())?;
    }
    let out = run(host, &put_settings(&notch_core::win::stamp()), after.as_bytes())?;
    if !install {
        run(host, REMOVE_RELAY, b"")?;
    }
    crate::log::line(format!("ssh hooks {} on a host", if install { "installed" } else { "removed" }));
    Ok(last_line(&out).map(|b| format!("{host}:{b}")).unwrap_or_default())
}

// ── Tunnels ───────────────────────────────────────────────────────────────────

/// One host's tunnel, kept up by its own thread while the host is listed.
struct Tunnel {
    host: String,
    /// The running ssh: only it may connect to the port.
    child: Mutex<Option<Child>>,
    pid: AtomicU32,
    /// Connections being served, so a host cannot pile them up.
    open: AtomicUsize,
    stopped: Mutex<bool>,
    wake: Condvar,
    state: Mutex<(String, String)>,
}

impl Tunnel {
    fn status(&self) -> HostStatus {
        let (state, error) = self.state.lock().unwrap().clone();
        HostStatus { host: self.host.clone(), state, error }
    }
    fn set(&self, state: &str, error: String) {
        *self.state.lock().unwrap() = (state.to_string(), error);
    }
    fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }
    /// Waits `time` unless stopped meanwhile; true when stopped.
    fn sleep(&self, time: Duration) -> bool {
        let guard = self.stopped.lock().unwrap();
        *self.wake.wait_timeout_while(guard, time, |stopped| !*stopped).unwrap().0
    }
    fn stop(&self) {
        *self.stopped.lock().unwrap() = true;
        self.wake.notify_all();
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// On while the agents feature runs (pipe.rs).
static ON: AtomicBool = AtomicBool::new(false);
static TUNNELS: Mutex<std::collections::BTreeMap<String, Arc<Tunnel>>> = Mutex::new(std::collections::BTreeMap::new());
/// The loopback listener and its port, while any host is listed.
static LISTENER: Mutex<Option<(u16, tauri::async_runtime::JoinHandle<()>)>> = Mutex::new(None);

pub fn start() {
    ON.store(true, Ordering::SeqCst);
    sync();
}

pub fn stop() {
    ON.store(false, Ordering::SeqCst);
    sync();
}

/// Starts a tunnel for each listed host that has none, and stops the others.
fn sync() {
    let hosts = if ON.load(Ordering::SeqCst) { load() } else { Vec::new() };
    let mut tunnels = TUNNELS.lock().unwrap();
    tunnels.retain(|host, tunnel| {
        let keep = hosts.contains(host);
        if !keep {
            tunnel.stop();
        }
        keep
    });
    let mut listener = LISTENER.lock().unwrap();
    if hosts.is_empty() {
        if let Some((_, task)) = listener.take() {
            task.abort();
        }
        return;
    }
    let port = match listener.as_ref() {
        Some((port, _)) => *port,
        None => match listen() {
            Ok(started) => {
                let port = started.0;
                *listener = Some(started);
                port
            }
            Err(err) => {
                crate::log::line(format!("ssh hosts: no loopback port ({err})"));
                return;
            }
        },
    };
    for host in hosts {
        if tunnels.contains_key(&host) {
            continue;
        }
        let tunnel = Arc::new(Tunnel {
            host: host.clone(),
            child: Mutex::new(None),
            pid: AtomicU32::new(0),
            open: AtomicUsize::new(0),
            stopped: Mutex::new(false),
            wake: Condvar::new(),
            state: Mutex::new(("connecting".into(), String::new())),
        });
        let kept = tunnel.clone();
        std::thread::spawn(move || keep_up(kept, port));
        tunnels.insert(host, tunnel);
    }
}

fn keep_up(tunnel: Arc<Tunnel>, port: u16) {
    const FIRST: Duration = Duration::from_secs(5);
    let mut retry = FIRST;
    while !tunnel.is_stopped() {
        tunnel.set("connecting", String::new());
        let started = Instant::now();
        let error = forward(&tunnel, port);
        if tunnel.is_stopped() {
            return;
        }
        crate::log::line(format!("ssh tunnel: {error}"));
        tunnel.set("error", error);
        if started.elapsed() > Duration::from_secs(60) {
            retry = FIRST;
        }
        if tunnel.sleep(retry) {
            return;
        }
        retry = (retry * 2).min(Duration::from_secs(300));
    }
}

/// Holds `ssh -R` open until it ends; returns why it ended.
fn forward(tunnel: &Tunnel, port: u16) -> String {
    let home = match run(&tunnel.host, PREP, b"") {
        Ok(out) => last_line(&out).unwrap_or_default(),
        Err(err) => return err,
    };
    if !home.starts_with('/') || home.contains(':') {
        return format!("{}: no home folder", tunnel.host);
    }
    let spawned = ssh().and_then(|mut cmd| {
        cmd.args(["-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=2", "-R"])
            .arg(format!("{home}/{DIR}/lintel.sock:127.0.0.1:{port}"))
            .args(["--", &tunnel.host, "echo up; cat > /dev/null"])
            // Held by us alone: when Lintel goes, `cat` on the host ends and ssh with it.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ssh: {e}"))
    });
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => return err,
    };
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    tunnel.pid.store(child.id(), Ordering::SeqCst);
    *tunnel.child.lock().unwrap() = Some(child);
    if tunnel.is_stopped() {
        tunnel.stop();
    }
    let errors = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.map(|mut e| e.read_to_end(&mut text));
        last_line(&text)
    });
    if let Some(stdout) = stdout {
        let reader = BufReader::new(stdout);
        // `up` once the forward is in place; a login shell may print something first.
        for line in reader.lines().map_while(Result::ok) {
            if line.trim() == "up" {
                tunnel.set("connected", String::new());
                crate::log::line("ssh tunnel up");
            }
        }
    }
    let status = tunnel.child.lock().unwrap().take().and_then(|mut c| c.wait().ok());
    tunnel.pid.store(0, Ordering::SeqCst);
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    errors.join().ok().flatten().unwrap_or_else(|| format!("ssh exited with {code}"))
}

/// The loopback port every tunnel forwards to, served until no host is listed.
fn listen() -> std::io::Result<(u16, tauri::async_runtime::JoinHandle<()>)> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let task = tauri::async_runtime::spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    if let Ok(stream) = stream.into_std() {
                        std::thread::spawn(move || serve(stream, port));
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    });
    Ok((port, task))
}

/// What the host's relay sends: its command line, the one variable that matters
/// and the hook's stdin.
#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    stdin: String,
}

/// This machine's terminal context, which a host's event must not carry.
const LOCAL_CONTEXT: &[&str] = &[
    "CLAUDE_CODE_ENTRYPOINT", "LINTEL_WATCHED", "TERM_PROGRAM", "WT_SESSION", "TERM_SESSION_ID", "VSCODE_PID",
    "CLAUDE_CODE_SSE_PORT", "CURSOR_TRACE_ID", "VSCODE_GIT_ASKPASS_NODE",
];
const MAX_REQUEST: u64 = 24 << 20;
const MAX_OPEN: usize = 8;

/// One connection from a tunnel: the event goes through this machine's relay,
/// and what the relay prints goes back to the host.
fn serve(stream: TcpStream, port: u16) {
    let _ = stream.set_nonblocking(false);
    let Some(tunnel) = stream.peer_addr().ok().and_then(|peer| tcp_owner(peer.port(), port)).and_then(tunnel_of) else {
        crate::log::line("ssh hosts: refused a connection that did not come from a tunnel");
        return;
    };
    if tunnel.open.fetch_add(1, Ordering::SeqCst) >= MAX_OPEN {
        tunnel.open.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    relay(&stream, &tunnel.host);
    tunnel.open.fetch_sub(1, Ordering::SeqCst);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn relay(stream: &TcpStream, host: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = Vec::new();
    if BufReader::new(stream).take(MAX_REQUEST).read_until(b'\n', &mut line).is_err() {
        return;
    }
    let Ok(request) = serde_json::from_slice::<Request>(&line) else { return };
    let Some(event) = request.args.first().filter(|e| (1..=40).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphabetic())) else { return };
    let mut cmd = Command::new(crate::settings::hook_exe_path());
    cmd.arg(event).args(["--remote", host]);
    for var in LOCAL_CONTEXT {
        cmd.env_remove(var);
    }
    if let Some(entry) = request.env.get("CLAUDE_CODE_ENTRYPOINT").filter(|e| e.len() <= 64 && e.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))) {
        cmd.env("CLAUDE_CODE_ENTRYPOINT", entry);
    }
    let spawned = platform::no_console(&mut cmd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = spawned else { return };
    let input = request.stdin.into_bytes();
    let mut stdin = child.stdin.take();
    std::thread::spawn(move || {
        let _ = stdin.as_mut().map(|s| s.write_all(&input));
    });
    let stdout = child.stdout.take();
    let child = Arc::new(Mutex::new(Some(child)));
    // The relay on the host went (the question was answered in its terminal):
    // ours goes too, and the card with it.
    if let Ok(mut watch) = stream.try_clone() {
        let child = child.clone();
        std::thread::spawn(move || {
            let _ = watch.set_read_timeout(None);
            let mut sink = [0u8; 256];
            while matches!(watch.read(&mut sink), Ok(n) if n > 0) {}
            if let Some(child) = child.lock().unwrap().as_mut() {
                let _ = child.kill();
            }
        });
    }
    let mut reply = Vec::new();
    if let Some(mut out) = stdout {
        let _ = out.read_to_end(&mut reply);
    }
    if let Some(mut child) = child.lock().unwrap().take() {
        let _ = child.wait();
    }
    let _ = (&*stream).write_all(&reply);
}

fn tunnel_of(pid: u32) -> Option<Arc<Tunnel>> {
    TUNNELS.lock().unwrap().values().find(|t| t.pid.load(Ordering::SeqCst) == pid).cloned()
}

#[repr(C)]
struct TcpRow {
    state: u32,
    local_addr: u32,
    local_port: u32,
    remote_addr: u32,
    remote_port: u32,
    pid: u32,
}

#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(table: *mut core::ffi::c_void, size: *mut u32, order: i32, family: u32, class: u32, reserved: u32) -> u32;
}

/// The process on the other end of a loopback connection to `port`, by the
/// port it connects from.
fn tcp_owner(from: u16, port: u16) -> Option<u32> {
    const AF_INET: u32 = 2;
    const TCP_TABLE_OWNER_PID_CONNECTIONS: u32 = 4;
    let mut size = 0u32;
    for _ in 0..4 {
        let mut buf = vec![0u32; (size as usize).div_ceil(4).max(1)];
        let code = unsafe { GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_CONNECTIONS, 0) };
        if code == 122 {
            continue; // ERROR_INSUFFICIENT_BUFFER: `size` now says how much
        }
        if code != 0 {
            return None;
        }
        let count = (buf[0] as usize).min((buf.len() - 1) / 6);
        // SAFETY: the table is a count followed by `count` rows of six u32s, all inside `buf`.
        let rows = unsafe { std::slice::from_raw_parts(buf[1..].as_ptr().cast::<TcpRow>(), count) };
        let loopback = u32::from_ne_bytes([127, 0, 0, 1]);
        return rows
            .iter()
            .find(|r| {
                r.local_addr == loopback
                    && u16::from_be(r.local_port as u16) == from
                    && u16::from_be(r.remote_port as u16) == port
            })
            .map(|r| r.pid);
    }
    None
}

// ── Sessions from a host ──────────────────────────────────────────────────────

/// Remote sessions at once, all hosts together: past it, a new one is not kept.
const MAX_SESSIONS: usize = 256;
/// Session id → host, for Open terminal.
static SESSIONS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// A host's event reached the island (pipe.rs).
pub fn note(payload: &Value, event: &str) {
    let (Some(id), Some(host)) = (
        payload.get("session_id").and_then(Value::as_str),
        payload.get("lintel_remote").and_then(Value::as_str),
    ) else {
        return;
    };
    let mut sessions = SESSIONS.lock().unwrap();
    let sessions = sessions.get_or_insert_with(HashMap::new);
    if event == "SessionEnd" {
        sessions.remove(id);
    } else if sessions.len() < MAX_SESSIONS && id.len() <= 128 {
        sessions.insert(id.to_string(), host.to_string());
    }
}

/// Open terminal for a session on a host: nothing here is its window, so the
/// folder opens on the host in VS Code's Remote - SSH. None: not a host's session.
pub fn open_session(session_id: Option<&str>, path: Option<&str>) -> Option<bool> {
    let host = SESSIONS.lock().unwrap().as_ref()?.get(session_id?)?.clone();
    let Some(path) = path.filter(|p| p.starts_with('/') && !p.chars().any(char::is_control)) else { return Some(false) };
    let Some(code) = platform::find_on_path("code").filter(|_| valid_host(&host)) else { return Some(false) };
    let mut cmd = Command::new(code);
    cmd.args(["--remote", &format!("ssh-remote+{host}"), path]);
    Some(platform::no_console(&mut cmd).spawn().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_never_options() {
        for ok in ["devbox", "me@build.example.org", "host-2_a"] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in ["", "-oProxyCommand=x", "a b", "a;b", "a'b", "$(x)", "a\nb", "h:22"] {
            assert!(!valid_host(bad), "{bad:?}");
        }
    }

    #[test]
    fn scripts_survive_any_login_shell() {
        for script in [READ, PUT_RELAY, REMOVE_RELAY, PREP, &put_settings("20261020-030405")] {
            assert!(!script.contains(['\'', '\\']), "{script}");
        }
    }

    #[test]
    fn what_a_host_has_is_read_back_exactly() {
        let mut out = b"Welcome!\nlintel-read\n/home/me\npy\n7\n-\n".to_vec();
        out.extend_from_slice(b"{\"a\":1}");
        let found = parse_found(&out).unwrap();
        assert_eq!((found.home.as_str(), found.python), ("/home/me", true));
        assert_eq!(found.settings.as_deref(), Some(&b"{\"a\":1}"[..]));
        assert!(found.relay.is_none());
        assert_eq!(fingerprint(&found).split(':').nth(1), Some("none"));
        // Short of what it announced, or no home: not a reading at all.
        assert!(parse_found(b"lintel-read\n/home/me\npy\n9\n-\n{}").is_none());
        assert!(parse_found(b"lintel-read\n\npy\n-\n-\n").is_none());
        assert!(parse_found(b"/home/me\npy\n-\n-\n").is_none(), "no marker, no reading");
    }

    #[test]
    fn the_hooks_on_a_host_run_its_relay_and_go_cleanly() {
        let found = Found { home: "/home/me".into(), python: true, settings: Some(br#"{"model":"opus"}"#.to_vec()), relay: None };
        let plan = preview_of("devbox", &found, true).unwrap();
        assert!(plan.diff.contains("+ #!/usr/bin/env python3"), "the relay is shown whole");
        assert!(plan.diff.contains("python3 '/home/me/.lintel/lintel-hook.py' Stop 2>/dev/null || true"));
        let (_, after) = settings_change(&found, true).unwrap();
        let after: Value = serde_json::from_str(&after).unwrap();
        assert!(crate::hooks::has_ours(&after));
        let back = crate::hooks::without_ours(&after).unwrap();
        assert_eq!(back, serde_json::json!({ "model": "opus" }));
        // No python3: nothing to install.
        let bare = Found { python: false, ..found };
        assert!(preview_of("devbox", &bare, true).is_err());
    }
}
