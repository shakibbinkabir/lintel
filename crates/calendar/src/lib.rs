// Calendar — a Lintel feature: calendars from ICS links the user pastes, local reminders, the
// events of a week for the island, and the Cal.com bookings merged by the page.
// Nothing runs before `start`. The only thread is one that sleeps 30 minutes at a time on a condvar
// and fetches only when the section is on screen, an event is due soon, or the data is 3 hours old.

pub mod ics;

use jiff::{tz::TimeZone, Timestamp, Zoned};
use notch_core::{net, secrets, storage, Ctx, Feature, Group, Manifest, ShortcutDef, Use};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

static MANIFEST: Manifest = Manifest {
    id: "calendar",
    name: "Calendar",
    group: Group::Island,
    default_on: true,
    uses: &[Use::Network],
    shortcuts: &[ShortcutDef { id: "add-reminder", label: "Add a reminder", default: None }],
    tray: &[],
    defaults: r#"{ "countdown": true, "calcom": true, "description": true, "lead": 60, "alarm": 0 }"#,
};

const COLORS: [&str; 6] = ["#8ea2ff", "#34d399", "#f5a524", "#f4505e", "#22d3ee", "#c9956a"];
const TICK: Duration = Duration::from_secs(1800);

#[derive(Serialize, Deserialize, Clone)]
struct CalMeta {
    id: u32,
    name: String,
    color: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct Reminder {
    id: i64,
    text: String,
    done_at: Option<i64>,
}

#[derive(Default)]
struct Loaded {
    events: Vec<ics::Event>,
    error: Option<String>,
}

struct Inner {
    cx: Ctx,
    dir: PathBuf,
    cals: Mutex<Vec<CalMeta>>,
    loaded: Mutex<HashMap<u32, Loaded>>,
    reminders: Mutex<Vec<Reminder>>,
    fetched: Mutex<Option<Instant>>,
    visible: AtomicBool,
    stop: Mutex<bool>,
    wake: Condvar,
}

pub struct Calendar {
    inner: Mutex<Option<Arc<Inner>>>,
}

/// The running calendar, for `agenda_today`.
static LIVE: Mutex<Option<Arc<Inner>>> = Mutex::new(None);

/// Today's agenda as text, or None while the calendar feature is off. Only the chat
/// asks, and only when the user switched "Today's agenda" on.
pub fn agenda_today() -> Option<String> {
    LIVE.lock().ok()?.clone()?.agenda()
}

fn now_s() -> i64 {
    Timestamp::now().as_second()
}

fn secret_key(id: u32) -> String {
    format!("calendar/u{id}")
}

impl Inner {
    fn save_cals(&self) {
        let _ = storage::json_save_atomic(&self.dir.join("calendars.json"), &*self.cals.lock().unwrap());
    }
    fn save_reminders(&self) {
        let _ = storage::json_save_atomic(&self.dir.join("reminders.json"), &*self.reminders.lock().unwrap());
    }
    fn emit(&self, ev: &str) {
        self.cx.emit("calendar", ev, json!({}));
    }

    /// Occurrences of every calendar in `[from, to)` as the page wants them.
    fn events(&self, from: i64, to: i64) -> Value {
        let local = TimeZone::system();
        let cals = self.cals.lock().unwrap();
        let loaded = self.loaded.lock().unwrap();
        let mut out = vec![];
        let mut unsupported = vec![];
        for c in cals.iter() {
            let Some(l) = loaded.get(&c.id) else { continue };
            for e in &l.events {
                if let Some(Err(why)) = &e.rule {
                    unsupported.push(json!({ "title": e.title, "cal": c.name, "reason": why }));
                }
                for o in e.expand(&local, from, to) {
                    out.push(json!({
                        "title": o.title, "location": o.location, "description": o.description, "link": o.link, "start": o.start * 1000, "end": o.end * 1000,
                        "allDay": o.all_day, "cal": c.name, "color": c.color, "unsupported": o.unsupported,
                    }));
                }
            }
        }
        json!({ "events": out, "unsupported": unsupported })
    }

