// Cleaner — a Lintel feature: a scan of the places that fill a disk for nothing,
// a review list, and a clean of what the user ticked; an uninstaller that runs
// a program's own uninstaller and lists what it left; the startup list, to read.
//
// Nothing runs before `start`, and after it only one thing may: the timer of
// the scan that runs by itself, when the user picked a schedule. That scan
// reads sizes and tells; it removes nothing. A file leaves its place in one
// way only: the user saw its row with its size, ticked it and confirmed, and
// `svc_fs::recycle` moved it to the Recycle Bin after checking it again. The
// page names rows by the number the scan gave them, never by a path.
//
// Two more readers share the window: the disk analyzer (sizes as a tree; it
// can only show a node in Explorer) and the duplicate finder (its copies are
// rows of the same list, moved by the same `clean`, never the last copy).

pub mod clean;
pub mod disk;
pub mod dupes;
pub mod startup;
pub mod uninstall;

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clean::Row;
use notch_core::storage::{json_load, json_save_atomic};
use notch_core::{Ctx, Feature, Group, Manifest, Monitor, Use, WindowSpec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use svc_apps::InstalledApp;
use svc_fs::Refusal;

const ID: &str = "cleaner-uninstaller";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Cleaner",
    group: Group::Tools,
    default_on: true,
    uses: &[Use::FileDelete],
    shortcuts: &[],
    tray: &[],
    // screenshotDays: 0 turns that search off; schedule: "off" | "daily" | "weekly" (a scan that
    // only tells); notify: tell what that scan found; downloads: list old Telegram downloads, of
    // downloadsDays (1, 2, 7, 14, 30) and downloadsKinds (image, video, audio, document, archive, other).
    defaults: r#"{ "screenshotDays": 30, "schedule": "off", "notify": true, "downloads": false, "downloadsDays": 7, "downloadsKinds": ["image", "video", "audio"] }"#,
};

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How the last clean ended.
#[derive(Clone, Default, Serialize)]
struct Done {
    moved: usize,
    left: usize,
    bytes: u64,
    /// Why what was left was left, a word each (`svc_fs::Refusal::code`).
    why: Vec<&'static str>,
}

/// The uninstall the page follows.
#[derive(Clone, Serialize)]
struct Un {
    key: String,
    name: String,
    /// "running" | "waiting" | "gone" | "still" | "failed"
    phase: &'static str,
    /// Why it failed: "admin", or what Windows or winget said.
    message: String,
    /// winget's command for the same program, offered when its own uninstaller needs an administrator.
    winget: Option<String>,
    #[serde(skip)]
    app: InstalledApp,
}

#[derive(Default)]
struct St {
    /// "" | "scanning" | "cleaning" | "dupes"
    phase: &'static str,
    /// The target being read.
    scanning: &'static str,
    /// The last scan.
    rows: Vec<Row>,
    /// What the last uninstall left behind.
    left: Vec<Row>,
    /// The duplicate finder's groups, a row a copy.
    dupes: Vec<Vec<Row>>,
    dupes_at: Option<u64>,
    /// The last row number given: a row of an older list is never taken for a new one.
    ids: u32,
    done: Option<Done>,
    progress: (usize, usize),
    bin: Option<(u64, u64)>,
    scanned_at: Option<u64>,
    un: Option<Un>,
    /// The tab the window is asked to show.
    tab: String,
    /// Stops the scan or the clean that runs.
    cancel: Arc<AtomicBool>,
}

fn busy(st: &St) -> bool {
    !st.phase.is_empty() || st.un.as_ref().is_some_and(|un| matches!(un.phase, "running" | "waiting"))
}

/// What outlives the app: when the last scan ran, for the schedule.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    last_scan: Option<u64>,
}

/// The disk analyzer's last tree.
#[derive(Default)]
struct Disk {
    /// "" | "scanning"
    phase: &'static str,
    root: String,
    nodes: Vec<disk::Flat>,
    at: Option<u64>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    st: Mutex<St>,
    disk: Mutex<Disk>,
    /// The schedule's thread, by the frequency it was armed for; it ends when the sender goes.
    timer: Mutex<Option<(String, Sender<()>)>>,
    /// The user will not wait any longer for an uninstaller to be done.
    stop_wait: AtomicBool,
    /// The feature is off.
    off: AtomicBool,
}

#[derive(Default)]
pub struct Cleaner(Arc<Inner>);

