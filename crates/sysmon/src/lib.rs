// System monitor — a Lintel feature. The contract is docs/ARCHITECTURE.md §2:
// nothing runs before `start`, everything is gone after `stop`, and every
// command of the feature arrives through `call`.
//
// Over crates/svc-sysmon. The sampler runs for two reasons only, each a guard
// held here: the System section is on screen (`watch`, every card's metric),
// or an alert is switched on in the settings (just the metrics it judges).
// With neither, nothing is read. The process list, the ports, the devices and
// the addresses are read when the page asks.
//
// Calls: `watch { on, detail? }` → state, `procs`, `kill { targets: [{ pid, started }], ask }`,
// `ports`, `openPort { port, address }`, `devices`, `addresses`, `eject { letter }`,
// `speedStart`, `speedCancel`, `window { tab }`, `tab`.
// Events: `tick { snap, procs? }`, `alert { kind, … }`, `speed { phase, ms, down, up }`,
// `devices [name]`, `tab` name.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notch_core::{Ctx, Feature, Group, Manifest, Use, WindowSpec};
use serde::Serialize;
use serde_json::{json, Value};
use svc_sysmon::procs::{self, Proc};
use svc_sysmon::{Metric, Snapshot, SustainedGate};

pub mod speed;

const ID: &str = "sysmon";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "System monitor",
    group: Group::System,
    default_on: true,
    // The speed test, on a click.
    uses: &[Use::Network],
    shortcuts: &[],
    tray: &[],
    // alert*: all off; a threshold is a percentage. groupProcesses: the process window's own.
    defaults: r#"{ "alertCpu": false, "alertCpuPercent": 90, "alertMemory": false,
        "alertDisk": false, "alertDiskFreePercent": 10, "alertCooldownMinutes": 15, "groupProcesses": true }"#,
};

/// Every card of the section.
const ALL: [Metric; 7] =
    [Metric::Cpu, Metric::Gpu, Metric::Memory, Metric::Disk, Metric::Network, Metric::Power, Metric::Temps];
/// Rows of a card's process list.
const TOP: usize = 8;

#[derive(Default)]
pub struct Sysmon {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    st: Mutex<St>,
}

#[derive(Default)]
struct St {
    on: bool,
    /// The readings reach us here for as long as the feature runs; it keeps nothing running.
    ticks: Option<svc_sysmon::Sub>,
    /// The section is on screen: every metric, and word of a device plugged in.
    watch: Option<(svc_sysmon::Guard, svc_sysevents::Sub)>,
    /// The mascot is on screen: CPU, memory and power are read for its mood.
    mood: Option<svc_sysmon::Guard>,
    /// With it: the hardware helper reads too, if one runs (the feature `hw-helper`).
    hw: Option<svc_hw::Sub>,
    /// The card whose page is open, when it lists processes: "cpu", "mem" or "gpu".
    detail: String,
    /// The metrics the alerts that are on need.
    armed: Option<svc_sysmon::Guard>,
    cfg: Cfg,
    cpu_gate: SustainedGate,
    /// When each kind of alert was last sent.
    sent: HashMap<&'static str, Instant>,
    speed: Option<tauri::async_runtime::JoinHandle<()>>,
    /// The tab the tools window opens on.
    tab: String,
}

#[derive(Default, Clone, PartialEq)]
struct Cfg {
    cpu: bool,
    cpu_percent: f32,
    memory: bool,
    disk: bool,
    disk_free_percent: f64,
    cooldown: Duration,
}

impl Cfg {
    fn of(v: &Value) -> Self {
        let number = |key: &str, default: f64, low: f64, high: f64| v[key].as_f64().unwrap_or(default).clamp(low, high);
        Self {
            cpu: v["alertCpu"].as_bool().unwrap_or(false),
            cpu_percent: number("alertCpuPercent", 90.0, 50.0, 100.0) as f32,
            memory: v["alertMemory"].as_bool().unwrap_or(false),
            disk: v["alertDisk"].as_bool().unwrap_or(false),
            disk_free_percent: number("alertDiskFreePercent", 10.0, 5.0, 30.0),
            cooldown: Duration::from_secs(60 * number("alertCooldownMinutes", 15.0, 2.0, 60.0) as u64),
        }
    }

