// Incremental reader of the local Claude Code and Codex logs. Read-only; it keeps counters, model names,
// folder names and turn boundaries, never message text. No network.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const DAY: i64 = 86_400_000;
/// How far back the logs are read.
const WINDOW: i64 = 8 * DAY;
const IDLE: i64 = 600_000; // a turn silent this long is not "working"

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prov {
    Claude,
    Codex,
    OpenCode,
    Muse,
}
impl Prov {
    pub fn id(self) -> &'static str {
        match self { Prov::Claude => "claude", Prov::Codex => "codex", Prov::OpenCode => "opencode", Prov::Muse => "muse" }
    }
    pub fn name(self) -> &'static str {
        match self { Prov::Claude => "Claude Code", Prov::Codex => "Codex", Prov::OpenCode => "OpenCode", Prov::Muse => "Muse" }
    }
}

struct Rec {
    ts: i64,
    /// The card it counts for: a provider id, or `claude:<folder>` for another Claude Code profile.
    card: String,
    model: String,
    project: String,
    inp: u64,
    out: u64,
    cr: u64,
    cw: u64,
    cw1h: u64,
}

#[derive(Default)]
struct FileSt {
    /// Bytes read; for a database, the last row read.
    off: u64,
    card: String,
    session: String,
    cwd: String,
    model: String,
    last_total: u64,
}

#[derive(Default, Clone)]
struct Sess {
    card: String,
    project: String,
    model: String,
    start: Option<i64>,
    last: i64,
}

pub struct Ended {
    pub prov: Prov,
    pub project: String,
    pub duration_ms: i64,
    pub at: i64,
    pub session: String,
}

#[derive(Default)]
pub struct Store {
    files: HashMap<PathBuf, FileSt>,
    recs: HashMap<String, Rec>,
    sess: HashMap<(Prov, String), Sess>,
    /// Cards whose log folder exists: (id, name), in the order found.
    seen: Vec<(String, String)>,
}

/// A folder to read and watch, and the card its counts belong to.
#[derive(Clone)]
pub struct Root {
    pub prov: Prov,
    pub path: PathBuf,
    pub card: String,
    pub name: String,
}

/// One log (or database) to read: provider, card, path.
pub type Log = (Prov, String, PathBuf);

// ── time ─────────────────────────────────────────────────────────────────────