fn snapshot(inner: &Inner) -> Value {
    let st = inner.st.lock().unwrap();
    let rows = |rows: &[Row]| -> Vec<Value> {
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.id, "kind": r.kind, "title": r.title, "name": r.name, "place": r.place.to_string_lossy(),
                    "bytes": r.bytes(), "count": r.items.len(), "checked": r.checked, "note": r.note, "modified": r.modified,
                })
            })
            .collect()
    };
    json!({
        "phase": if st.phase.is_empty() { "idle" } else { st.phase },
        "scanning": st.scanning,
        "rows": rows(&st.rows),
        "left": rows(&st.left),
        "dupes": st.dupes.iter().map(|g| rows(g)).collect::<Vec<_>>(),
        "dupesAt": st.dupes_at,
        "done": st.done,
        "progress": [st.progress.0, st.progress.1],
        "bin": st.bin.map(|(bytes, items)| [bytes, items]),
        "scannedAt": st.scanned_at,
        "un": st.un,
        "tab": st.tab,
    })
}

fn tell(cx: &Ctx, inner: &Inner) {
    cx.emit(ID, "state", snapshot(inner));
}

fn tell_disk(cx: &Ctx, inner: &Inner) {
    let d = inner.disk.lock().unwrap();
    let nodes: Vec<Value> = d
        .nodes
        .iter()
        .map(|n| json!({ "n": n.name, "d": n.dir, "b": n.bytes, "p": (n.parent != usize::MAX).then_some(n.parent), "k": n.kids, "path": n.path.to_string_lossy() }))
        .collect();
    cx.emit(ID, "disk", json!({ "phase": if d.phase.is_empty() { "idle" } else { d.phase }, "root": d.root, "nodes": nodes, "at": d.at }));
}

/// Reads the sizes under a drive or a folder. Nothing is removed.
fn disk_scan(cx: &Ctx, inner: &Arc<Inner>, root: &str) -> Result<(), String> {
    let path = PathBuf::from(root);
    if !path.is_absolute() || root.starts_with("\\\\") || !path.is_dir() {
        return Err("not a folder on this PC".into());
    }
    let cancel = {
        let mut d = inner.disk.lock().unwrap();
        if !d.phase.is_empty() || inner.off.load(SeqCst) {
            return Err("busy".into());
        }
        d.phase = "scanning";
        d.root = root.to_string();
        d.nodes.clear();
        d.cancel = Arc::new(AtomicBool::new(false));
        d.cancel.clone()
    };
    tell_disk(cx, inner);
    let (cx, inner) = (cx.clone(), inner.clone());
    std::thread::spawn(move || {
        let last = Cell::new(Instant::now());
        let nodes = disk::scan(&path, &cancel, &|dir| {
            if last.get().elapsed() >= Duration::from_millis(150) {
                last.set(Instant::now());
                cx.emit(ID, "diskProgress", dir.to_string_lossy());
            }
        });
        {
            let mut d = inner.disk.lock().unwrap();
            d.phase = "";
            d.at = Some(now());
            d.nodes = if cancel.load(SeqCst) { Vec::new() } else { nodes };
        }
        tell_disk(&cx, &inner);
    });
    Ok(())
}

/// Looks for copies of the same file in the user's own folders. Lists; moves nothing.
fn find_dupes(cx: &Ctx, inner: &Arc<Inner>) -> Result<(), String> {
    let cancel = {
        let mut st = inner.st.lock().unwrap();
        if busy(&st) || inner.off.load(SeqCst) {
            return Err("busy".into());
        }
        st.phase = "dupes";
        st.dupes.clear();
        st.done = None;
        st.cancel = Arc::new(AtomicBool::new(false));
        st.cancel.clone()
    };
    tell(cx, inner);
    let (cx, inner) = (cx.clone(), inner.clone());
    std::thread::spawn(move || {
        let last = Cell::new(Instant::now());
        let groups = dupes::find(&cancel, &|stage, n, of| {
            if last.get().elapsed() >= Duration::from_millis(150) {
                last.set(Instant::now());
                cx.emit(ID, "dupesProgress", json!([stage, n, of]));
            }
        });
        {
            let mut st = inner.st.lock().unwrap();
            let cancelled = cancel.load(SeqCst) || inner.off.load(SeqCst);
            for group in groups.into_iter().filter(|_| !cancelled) {
                let mut rows = Vec::new();
                for (item, bytes) in group {
                    st.ids += 1;
                    let place = item.path().parent().unwrap_or(item.root()).to_path_buf();
                    let modified = item.modified().and_then(|at| at.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs());
                    rows.push(Row { id: st.ids, kind: "duplicates", title: "Duplicate files", name: Some(item.name()), place, checked: false, note: "", modified, items: vec![(item, bytes)] });
                }
                st.dupes.push(rows);
            }
            st.phase = "";
            st.dupes_at = (!cancelled).then(now);
        }
        tell(&cx, &inner);
    });
    Ok(())
}