    fn metrics(&self) -> Vec<Metric> {
        [(self.cpu, Metric::Cpu), (self.memory, Metric::Memory), (self.disk, Metric::Disk)]
            .into_iter()
            .filter_map(|(on, m)| on.then_some(m))
            .collect()
    }
}

/// The processes of one program as one row.
#[derive(Serialize)]
struct Row {
    name: String,
    count: usize,
    /// 0…1 of all cores; null while the first reading is still the baseline.
    cpu: Option<f32>,
    mem: u64,
    gpu: f32,
    protected: bool,
    targets: Vec<Value>,
}

/// `chrome.exe` and `PING.EXE` as "chrome" and "PING".
fn display_name(image: &str) -> String {
    let cut = image.len().saturating_sub(4);
    if image.is_char_boundary(cut) && image[cut..].eq_ignore_ascii_case(".exe") { image[..cut].to_owned() } else { image.to_owned() }
}

/// The heaviest programs by `key` ("cpu", "mem", "gpu").
fn top(key: &str, snap: &Snapshot) -> Vec<Row> {
    let gpu: HashMap<u32, f32> = snap.gpu_pids.iter().copied().collect();
    let mut rows: HashMap<String, Row> = HashMap::new();
    for p in procs::processes(Duration::from_millis(500)) {
        let Proc { pid, name, started, cpu, mem, protected, .. } = p;
        let row = rows.entry(name.to_lowercase()).or_insert_with(|| Row {
            name: display_name(&name),
            count: 0,
            cpu: None,
            mem: 0,
            gpu: 0.0,
            protected: false,
            targets: Vec::new(),
        });
        row.count += 1;
        row.cpu = match (row.cpu, cpu) {
            (a, None) => a,
            (a, Some(b)) => Some(a.unwrap_or(0.0) + b),
        };
        row.mem += mem;
        row.gpu = (row.gpu + gpu.get(&pid).copied().unwrap_or(0.0)).min(1.0);
        row.protected |= protected;
        row.targets.push(json!({ "pid": pid, "started": started.to_string() }));
    }
    let mut rows: Vec<Row> = rows.into_values().collect();
    let weight = |r: &Row| match key {
        "mem" => r.mem as f64,
        "gpu" => r.gpu as f64,
        _ => r.cpu.unwrap_or(0.0) as f64,
    };
    rows.sort_by(|a, b| weight(b).total_cmp(&weight(a)).then_with(|| a.name.cmp(&b.name)));
    rows.truncate(TOP);
    if key == "gpu" {
        rows.retain(|r| r.gpu > 0.0);
    }
    rows
}

impl Inner {
    /// On the sampler's thread, after every reading.
    fn tick(&self, cx: &Ctx, snap: &Snapshot) {
        let (watching, detail, mood) = {
            let st = self.st.lock().unwrap();
            (st.watch.is_some(), st.detail.clone(), st.mood.is_some())
        };
        if mood {
            let pct = snap.power.as_ref().and_then(|p| p.percent);
            cx.emit(ID, "mood", json!({
                "cpu": snap.cpu,
                "pressure": snap.mem.as_ref().map(|m| m.pressure),
                "battery": snap.power.as_ref().filter(|p| p.battery && !p.ac).and(pct),
                "temp": snap.temps.iter().map(|t| t.celsius).reduce(f32::max),
            }));
        }
        if watching {
            let mut said = json!({ "snap": snap });
            if !detail.is_empty() {
                said["procs"] = json!(top(&detail, snap));
            }
            // Rows of the hardware helper, while one runs and its reading is fresh.
            if let Some(hw) = svc_hw::summary() {
                said["hw"] = json!(hw);
                said["pins"] = cx.settings("hw-helper")["pins"].clone();
            }
            cx.emit(ID, "tick", said);
        }
        self.judge(cx, snap);
    }