fn days_from_civil(mut y: i64, m: i64, d: i64) -> i64 {
    if m <= 2 {
        y -= 1;
    }
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// "2026-10-10T12:53:13.152Z" -> epoch ms.
pub fn parse_iso(s: &str) -> Option<i64> {
    let n = |a: usize, l: usize| s.get(a..a + l)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 2)?, n(8, 2)?, n(11, 2)?, n(14, 2)?, n(17, 2)?);
    let ms = if s.as_bytes().get(19) == Some(&b'.') { n(20, 3).unwrap_or(0) } else { 0 };
    Some(((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Minutes to add to local time to get UTC (Windows `Bias`, daylight included).
pub fn bias_ms() -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz = TIME_ZONE_INFORMATION::default();
    let r = unsafe { GetTimeZoneInformation(&mut tz) };
    (tz.Bias + if r == 2 { tz.DaylightBias } else { tz.StandardBias }) as i64 * 60_000
}

/// "claude-opus-5-5-20260101" -> "Opus 5.5"; other ids as they are.
fn display(model: &str) -> String {
    let Some(rest) = model.strip_prefix("claude-") else { return model.to_string() };
    let mut parts = rest.split('-').filter(|p| !(p.len() >= 8 && p.chars().all(|c| c.is_ascii_digit())));
    let fam = parts.next().unwrap_or("");
    let ver: Vec<&str> = parts.collect();
    let fam = fam[..1].to_uppercase() + &fam[1..];
    if ver.is_empty() { fam } else { format!("{fam} {}", ver.join(".")) }
}

fn project_of(cwd: &str) -> String {
    let c = cwd.replace('\\', "/");
    let c = c.split("/.claude/worktrees/").next().unwrap_or(&c).trim_end_matches('/');
    match c.rsplit('/').next() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => "Unknown".into(),
    }
}

// ── roots ────────────────────────────────────────────────────────────────────

fn root(prov: Prov, path: PathBuf, card: &str, name: &str) -> Root {
    Root { prov, path, card: card.into(), name: name.into() }
}

/// Every folder with logs: Claude Code's main config folder and each other profile (`CLAUDE_CONFIG_DIR`
/// homes kept as `.claude-<name>`, or `.config/claude`), Codex, OpenCode and Muse.
pub fn roots() -> Vec<Root> {
    let home = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default());
    let local = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default());
    let main = std::env::var("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|_| home.join(".claude"));
    let mut out = vec![root(Prov::Claude, main.join("projects"), "claude", "Claude Code")];
    let mut homes = vec![(home.join(".config").join("claude"), "config".to_string())];
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = n.strip_prefix(".claude-").filter(|r| !r.is_empty()) {
                homes.push((e.path(), rest.to_string()));
            }
        }
    }
    if main != home.join(".claude") {
        homes.push((home.join(".claude"), "default".into()));
    }
    homes.sort_by(|a, b| a.1.cmp(&b.1));
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    for (dir, name) in homes {
        let projects = dir.join("projects");
        // a profile that links to a folder already listed would be counted twice
        if projects.is_dir() && !out.iter().any(|r| canon(&r.path) == canon(&projects)) {
            let label = name[..1].to_uppercase() + &name[1..];
            out.push(root(Prov::Claude, projects, &format!("claude:{name}"), &format!("Claude Code · {label}")));
        }
    }
    let codex = std::env::var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".codex"));
    out.push(root(Prov::Codex, codex.join("sessions"), "codex", "Codex"));
    out.push(root(Prov::Codex, codex.join("archived_sessions"), "codex", "Codex"));
    for base in [home.join(".local").join("share"), local] {
        out.push(root(Prov::OpenCode, base.join("opencode"), "opencode", "OpenCode"));
        out.push(root(Prov::Muse, base.join("muse").join("sessions"), "muse", "Muse"));
    }
    out.into_iter().filter(|r| r.path.is_dir()).collect()
}

fn walk(dir: &Path, cutoff_s: u64, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            walk(&p, cutoff_s, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") && md.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).is_some_and(|d| d.as_secs() >= cutoff_s) {
            out.push(p);
        }
    }
}

fn has(line: &[u8], pat: &str) -> bool {
    let p = pat.as_bytes();
    let mut i = 0;
    while let Some(k) = line[i..].iter().position(|&b| b == p[0]) {
        i += k;
        if line[i..].starts_with(p) {
            return true;
        }
        i += 1;
    }
    false
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

// ── store ────────────────────────────────────────────────────────────────────

/// Every log touched in the last 8 days (and the OpenCode database). Touches no counters, so it runs
/// outside the store's lock. A card in `hidden` is not read.
pub fn logs(roots: &[Root], now: i64, hidden: &[String]) -> Vec<Log> {
    let mut out = vec![];
    for r in roots.iter().filter(|r| !hidden.contains(&r.card)) {
        if r.prov == Prov::OpenCode {
            let db = r.path.join("opencode.db");
            if db.is_file() {
                out.push((r.prov, r.card.clone(), db));
            }
            continue;
        }
        let mut files = vec![];
        walk(&r.path, ((now - WINDOW) / 1000) as u64, &mut files);
        if r.prov == Prov::Muse {
            files.retain(|f| f.file_name().is_some_and(|n| n == "session.jsonl") && !f.strip_prefix(&r.path).is_ok_and(|p| p.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))));
        }
        out.extend(files.into_iter().map(|f| (r.prov, r.card.clone(), f)));
    }
    out
}

impl Store {
    /// Reads what is new in every log touched in the last 8 days. Returns the turns that ended while
    /// reading when `report` is set (never on the first full read).
    pub fn scan(&mut self, roots: &[Root], now: i64, report: bool) -> Vec<Ended> {
        let mut ended = vec![];
        self.found(roots);
        for (prov, card, f) in logs(roots, now, &[]) {
            self.read(prov, &card, &f, now, report, &mut ended);
        }
        ended
    }