/// Explorer, with the file or folder selected in its parent.
fn select_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
}

/// What a clean leaves in a list of rows: the files that went are no longer there, and what stayed is shown again, unticked.
fn settle_rows(rows: &mut Vec<Row>, ids: &[u32], gone: &HashSet<PathBuf>, stayed: &HashSet<PathBuf>) {
    for row in rows.iter_mut().filter(|r| ids.contains(&r.id)) {
        row.items.retain(|(item, _)| !gone.contains(item.path()));
        if row.items.iter().any(|(item, _)| stayed.contains(item.path())) {
            row.checked = false;
            row.note = "left";
        }
    }
    rows.retain(|r| !r.items.is_empty());
}

/// Reads what every target has to offer. Nothing is removed. `automatic` is
/// the schedule's own pass, the only one that announces what it found.
fn scan(cx: &Ctx, inner: &Inner, automatic: bool) {
    let cancel = {
        let mut st = inner.st.lock().unwrap();
        if busy(&st) || inner.off.load(SeqCst) {
            return;
        }
        st.phase = "scanning";
        st.rows.clear();
        st.done = None;
        st.cancel = Arc::new(AtomicBool::new(false));
        st.cancel.clone()
    };
    let s = cx.settings(ID);
    let opts = clean::Opts {
        now: SystemTime::now(),
        screenshot_days: s["screenshotDays"].as_u64().unwrap_or(30).min(3650),
        downloads: (s["downloads"].as_bool() == Some(true)).then(|| (s["downloadsDays"].as_u64().unwrap_or(7), serde_json::from_value(s["downloadsKinds"].clone()).unwrap_or_default())),
        running: notch_core::win::process_table().into_values().map(|p| p.exe.to_lowercase()).collect(),
    };
    for target in clean::TARGETS {
        if cancel.load(SeqCst) || inner.off.load(SeqCst) {
            break;
        }
        inner.st.lock().unwrap().scanning = target.title;
        tell(cx, inner);
        let found = clean::scan(target, &opts, &cancel);
        let mut st = inner.st.lock().unwrap();
        for mut row in found {
            st.ids += 1;
            row.id = st.ids;
            st.rows.push(row);
        }
    }
    let bin = svc_fs::recycle_bin();
    let (bytes, count) = {
        let mut st = inner.st.lock().unwrap();
        st.phase = "";
        st.scanning = "";
        st.bin = bin;
        st.scanned_at = Some(now());
        let ticked = st.rows.iter().filter(|r| r.checked);
        (ticked.clone().map(Row::bytes).sum::<u64>(), ticked.count())
    };
    let _ = json_save_atomic(&cx.data_dir(ID).join("state.json"), &Saved { last_scan: Some(now()) });
    tell(cx, inner);
    if automatic && bytes > 0 && cx.settings(ID)["notify"].as_bool() != Some(false) && !cx.paused() {
        cx.emit(ID, "found", json!({ "bytes": bytes, "count": count }));
    }
}