    /// What the strip shows now: the cards of `ics::cards` over the calendars and the extra items the page adds.
    fn cards(&self, lead_min: i64, extra: &Value) -> Value {
        let (now, local) = (now_s(), TimeZone::system());
        let lead = lead_min.clamp(5, 240) * 60;
        let mut items: Vec<ics::Item> = vec![];
        {
            let cals = self.cals.lock().unwrap();
            let loaded = self.loaded.lock().unwrap();
            for l in cals.iter().filter_map(|c| loaded.get(&c.id)) {
                for e in &l.events {
                    for o in e.expand(&local, now, now + lead + 1).into_iter().filter(|o| !o.all_day) {
                        items.push(ics::Item { title: o.title, start: o.start, end: o.end, link: o.link });
                    }
                }
            }
        }
        for x in extra.as_array().into_iter().flatten() {
            let (s, e) = (x["start"].as_i64().unwrap_or(0) / 1000, x["end"].as_i64().unwrap_or(0) / 1000);
            items.push(ics::Item { title: x["title"].as_str().unwrap_or("").chars().take(200).collect(), start: s, end: e, link: None });
        }
        let cards: Vec<Value> = ics::cards(&items, now, lead)
            .into_iter()
            .map(|c| {
                json!({
                    "start": c.start * 1000, "inProgress": c.in_progress,
                    "events": c.items.iter().map(|i| json!({ "title": i.title, "link": i.link })).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({ "cards": cards })
    }

    /// Today's events, one per line, for the chat when the user allows it.
    fn agenda(&self) -> Option<String> {
        let tz = TimeZone::system();
        let start = Zoned::now().with_time_zone(tz.clone()).start_of_day().ok()?;
        let end = start.checked_add(jiff::Span::new().days(1)).ok()?;
        let found = self.events(start.timestamp().as_second(), end.timestamp().as_second());
        let mut list: Vec<&Value> = found["events"].as_array()?.iter().collect();
        list.sort_by_key(|e| e["start"].as_i64().unwrap_or(0));
        let hm = |ms: i64| Timestamp::from_millisecond(ms).map(|t| t.to_zoned(tz.clone()).strftime("%H:%M").to_string()).unwrap_or_default();
        let lines: Vec<String> = list
            .iter()
            .take(30)
            .map(|e| {
                let when = if e["allDay"] == true { "All day".to_string() } else { format!("{}-{}", hm(e["start"].as_i64().unwrap_or(0)), hm(e["end"].as_i64().unwrap_or(0))) };
                let place = e["location"].as_str().filter(|l| !l.is_empty()).map(|l| format!(" ({})", l.chars().take(80).collect::<String>())).unwrap_or_default();
                format!("{when} {}{place}", e["title"].as_str().unwrap_or("").chars().take(120).collect::<String>())
            })
            .collect();
        Some(if lines.is_empty() { "No events today.".to_string() } else { lines.join("\n") })
    }

    fn state(&self) -> Value {
        let loaded = self.loaded.lock().unwrap();
        let cals: Vec<Value> = self
            .cals
            .lock()
            .unwrap()
            .iter()
            .map(|c| {
                let l = loaded.get(&c.id);
                json!({
                    "id": c.id, "name": c.name, "color": c.color,
                    "error": l.and_then(|l| l.error.clone()),
                    "loaded": l.is_some_and(|l| l.error.is_none()),
                    "events": l.map_or(0, |l| l.events.len()),
                    "unsupported": l.map_or(0, |l| l.events.iter().filter(|e| matches!(e.rule, Some(Err(_)))).count()),
                })
            })
            .collect();
        json!({ "calendars": cals, "reminders": self.visible_reminders() })
    }

    /// Open reminders, and the ones ticked today (they stay for the day, then drop out of the list).
    fn visible_reminders(&self) -> Vec<Value> {
        let tz = TimeZone::system();
        let today = Zoned::now().with_time_zone(tz.clone()).date();
        let day_of = |ms: i64| Timestamp::from_millisecond(ms).ok().map(|t| t.to_zoned(tz.clone()).date());
        let mut list: Vec<Reminder> = self
            .reminders
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.done_at.is_none_or(|d| day_of(d) == Some(today)))
            .cloned()
            .collect();
        list.sort_by_key(|r| (r.done_at.is_some(), r.id));
        list.iter().map(|r| json!({ "id": r.id, "text": r.text, "done": r.done_at.is_some() })).collect()
    }

    /// Next two hours hold a timed event: the countdown will want fresh data.
    fn due_soon(&self) -> bool {
        let (now, local) = (now_s(), TimeZone::system());
        let cals = self.cals.lock().unwrap();
        let loaded = self.loaded.lock().unwrap();
        cals.iter().filter_map(|c| loaded.get(&c.id)).flat_map(|l| &l.events).any(|e| e.expand(&local, now, now + 7200).iter().any(|o| !o.all_day))
    }

    fn wanted(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
            || self.fetched.lock().unwrap().is_none_or(|t| t.elapsed() > Duration::from_secs(3 * 3600))
            || self.due_soon()
    }
}

/// Fetch one calendar (or all). The address is the user's own; it is never logged or sent to the page.
async fn refresh(inner: Arc<Inner>, only: Option<u32>) {
    if inner.cx.paused() {
        return;
    }
    let ids: Vec<u32> = inner.cals.lock().unwrap().iter().map(|c| c.id).filter(|id| only.is_none_or(|o| o == *id)).collect();
    for id in ids {
        let result = match secrets::get(&secret_key(id)) {
            None => Err("The address is missing".to_string()),
            // Published calendar links often redirect (to a CDN, to a regional host).
            Some(url) => net::bounded_get_following(&url, Duration::from_secs(20), 8 << 20).await.map_err(|e| e.replace(&url, "the address")),
        };
        let loaded = match result {
            Ok(body) => Loaded { events: ics::parse(&String::from_utf8_lossy(&body)), error: None },
            Err(e) => Loaded { events: vec![], error: Some(e) },
        };
        inner.loaded.lock().unwrap().insert(id, loaded);
    }
    *inner.fetched.lock().unwrap() = Some(Instant::now());
    inner.emit("changed");
}

fn spawn_refresh(inner: &Arc<Inner>, only: Option<u32>) {
    let inner = inner.clone();
    tauri::async_runtime::spawn(refresh(inner, only));
}

fn arg_str<'a>(args: &'a Value, k: &str) -> &'a str {
    args[k].as_str().unwrap_or("").trim()
}

impl Feature for Calendar {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let mut slot = self.inner.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let dir = cx.data_dir("calendar");
        let mut reminders: Vec<Reminder> = storage::json_load(&dir.join("reminders.json"));
        // Ticked reminders are hidden after their day and forgotten after a month.
        let month_ago = Timestamp::now().as_millisecond() - 30 * 86_400_000;
        reminders.retain(|r| r.done_at.is_none_or(|d| d > month_ago));
        let inner = Arc::new(Inner {
            cx: cx.clone(),
            cals: Mutex::new(storage::json_load(&dir.join("calendars.json"))),
            dir,
            loaded: Mutex::new(HashMap::new()),
            reminders: Mutex::new(reminders),
            fetched: Mutex::new(None),
            visible: AtomicBool::new(false),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        // Nothing is fetched without a calendar the user added.
        if !inner.cals.lock().unwrap().is_empty() {
            spawn_refresh(&inner, None);
        }
        let t = inner.clone();
        std::thread::spawn(move || loop {
            let (g, _) = t.wake.wait_timeout_while(t.stop.lock().unwrap(), TICK, |s| !*s).unwrap();
            if *g {
                return;
            }
            drop(g);
            if !t.cals.lock().unwrap().is_empty() && !t.cx.paused() && t.wanted() {
                tauri::async_runtime::block_on(refresh(t.clone(), None));
            }
        });
        *LIVE.lock().unwrap() = Some(inner.clone());
        *slot = Some(inner);
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        *LIVE.lock().unwrap() = None;
        if let Some(i) = self.inner.lock().unwrap().take() {
            *i.stop.lock().unwrap() = true;
            i.wake.notify_all();
        }
    }

    fn call(&self, _cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let i = self.inner.lock().unwrap().clone().ok_or("turned off")?;
        Ok(match method {
            "state" => i.state(),
            "events" => {
                let (from, to) = (args["from"].as_i64().unwrap_or(0) / 1000, args["to"].as_i64().unwrap_or(0) / 1000);
                if to <= from || to - from > 100 * 86400 {
                    return Err("bad range".into());
                }
                i.events(from, to)
            }
            "cards" => i.cards(args["lead"].as_i64().unwrap_or(60), &args["extra"]),
            "seen" => {
                let on = args["visible"].as_bool().unwrap_or(false);
                i.visible.store(on, Ordering::Relaxed);
                let stale = i.fetched.lock().unwrap().is_none_or(|t| t.elapsed() > Duration::from_secs(900));
                if on && stale && !i.cals.lock().unwrap().is_empty() {
                    spawn_refresh(&i, None);
                }
                json!({})
            }
            "refresh" => {
                spawn_refresh(&i, None);
                json!({})
            }
            "add_calendar" => {
                let url = arg_str(&args, "url").replacen("webcal://", "https://", 1);
                // https, or plain http from this machine (a calendar server of the user's own): what the fetch accepts.
                if net::fetchable(&url).is_err() || url.len() > 2000 {
                    return Err("The address must start with https:// or webcal://".into());
                }
                let mut cals = i.cals.lock().unwrap();
                let id = cals.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                let name = arg_str(&args, "name");
                // The link can hold a secret token, so it lives in the Credential Manager and not in the data folder.
                secrets::set(&secret_key(id), &url)?;
                cals.push(CalMeta {
                    id,
                    name: if name.is_empty() { format!("Calendar {id}") } else { name.chars().take(40).collect() },
                    color: COLORS[(id as usize - 1) % COLORS.len()].into(),
                });
                drop(cals);
                i.save_cals();
                spawn_refresh(&i, Some(id));
                i.state()
            }
            "remove_calendar" => {
                let id = args["id"].as_u64().unwrap_or(0) as u32;
                i.cals.lock().unwrap().retain(|c| c.id != id);
                i.loaded.lock().unwrap().remove(&id);
                let _ = secrets::clear(&secret_key(id));
                i.save_cals();
                i.emit("changed");
                i.state()
            }
            "add_reminder" => {
                let text: String = arg_str(&args, "text").chars().take(200).collect();
                if text.is_empty() {
                    return Err("empty".into());
                }
                let mut r = i.reminders.lock().unwrap();
                let id = Timestamp::now().as_millisecond().max(r.iter().map(|x| x.id + 1).max().unwrap_or(0));
                r.push(Reminder { id, text, done_at: None });
                drop(r);
                i.save_reminders();
                i.state()
            }
            "toggle_reminder" => {
                let id = args["id"].as_i64().unwrap_or(0);
                if let Some(r) = i.reminders.lock().unwrap().iter_mut().find(|r| r.id == id) {
                    r.done_at = if r.done_at.is_some() { None } else { Some(Timestamp::now().as_millisecond()) };
                }
                i.save_reminders();
                i.state()
            }
            "delete_reminder" => {
                let id = args["id"].as_i64().unwrap_or(0);
                i.reminders.lock().unwrap().retain(|r| r.id != id);
                i.save_reminders();
                i.state()
            }
            _ => return Err("unknown method".into()),
        })
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/calendar/strings.json")
    }
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(Calendar { inner: Mutex::new(None) })
}