    /// The alerts that are on, against this reading.
    fn judge(&self, cx: &Ctx, snap: &Snapshot) {
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        let cfg = &st.cfg;
        let mut due: Vec<(&'static str, Value)> = Vec::new();
        // A burst proves nothing: the load has to hold on separate readings.
        let load = snap.cpu.map(|c| c * 100.0);
        if cfg.cpu && st.cpu_gate.should_alert(load, cfg.cpu_percent, snap.at) {
            due.push(("cpu", json!({ "percent": load.unwrap_or(0.0).round() })));
        }
        if cfg.memory && snap.mem.as_ref().is_some_and(|m| m.pressure >= 2) {
            due.push(("memory", json!({})));
        }
        if let (true, Some(disk)) = (cfg.disk, &snap.disk) {
            // The first volume of some size that is nearly full.
            let low = |v: &&svc_sysmon::Volume| v.total >= 10 << 30 && (v.free as f64) < v.total as f64 * cfg.disk_free_percent / 100.0;
            if let Some(v) = disk.volumes.iter().find(low) {
                due.push(("disk", json!({ "letter": v.letter, "free": v.free })));
            }
        }
        for (kind, mut said) in due {
            if st.sent.get(kind).is_none_or(|at| at.elapsed() >= cfg.cooldown) {
                st.sent.insert(kind, Instant::now());
                said["kind"] = json!(kind);
                cx.emit(ID, "alert", said);
            }
        }
    }

    /// Holds exactly the metrics the alerts that are on need.
    fn arm(&self, cfg: Cfg) {
        let mut st = self.st.lock().unwrap();
        if !st.on {
            return;
        }
        let metrics = cfg.metrics();
        // The new guard before the old one goes: the sampler is not stopped in between.
        let armed = (!metrics.is_empty()).then(|| svc_sysmon::need(&metrics));
        st.armed = armed;
        if !cfg.cpu {
            st.cpu_gate = SustainedGate::default();
        }
        st.cfg = cfg;
    }

    /// The section came on screen, went, or opened another card.
    fn watch(self: &Arc<Self>, cx: &Ctx, on: bool, detail: &str) -> Value {
        let mut st = self.st.lock().unwrap();
        if !st.on || !on {
            let ended = (st.watch.take(), st.hw.take());
            st.detail.clear();
            drop(st);
            drop(ended);
            return Value::Null;
        }
        st.detail = if matches!(detail, "cpu" | "mem" | "gpu") { detail.to_owned() } else { String::new() };
        if st.watch.is_none() {
            let cx = cx.clone();
            let devices = svc_sysevents::on_device_change(move || {
                // Off the events thread: listing the devices takes a moment.
                let cx = cx.clone();
                std::thread::spawn(move || cx.emit(ID, "devices", svc_sysmon::extra::usb_devices()));
            });
            st.watch = Some((svc_sysmon::need(&ALL), devices));
            st.hw = Some(svc_hw::subscribe(svc_sysmon::TICK.as_millis() as u32, |_| {}));
        }
        let detail = st.detail.clone();
        drop(st);
        let snap = svc_sysmon::snapshot();
        json!({
            "classes": svc_sysmon::core_classes(),
            "host": speed::HOST,
            // The baseline of the processes' CPU shares, and their memory at once.
            "procs": if detail.is_empty() { Value::Null } else { json!(top(&detail, &snap)) },
        })
    }

    fn kill(&self, args: &Value) -> Value {
        let ask = args["ask"].as_bool().unwrap_or(false);
        let results: Vec<Value> = args["targets"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|t| Some((t["pid"].as_u64()? as u32, t["started"].as_str()?.parse::<u64>().ok()?)))
            .map(|(pid, started)| {
                let done = if ask { procs::ask_to_close(pid, started).map(|_| ()) } else { procs::kill(pid, started) };
                json!({ "pid": pid, "ok": done.is_ok(), "why": done.err() })
            })
            .collect();
        json!(results)
    }

    fn speed(self: &Arc<Self>, cx: &Ctx, start: bool) {
        let mut st = self.st.lock().unwrap();
        if let Some(running) = st.speed.take() {
            if start {
                st.speed = Some(running);
                return;
            }
            running.abort();
            cx.emit(ID, "speed", speed::Progress { phase: "idle", ..Default::default() });
        }
        if start && st.on {
            let (cx, inner) = (cx.clone(), self.clone());
            st.speed = Some(tauri::async_runtime::spawn(async move {
                speed::run(|p| cx.emit(ID, "speed", p.clone())).await;
                inner.st.lock().unwrap().speed = None;
            }));
        }
    }
}

/// `http://` and the place a listening socket answers at from this machine.
fn local_url(address: &str, port: u64) -> Option<String> {
    let port = u16::try_from(port).ok().filter(|p| *p > 0)?;
    let host = match address.parse::<IpAddr>().ok()? {
        // A wildcard is not a destination.
        IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
        IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_owned(),
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Some(format!("http://{host}:{port}"))
}

impl Feature for Sysmon {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let inner = &self.inner;
        {
            let mut st = inner.st.lock().unwrap();
            if st.on {
                return Ok(());
            }
            st.on = true;
            let (cx, inner) = (cx.clone(), inner.clone());
            st.ticks = Some(svc_sysmon::on_tick(move |snap| inner.tick(&cx, snap)));
        }
        inner.arm(Cfg::of(&cx.settings(ID)));
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        let ended = {
            let mut st = self.inner.st.lock().unwrap();
            if let Some(test) = &st.speed {
                test.abort();
            }
            std::mem::take(&mut *st)
        };
        // No lock is held while the guards go: a tick under way may want it.
        drop(ended);
    }

    fn settings_changed(&self, _cx: &Ctx, settings: &Value) {
        let cfg = Cfg::of(settings);
        if cfg != self.inner.st.lock().unwrap().cfg {
            self.inner.arm(cfg);
        }
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let inner = &self.inner;
        match method {
            "watch" => Ok(inner.watch(cx, args["on"].as_bool().unwrap_or(false), args["detail"].as_str().unwrap_or_default())),
            "mood" => {
                let on = args["on"].as_bool().unwrap_or(false);
                let mut st = inner.st.lock().unwrap();
                let guard = (on && st.on).then(|| svc_sysmon::need(&[Metric::Cpu, Metric::Memory, Metric::Power, Metric::Temps]));
                let old = std::mem::replace(&mut st.mood, guard);
                drop(st);
                drop(old);
                Ok(Value::Null)
            }
            "procs" => Ok(json!(procs::processes(Duration::from_secs(1)))),
            // Only ever from a click on a confirmation that names the process.
            "kill" => Ok(inner.kill(&args)),
            "ports" => procs::ports().map(|p| json!(p)),
            "openPort" => {
                let url = local_url(args["address"].as_str().unwrap_or_default(), args["port"].as_u64().unwrap_or(0));
                notch_core::win::open_url(&url.ok_or("not a local address")?);
                Ok(Value::Null)
            }
            "devices" => Ok(json!(svc_sysmon::extra::usb_devices())),
            "addresses" => Ok(json!(svc_sysmon::extra::addresses())),
            "eject" => {
                let done = svc_sysmon::extra::eject(args["letter"].as_str().unwrap_or_default());
                Ok(json!({ "ok": done.is_ok(), "why": done.err() }))
            }
            "speedStart" | "speedCancel" => {
                inner.speed(cx, method == "speedStart");
                Ok(Value::Null)
            }
            "window" => {
                let tab = args["tab"].as_str().unwrap_or("processes").to_owned();
                inner.st.lock().unwrap().tab = tab.clone();
                let spec = WindowSpec { label: "sysmon-tools".into(), feature: ID.into(), page: "tools".into(), w: 640.0, h: 560.0, ..Default::default() };
                let window = cx.window().open(spec)?;
                let _ = window.set_title(&notch_core::i18n::t("Processes and ports"));
                // A window that was open already goes to the tab asked for.
                cx.emit(ID, "tab", tab);
                Ok(Value::Null)
            }
            "tab" => Ok(json!(inner.st.lock().unwrap().tab)),
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/sysmon/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(Sysmon::default())
}