/// Moves the files of the rows the user ticked to the Recycle Bin, one by one,
/// each checked again by `svc_fs::recycle`. Only rows of the lists now shown,
/// by their numbers.
fn clean(cx: &Ctx, inner: &Arc<Inner>, ids: &[u32]) -> Result<(), String> {
    let (work, cancel) = {
        let mut st = inner.st.lock().unwrap();
        if busy(&st) {
            return Err("busy".into());
        }
        // A group of identical files keeps at least one: the page cannot ask for all of them.
        if st.dupes.iter().any(|g| g.iter().all(|r| ids.contains(&r.id))) {
            return Err("keep one copy of every file".into());
        }
        let work: Vec<_> = st.rows.iter().chain(&st.left).chain(st.dupes.iter().flatten()).filter(|r| ids.contains(&r.id)).flat_map(|r| r.items.clone()).collect();
        if work.is_empty() {
            return Err("nothing of the list is ticked".into());
        }
        st.phase = "cleaning";
        st.progress = (0, work.len());
        st.done = None;
        st.cancel = Arc::new(AtomicBool::new(false));
        (work, st.cancel.clone())
    };
    tell(cx, inner);
    let (cx, inner, ids) = (cx.clone(), inner.clone(), ids.to_vec());
    std::thread::spawn(move || {
        let mut done = Done::default();
        let (mut gone, mut stayed): (HashSet<PathBuf>, HashSet<PathBuf>) = Default::default();
        let mut sent = Instant::now();
        for (i, (item, bytes)) in work.iter().enumerate() {
            if cancel.load(SeqCst) || inner.off.load(SeqCst) {
                break;
            }
            match svc_fs::recycle(item) {
                Ok(_) => {
                    done.moved += 1;
                    done.bytes += bytes;
                    gone.insert(item.path().to_path_buf());
                }
                // Gone since the scan: nothing to move, nothing left.
                Err(Refusal::Missing) => {
                    gone.insert(item.path().to_path_buf());
                }
                Err(why) => {
                    done.left += 1;
                    stayed.insert(item.path().to_path_buf());
                    if !done.why.contains(&why.code()) {
                        done.why.push(why.code());
                    }
                }
            }
            if sent.elapsed() >= Duration::from_millis(150) {
                sent = Instant::now();
                inner.st.lock().unwrap().progress = (i + 1, work.len());
                cx.emit(ID, "progress", [i + 1, work.len()]);
            }
        }
        notch_core::log::line(format!("cleaner: {} moved to the Recycle Bin, {} left in place", done.moved, done.left));
        let bin = svc_fs::recycle_bin();
        {
            let mut st = inner.st.lock().unwrap();
            let st = &mut *st;
            settle_rows(&mut st.rows, &ids, &gone, &stayed);
            settle_rows(&mut st.left, &ids, &gone, &stayed);
            for group in &mut st.dupes {
                settle_rows(group, &ids, &gone, &stayed);
            }
            st.dupes.retain(|g| g.len() > 1);
            st.phase = "";
            st.done = Some(done);
            st.bin = bin;
        }
        tell(&cx, &inner);
    });
    Ok(())
}