    /// A card whose log folder exists is shown, with or without recent logs.
    pub fn found(&mut self, roots: &[Root]) {
        for r in roots {
            if !self.seen.iter().any(|(id, _)| *id == r.card) {
                self.seen.push((r.card.clone(), r.name.clone()));
            }
        }
    }

    /// What is new in one log. The unit of work the feature holds its lock for.
    pub fn read(&mut self, prov: Prov, card: &str, path: &Path, now: i64, report: bool, ended: &mut Vec<Ended>) {
        let cutoff = now - WINDOW;
        if prov == Prov::OpenCode {
            return self.opencode(card, path, cutoff);
        }
        let Ok(file) = File::open(path) else { return };
        let len = file.metadata().map_or(0, |m| m.len());
        let mut st = self.files.remove(path).unwrap_or_default();
        if len < st.off {
            st = FileSt::default();
        }
        st.card = card.to_string();
        if len > st.off {
            let mut rd = BufReader::with_capacity(1 << 20, file);
            if rd.seek(SeekFrom::Start(st.off)).is_ok() {
                let mut buf = vec![];
                loop {
                    buf.clear();
                    match rd.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if buf.last() != Some(&b'\n') {
                                break; // partial line: wait for the rest
                            }
                            st.off += n as u64;
                            let line = buf.trim_ascii_end(); // drops CRLF too
                            if line.is_empty() {
                                continue;
                            }
                            match prov {
                                Prov::Claude => self.claude(line, &mut st, cutoff, report, ended),
                                Prov::Codex => self.codex(line, path, &mut st, cutoff, report, ended),
                                Prov::Muse => self.muse(line, path, &mut st, cutoff, report, ended),
                                Prov::OpenCode => {}
                            }
                        }
                    }
                }
            }
        }
        self.files.insert(path.to_path_buf(), st);
    }

    fn end_turn(&mut self, prov: Prov, session: &str, ts: i64, notify: bool, report: bool, dur: Option<i64>, ended: &mut Vec<Ended>) {
        if let Some(s) = self.sess.get_mut(&(prov, session.to_string())) {
            if let Some(start) = s.start.take() {
                if notify && report {
                    ended.push(Ended { prov, project: s.project.clone(), duration_ms: dur.unwrap_or(ts - start), at: ts, session: session.to_string() });
                }
            }
        }
    }

    fn claude(&mut self, line: &[u8], st: &mut FileSt, cutoff: i64, report: bool, ended: &mut Vec<Ended>) {
        let assistant = has(line, "\"type\":\"assistant\"");
        let user = !assistant && has(line, "\"type\":\"user\"") && !has(line, "\"type\":\"tool_result\"");
        if !assistant && !user {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
        let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) else { return };
        if ts < cutoff {
            return;
        }
        let side = v["isSidechain"].as_bool() == Some(true);
        let session = v["sessionId"].as_str().unwrap_or("").to_string();
        if let Some(c) = v["cwd"].as_str() {
            if st.cwd != c {
                st.cwd = c.to_string();
            }
        }
        let project = project_of(&st.cwd);
        let msg = &v["message"];
        if assistant {
            let model = msg["model"].as_str().unwrap_or("");
            let us = &msg["usage"];
            if us.is_object() && !model.starts_with('<') && !model.is_empty() {
                let key = format!("c:{}:{}", msg["id"].as_str().unwrap_or(""), v["requestId"].as_str().unwrap_or(""));
                let cw = u(&us["cache_creation_input_tokens"]);
                self.recs.insert(key, Rec {
                    ts, card: st.card.clone(), model: model.to_string(), project: project.clone(),
                    inp: u(&us["input_tokens"]), out: u(&us["output_tokens"]), cr: u(&us["cache_read_input_tokens"]),
                    cw, cw1h: u(&us["cache_creation"]["ephemeral_1h_input_tokens"]).min(cw),
                });
            }
            if side {
                return;
            }
            let s = self.sess.entry((Prov::Claude, session.clone())).or_default();
            s.card = st.card.clone();
            s.project = project;
            s.model = model.to_string();
            s.last = ts;
            if s.start.is_none() {
                s.start = Some(ts);
            }
            if matches!(msg["stop_reason"].as_str(), Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal")) {
                let ok = v["isApiErrorMessage"].as_bool() != Some(true);
                self.end_turn(Prov::Claude, &session, ts, ok, report, None, ended);
            }
        } else if !side && v["isMeta"].as_bool() != Some(true) {
            let c = &msg["content"];
            let text = c.as_str().or_else(|| c.get(0).and_then(|b| b["text"].as_str())).unwrap_or("");
            let silent = text.starts_with("[Request interrupted by user") || text.starts_with("<local-command-std");
            let real = c.is_string() || c.get(0).is_some_and(|b| b["type"] == "text");
            if silent {
                self.end_turn(Prov::Claude, &session, ts, false, report, None, ended);
            } else if real {
                let s = self.sess.entry((Prov::Claude, session)).or_default();
                s.card = st.card.clone();
                s.project = project;
                s.start = Some(ts);
                s.last = ts;
            }
        }
    }

    fn codex(&mut self, line: &[u8], path: &Path, st: &mut FileSt, cutoff: i64, report: bool, ended: &mut Vec<Ended>) {
        let hit = ["token_count", "task_started", "task_complete", "turn_aborted", "session_meta", "turn_context"].iter().any(|k| has(line, k));
        if !hit {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
        let p = &v["payload"];
        let ts = v["timestamp"].as_str().and_then(parse_iso).unwrap_or(0);
        match v["type"].as_str() {
            Some("session_meta") => {
                st.session = p["id"].as_str().unwrap_or("").to_string();
                if let Some(c) = p["cwd"].as_str() {
                    st.cwd = c.to_string();
                }
            }
            Some("turn_context") => {
                if let Some(m) = p["model"].as_str() {
                    st.model = m.to_string();
                }
                if let Some(c) = p["cwd"].as_str() {
                    st.cwd = c.to_string();
                }
            }
            Some("event_msg") => {
                if st.session.is_empty() {
                    st.session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                }
                let side = path.file_name().is_some_and(|n| n.to_string_lossy().contains('_'));
                let project = project_of(&st.cwd);
                match p["type"].as_str() {
                    Some("token_count") if ts >= cutoff => {
                        let info = &p["info"];
                        let total = u(&info["total_token_usage"]["total_tokens"]);
                        if total != 0 && total != st.last_total {
                            st.last_total = total;
                            let l = &info["last_token_usage"];
                            let cached = u(&l["cached_input_tokens"]);
                            self.recs.insert(format!("x:{}:{}", path.display(), total), Rec {
                                ts, card: st.card.clone(), model: st.model.clone(), project,
                                inp: u(&l["input_tokens"]).saturating_sub(cached), out: u(&l["output_tokens"]), cr: cached, cw: 0, cw1h: 0,
                            });
                        }
                    }
                    Some("task_started") if !side => {
                        let s = self.sess.entry((Prov::Codex, st.session.clone())).or_default();
                        s.card = st.card.clone();
                        s.project = project;
                        s.model = st.model.clone();
                        s.start = Some(ts);
                        s.last = ts;
                    }
                    Some("task_complete") if !side => {
                        let d = p["duration_ms"].as_i64();
                        self.end_turn(Prov::Codex, &st.session.clone(), ts, true, report, d, ended);
                    }
                    Some("turn_aborted") if !side => self.end_turn(Prov::Codex, &st.session.clone(), ts, false, report, None, ended),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// OpenCode keeps its sessions in one SQLite file (version 1: `message`, version 2: `session_message`).
    /// Opened read-only; only the token counts, the model and the folder are selected, never message text.
    fn opencode(&mut self, card: &str, path: &Path, cutoff: i64) {
        use rusqlite::{Connection, OpenFlags};
        let Ok(db) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) else { return };
        let _ = db.busy_timeout(std::time::Duration::from_secs(2));
        let has = |t: &str| db.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [t], |_| Ok(())).is_ok();
        let (msgs, sess, role) = if has("session_v2") { ("session_message", "session_v2", "m.type") } else { ("message", "session", "json_extract(m.data, '$.role')") };
        if !has(msgs) || !has(sess) {
            return;
        }
        let mut st = self.files.remove(path).unwrap_or_default();
        st.card = card.to_string();
        // A revert deletes the newest rows and SQLite hands their ids to the next ones: start over (the ids keep it idempotent).
        let top = db.query_row(&format!("SELECT coalesce(max(rowid), 0) FROM {msgs}"), [], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64;
        if top < st.off {
            st.off = 0;
        }
        let sql = format!(
            "SELECT m.rowid, m.id, m.time_created, s.directory, {role}, json_extract(m.data, '$.modelID'), json_extract(m.data, '$.model_id'), json_extract(m.data, '$.model'), \
             json_extract(m.data, '$.tokens.input'), json_extract(m.data, '$.tokens.output'), json_extract(m.data, '$.tokens.reasoning'), \
             json_extract(m.data, '$.tokens.cache.read'), json_extract(m.data, '$.tokens.cache.write') \
             FROM {msgs} m JOIN {sess} s ON m.session_id = s.id WHERE m.rowid > ?1 AND m.time_created >= ?2 ORDER BY m.rowid");
        if let Ok(mut q) = db.prepare(&sql) {
            let n = |r: &rusqlite::Row, i: usize| r.get::<_, Option<i64>>(i).ok().flatten().unwrap_or(0).max(0) as u64;
            let rows = q.query_map([st.off as i64, cutoff], |r| {
                let model = (5..8).find_map(|i| r.get::<_, Option<String>>(i).ok().flatten().filter(|m| !m.is_empty())).unwrap_or_default();
                // version 2 may keep the model as an object
                let model = match serde_json::from_str::<Value>(&model) {
                    Ok(v) if v.is_object() => v["id"].as_str().or(v["modelID"].as_str()).unwrap_or("").to_string(),
                    _ => model,
                };
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<String>>(3)?.unwrap_or_default(), r.get::<_, Option<String>>(4)?.unwrap_or_default(), model, [n(r, 8), n(r, 9), n(r, 10), n(r, 11), n(r, 12)]))
            });
            for (rowid, id, ts, dir, role, model, t) in rows.into_iter().flatten().flatten() {
                st.off = st.off.max(rowid as u64);
                if role != "assistant" || t.iter().sum::<u64>() == 0 {
                    continue;
                }
                self.recs.insert(format!("o:{id}"), Rec { ts, card: card.to_string(), model, project: project_of(&dir), inp: t[0], out: t[1] + t[2], cr: t[3], cw: t[4], cw1h: 0 });
            }
        }
        self.files.insert(path.to_path_buf(), st);
    }

    /// Muse session logs: one event per line; counters, model, folder and run boundaries only.
    fn muse(&mut self, line: &[u8], path: &Path, st: &mut FileSt, cutoff: i64, report: bool, ended: &mut Vec<Ended>) {
        let hit = ["\"payload\":{\"kind\":\"run", "\"payload\":{\"kind\":\"metadata\"", "\"payload\":{\"kind\":\"approval\""].iter().any(|k| has(line, k));
        if !hit {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
        let ts = v["recorded_at"].as_i64().unwrap_or(0) / 1000; // microseconds
        let p = &v["payload"];
        if st.session.is_empty() {
            st.session = v["stream"]["id"].as_str().map(str::to_string).unwrap_or_else(|| path.display().to_string());
        }
        let e = &p["event"];
        let side = path.components().any(|c| c.as_os_str() == "subagent");
        let call = |this: &mut Store, st: &FileSt, usage: &Value, model: &str| {
            let inp = u(&usage["input_tokens"]);
            let cached = ["cache_read_tokens", "cached_tokens", "cached_input_tokens"].iter().map(|k| u(&usage[*k])).max().unwrap_or(0).min(inp);
            let cw = u(&usage["cache_write_tokens"]).min(inp - cached);
            if ts >= cutoff {
                let key = format!("m:{}", v["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("{}:{ts}", st.session)));
                this.recs.insert(key, Rec { ts, card: st.card.clone(), model: model.to_string(), project: project_of(&st.cwd), inp: inp - cached - cw, out: u(&usage["output_tokens"]) + u(&usage["reasoning_tokens"]), cr: cached, cw, cw1h: 0 });
            }
        };
        match p["kind"].as_str() {
            Some("metadata") => {
                if let Some(c) = p["record"]["workspace_root"].as_str() {
                    st.cwd = c.to_string();
                }
                if let Some(m) = p["record"]["model_id"].as_str() {
                    st.model = m.to_string();
                }
            }
            Some("run_model") => {
                if let Some(m) = p["record"]["model_id"].as_str() {
                    st.model = m.to_string();
                }
            }
            Some("run") => match e["kind"].as_str() {
                Some("started") if !side => {
                    let s = self.sess.entry((Prov::Muse, st.session.clone())).or_default();
                    s.card = st.card.clone();
                    s.project = project_of(&st.cwd);
                    s.model = st.model.clone();
                    s.start.get_or_insert(ts);
                    s.last = ts;
                }
                Some("model_completed") => {
                    if let Some(m) = e["model"].as_str().filter(|m| !m.is_empty()) {
                        st.model = m.to_string();
                    }
                    call(self, st, &e["usage"], &st.model.clone());
                    if let Some(s) = self.sess.get_mut(&(Prov::Muse, st.session.clone())).filter(|_| !side) {
                        s.last = ts;
                    }
                }
                Some("terminal") if !side => {
                    let ok = e["terminal"].as_str() == Some("completed");
                    let d = e["turn_duration_ms"].as_i64();
                    self.end_turn(Prov::Muse, &st.session.clone(), ts, ok, report, d, ended);
                }
                _ => {}
            },
            Some("approval") if e["kind"].as_str() == Some("automated_review_completed") => {
                let m = e["model"]["model_id"].as_str().filter(|m| !m.is_empty()).unwrap_or(&st.model).to_string();
                call(self, st, &e["usage"], &m);
            }
            _ => {}
        }
    }

    /// Everything the page draws: per provider today, this week (7 local days) and the open turns.
    pub fn summary(&self, now: i64) -> Value {
        let bias = bias_ms();
        let today = (now - bias).div_euclid(DAY) * DAY + bias;
        let week = today - 6 * DAY;
        let mut provs = vec![];
        for (id, name) in &self.seen {
            let period = |from: i64| {
                let (mut t, mut n) = ([0u64; 4], 0u64);
                let mut models: HashMap<String, u64> = HashMap::new();
                let mut projects: HashMap<String, u64> = HashMap::new();
                let mut days = [0u64; 7];
                for r in self.recs.values().filter(|r| r.card == *id && r.ts >= week) {
                    let tok = r.inp + r.out + r.cr + r.cw;
                    days[(((r.ts - week) / DAY) as usize).min(6)] += tok;
                    if r.ts < from {
                        continue;
                    }
                    t[0] += r.inp; t[1] += r.out; t[2] += r.cr; t[3] += r.cw;
                    n += 1;
                    for (m, k) in [(&mut models, display(&r.model)), (&mut projects, r.project.clone())] {
                        *m.entry(k).or_default() += tok;
                    }
                }
                let rank = |m: HashMap<String, u64>| {
                    let mut v: Vec<_> = m.into_iter().collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1));
                    v.into_iter().map(|(name, tokens)| json!({ "name": name, "tokens": tokens })).collect::<Vec<_>>()
                };
                json!({
                    "input": t[0], "output": t[1], "cacheRead": t[2], "cacheWrite": t[3], "total": t.iter().sum::<u64>(),
                    "requests": n,
                    "models": rank(models), "projects": rank(projects), "days": days,
                })
            };
            let live: Vec<Value> = self.sess.iter()
                .filter(|((_, _), s)| s.card == *id && s.start.is_some() && now - s.last < IDLE)
                .map(|((_, id), s)| json!({ "session": id, "project": s.project, "model": display(&s.model), "startedAt": s.start, "last": s.last }))
                .collect();
            provs.push(json!({ "id": id, "name": name, "today": period(today), "week": period(week), "live": live }));
        }
        json!({ "providers": provs, "at": now })
    }
}