/// Whether Windows still lists the program, and what it left when it does not.
/// With `wait`, asks again every two seconds for five minutes: an uninstaller
/// often hands over to a copy of itself and ends long before the work is done.
fn settle(cx: &Ctx, inner: &Inner, app: &InstalledApp, wait: bool) -> bool {
    let until = Instant::now() + Duration::from_secs(300);
    let (gone, still) = loop {
        let still = svc_apps::installed();
        let gone = !still.iter().any(|a| a.key == app.key);
        if gone || !wait || Instant::now() >= until || inner.stop_wait.load(SeqCst) || inner.off.load(SeqCst) {
            break (gone, still);
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    let never = AtomicBool::new(false);
    let mut left = Vec::new();
    if gone {
        for item in uninstall::leftovers(app, &still, uninstall::PLACES) {
            let bytes = svc_fs::size(&item, &never);
            let place = item.path().parent().unwrap_or(item.root()).to_path_buf();
            // Never ticked for the user: these are found by name, and the tick is theirs.
            left.push(Row { id: 0, kind: "leftovers", title: "", name: Some(item.name()), place, checked: false, note: "", modified: None, items: vec![(item, bytes)] });
        }
    }
    {
        let mut st = inner.st.lock().unwrap();
        for row in &mut left {
            st.ids += 1;
            row.id = st.ids;
        }
        st.left = left;
        if let Some(un) = st.un.as_mut() {
            un.phase = if gone { "gone" } else { "still" };
        }
    }
    tell(cx, inner);
    gone
}

/// Starts the command the page showed for the program: its own registered
/// uninstaller, or winget's. Read again here from what Windows lists; the page
/// only names the program.
fn run_uninstaller(cx: &Ctx, inner: &Arc<Inner>, key: &str, winget: bool) -> Result<(), String> {
    let app = svc_apps::installed().into_iter().find(|a| a.key == key).ok_or("not an installed program")?;
    let offer = uninstall::winget_command(&app, svc_apps::winget::exe().as_deref());
    let cmd = if winget { offer.clone().ok_or("winget cannot remove this program")? } else { uninstall::own_command(&app).map_err(|why| why.code().to_string())? };
    {
        let mut st = inner.st.lock().unwrap();
        if busy(&st) {
            return Err("busy".into());
        }
        st.un = Some(Un { key: app.key.clone(), name: app.name.clone(), phase: "running", message: String::new(), winget: None, app: app.clone() });
        st.left.clear();
    }
    inner.stop_wait.store(false, SeqCst);
    tell(cx, inner);
    let (cx, inner) = (cx.clone(), inner.clone());
    std::thread::spawn(move || {
        notch_core::log::line(format!("cleaner: the uninstaller of {} was started{}", app.name, if winget { " through winget" } else { "" }));
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        // An uninstaller is never stopped half-way, and one that waits for its user may say nothing for as long as it likes.
        let never = AtomicBool::new(false);
        let failed = match svc_apps::run(&cmd.exe, &args, Duration::from_secs(4 * 3600), &never, |_| {}) {
            // 740: it wants administrator rights, and only Windows' own prompt gives them.
            Err(e) if e.contains("os error 740") => Some(("admin".to_string(), offer.filter(|_| !winget).map(|c| c.shown()))),
            Err(e) => Some((e, None)),
            Ok(out) => {
                if let Some(un) = inner.st.lock().unwrap().un.as_mut() {
                    un.phase = "waiting";
                }
                tell(&cx, &inner);
                // winget waits for the uninstaller itself: when it is back, the answer is known.
                let gone = settle(&cx, &inner, &app, !winget);
                let said = out.text.lines().map(str::trim).rfind(|line| !line.is_empty()).unwrap_or_default().chars().take(300).collect::<String>();
                (!gone && winget && out.code != Some(0)).then_some((said, None))
            }
        };
        if let Some((message, offer)) = failed {
            if let Some(un) = inner.st.lock().unwrap().un.as_mut() {
                (un.phase, un.message, un.winget) = ("failed", message, offer);
            }
            tell(&cx, &inner);
        }
    });
    Ok(())
}

impl Cleaner {
    /// The schedule: one thread that sleeps until the next scan is due, and
    /// none at all while the schedule is off.
    fn arm(&self, cx: &Ctx) {
        let frequency = cx.settings(ID)["schedule"].as_str().unwrap_or("off").to_string();
        let mut timer = self.0.timer.lock().unwrap();
        if timer.as_ref().is_some_and(|(armed, _)| *armed == frequency) {
            return;
        }
        *timer = None;
        let every: u64 = match frequency.as_str() {
            "daily" => 86_400,
            "weekly" => 7 * 86_400,
            _ => return,
        };
        let (tx, rx) = channel::<()>();
        *timer = Some((frequency, tx));
        let (cx, inner) = (cx.clone(), self.0.clone());
        std::thread::spawn(move || {
            let path = cx.data_dir(ID).join("state.json");
            // Never ran: the count starts now, without a surprise scan on the day it is switched on.
            if json_load::<Saved>(&path).last_scan.is_none() {
                let _ = json_save_atomic(&path, &Saved { last_scan: Some(now()) });
            }
            let due_in = || (json_load::<Saved>(&path).last_scan.unwrap_or(0) + every).saturating_sub(now());
            loop {
                if due_in() == 0 && !cx.paused() {
                    scan(&cx, &inner, true);
                }
                // ponytail: counts from the last scan and wakes once an hour to read the clock again,
                // which keeps a day right across sleep; a chosen hour of the day (upstream's
                // CleanerSchedule.nextFireDate) when someone asks for one.
                // Still due: paused, or busy with something else. Asked again in ten minutes.
                let nap = match due_in() {
                    0 => 600,
                    wait => wait.min(3600),
                };
                if rx.recv_timeout(Duration::from_secs(nap)) != Err(RecvTimeoutError::Timeout) {
                    return;
                }
            }
        });
    }
}

impl Feature for Cleaner {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        self.0.off.store(false, SeqCst);
        self.arm(cx);
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        self.0.timer.lock().unwrap().take();
        self.0.off.store(true, SeqCst);
        self.0.stop_wait.store(true, SeqCst);
        // A scan or a clean stops at its next file. An uninstaller that is open is the user's to finish.
        self.0.st.lock().unwrap().cancel.store(true, SeqCst);
        self.0.disk.lock().unwrap().cancel.store(true, SeqCst);
    }

    fn settings_changed(&self, cx: &Ctx, _settings: &Value) {
        self.arm(cx);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let inner = &self.0;
        let row_place = |id: u64| {
            let st = inner.st.lock().unwrap();
            st.rows.iter().chain(&st.left).chain(st.dupes.iter().flatten()).find(|r| u64::from(r.id) == id).map(|r| r.place.clone())
        };
        match method {
            "state" => return Ok(snapshot(inner)),
            "scan" => {
                let (cx, inner) = (cx.clone(), inner.clone());
                std::thread::spawn(move || scan(&cx, &inner, false));
            }
            "cancel" => inner.st.lock().unwrap().cancel.store(true, SeqCst),
            "dupes" => find_dupes(cx, inner)?,

            // The disk analyzer: the fixed drives, a scan of one, and a node shown in Explorer (by its number).
            "diskDrives" => return Ok(json!({ "drives": disk::drives(), "home": notch_core::win::home_dir().to_string_lossy() })),
            "disk" => {
                tell_disk(cx, inner);
            }
            "diskScan" => disk_scan(cx, inner, args["root"].as_str().unwrap_or_default())?,
            "diskCancel" => inner.disk.lock().unwrap().cancel.store(true, SeqCst),
            "diskReveal" => {
                let path = args["id"].as_u64().and_then(|id| inner.disk.lock().unwrap().nodes.get(id as usize).map(|n| n.path.clone()));
                if let Some(path) = path {
                    select_in_explorer(&path);
                }
            }
            "clean" => {
                let ids: Vec<u32> = serde_json::from_value(args["ids"].clone()).unwrap_or_default();
                clean(cx, inner, &ids)?;
            }
            "dismiss" => {
                inner.st.lock().unwrap().done = None;
                tell(cx, inner);
            }

            // The programs Windows lists, each with the command that would remove it.
            "apps" => {
                let winget = svc_apps::winget::exe();
                let rows: Vec<Value> = svc_apps::installed()
                    .iter()
                    .map(|a| {
                        let own = uninstall::own_command(a);
                        json!({
                            "key": a.key, "name": a.name, "version": a.version, "publisher": a.publisher, "store": a.store,
                            "cmd": own.as_ref().ok().map(|c| c.shown()),
                            "winget": uninstall::winget_command(a, winget.as_deref()).map(|c| c.shown()),
                            "why": own.err().map(|why| why.code()),
                        })
                    })
                    .collect();
                return Ok(json!(rows));
            }
            "uninstall" => run_uninstaller(cx, inner, args["key"].as_str().unwrap_or_default(), args["winget"].as_bool() == Some(true))?,
            "stopWaiting" => inner.stop_wait.store(true, SeqCst),
            "recheck" => {
                let app = inner.st.lock().unwrap().un.as_ref().filter(|un| un.phase == "still").map(|un| un.app.clone());
                if let Some(app) = app {
                    let (cx, inner) = (cx.clone(), inner.clone());
                    std::thread::spawn(move || settle(&cx, &inner, &app, false));
                }
            }
            "forget" => {
                let mut st = inner.st.lock().unwrap();
                if !busy(&st) {
                    st.un = None;
                    st.left.clear();
                }
                drop(st);
                tell(cx, inner);
            }

            "startup" => return Ok(json!(startup::list())),

            // Explorer and Settings, on fixed places or on the folder of a row of the list.
            "reveal" => {
                if let Some(place) = args["id"].as_u64().and_then(row_place) {
                    notch_core::win::reveal_folder(&place.to_string_lossy());
                }
            }
            "openBin" => notch_core::win::reveal_folder("shell:RecycleBinFolder"),
            "openStartup" => notch_core::win::open_url("ms-settings:startupapps"),
            "openApps" => notch_core::win::open_url("ms-settings:appsfeatures"),

            "open" => {
                let tab = args["tab"].as_str().unwrap_or_default().to_string();
                inner.st.lock().unwrap().tab = tab.clone();
                cx.window().open(WindowSpec { label: ID.into(), feature: ID.into(), page: "main".into(), w: 760.0, h: 560.0, frameless: true, monitor: Monitor::Island, ..Default::default() })?;
                cx.emit(ID, "tab", tab);
            }
            "close" => cx.window().close(ID),
            _ => return Err("unknown method".into()),
        }
        Ok(json!({}))
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/cleaner-uninstaller/strings.json")
    }
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(Cleaner::default())
}
