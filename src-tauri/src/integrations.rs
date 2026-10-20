// Integration pollers — the Rust side of StripePoller / GithubPoller /
// VercelPoller / N8nPoller / ResendPoller / NotionPoller / CalcomPoller.
//
// Same endpoints, same first-run delays and intervals as the Swift pollers. Each
// one emits an `integration` event; the island owns the badge, the sound and the
// 60 s auto-clear, exactly as the Swift handlers do.
//
// Nothing is polled until its key exists in the Credential Manager, and no
// request goes anywhere the user has not configured.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::async_runtime::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Notify;

use crate::github::{self, GitHubActivity, GitHubPulse};
use crate::island::WINDOW_LABEL;
use crate::log;
use crate::net;
use crate::secrets;

const TIMEOUT: Duration = Duration::from_secs(10);

/// What the island receives. `event` is only set when something actually changed,
/// which is what drives the pill badge and the sound.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationUpdate {
    pub id: &'static str,
    pub data: Value,
    pub error: Option<String>,
    pub event: Option<IntegrationEvent>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationEvent {
    pub success: bool,
    pub label: String,
    pub detail: Option<String>,
}

fn emit(app: &AppHandle, update: IntegrationUpdate) {
    let _ = app.emit_to(WINDOW_LABEL, "integration", update);
}

/// For the pollers below, which each send a key to a fixed https API: no
/// redirect is followed with it and nothing goes over plain http (the rule is
/// in net.rs). n8n's address is the user's own and goes through
/// `net::keyed_client` instead.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .build()
        // Never the default client in its place: that one follows redirects.
        // Building only fails where `Client::new()` would panic too.
        .expect("the http client")
}

// ── Developer alerts: shared pieces ───────────────────────────────────────────
//
// GitHub and Vercel notices (pull request 57, 141 and 281 of the upstream
// project had the idea; the pure rules are in alerts.rs). One poller per
// service, which only runs while its pill is on; a request that carries a token
// goes through `net::keyed_client` to its one host.

#[path = "alerts.rs"]
pub mod alerts;

use alerts::{Alert, Backoff, Dedupe, VercelSeen};

static BACKOFF: std::sync::LazyLock<Mutex<std::collections::HashMap<&'static str, Backoff>>> =
    std::sync::LazyLock::new(Default::default);
static ALERTED: std::sync::LazyLock<Mutex<Dedupe>> = std::sync::LazyLock::new(Default::default);
static VERCEL_SEEN: Mutex<VercelSeen> = Mutex::new(None);
/// When the pollers started: a deploy older than this is never news.
static STARTED_MS: std::sync::LazyLock<u64> = std::sync::LazyLock::new(now_ms);

/// A client for a request that carries a token. Only these two hosts.
fn api_client(host: &'static str) -> Option<reqwest::Client> {
    if host != "api.github.com" && host != "api.vercel.com" {
        return None;
    }
    let url = reqwest::Url::parse(&format!("https://{host}/")).ok()?;
    net::keyed_client(&url, TIMEOUT).ok()
}

fn blocked(host: &'static str) -> bool {
    BACKOFF.lock().unwrap().get(host).is_some_and(|b| b.blocked(now_ms()))
}

/// Reads the rate-limit headers and the status of an answer: a limit waits for
/// the reset time, other errors back off, a good answer clears the count.
fn note_response(host: &'static str, response: &reqwest::Response) {
    let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let status = response.status();
    let mut all = BACKOFF.lock().unwrap();
    let backoff = all.entry(host).or_default();
    let now = now_ms();
    if status.as_u16() == 429 || header("x-ratelimit-remaining") == Some(0) {
        backoff.limited(now, header("x-ratelimit-reset"), header("retry-after"));
    } else if status.is_success() {
        backoff.ok();
    } else {
        backoff.fail(now, 30);
    }
}

fn note_failure(host: &'static str) {
    BACKOFF.lock().unwrap().entry(host).or_default().fail(now_ms(), 30);
}

/// The island words it in the interface language and posts it as a notice.
fn emit_alerts(app: &AppHandle, alerts: Vec<Alert>) {
    for alert in alerts {
        if ALERTED.lock().unwrap().first(&alert.key) {
            let _ = app.emit_to(WINDOW_LABEL, "dev-alert", &alert);
        }
    }
}

/// The GitHub token for one request: the saved one, else the `gh` login when
/// "Use the gh login" is on. Never kept: the caller drops it after the request.
async fn github_token() -> Option<String> {
    if let Some(token) = secrets::get("github-token") {
        return Some(token);
    }
    if !github::gh_login_on() {
        return None;
    }
    tauri::async_runtime::spawn_blocking(github::gh_token).await.ok()?.ok()
}

/// The poller tasks, while the integrations feature runs.
static TASKS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Spawns every poller with the macOS delays and intervals. A second call
/// while they run does nothing.
pub fn start(app: AppHandle) {
    let mut tasks = TASKS.lock().unwrap();
    if !tasks.is_empty() {
        return;
    }
    let _ = *STARTED_MS;
    tasks.push(spawn(app.clone(), "integration_n8n", 3, 15, poll_n8n));
    tasks.push(spawn(app.clone(), "integration_vercel", 5, 30, poll_vercel));
    tasks.push(spawn(app.clone(), "integration_stripe", 6, 30, poll_stripe));
    tasks.push(spawn(app.clone(), "integration_resend", 6, 60, poll_resend));
    tasks.push(spawn(app.clone(), "integration_github", 7, 300, poll_github));
    tasks.extend(spawn_github_loops(app.clone()));
    tasks.push(spawn(app.clone(), "integration_calcom", 8, 300, poll_calcom));
    tasks.push(spawn(app.clone(), "integration_notion", 9, 300, poll_notion));
    tasks.push(spawn(app, "integration_linear", 10, 120, poll_linear));
    log::line(format!("integrations: {} pollers started", tasks.len()));
}

/// Aborts every poller: no timer is left, and a request in flight is dropped.
pub fn stop() {
    let tasks: Vec<JoinHandle<()>> = TASKS.lock().unwrap().drain(..).collect();
    for task in &tasks {
        task.abort();
    }
    log::line(format!("integrations: {} pollers stopped", tasks.len()));
}

/// True when the integrations feature runs and the user has this integration
/// switched on in settings.
fn enabled(app: &AppHandle, id: &str) -> bool {
    crate::registry::is_running(app, "integrations")
        && app.try_state::<crate::Shared>()
        .map(|shared| {
            let settings = shared.settings.lock().unwrap();
            settings.active_integrations.iter().any(|x| x == id)
        })
        .unwrap_or(false)
}

fn spawn<F, Fut>(app: AppHandle, id: &'static str, delay_secs: u64, every_secs: u64, poll: F) -> JoinHandle<()>
where
    F: Fn(AppHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(every_secs));
        loop {
            ticker.tick().await;
            // The ticker keeps its cadence; we just decline to do the work. An
            // integration the user switched off, or a paused app, must make no
            // network calls at all — CLAUDE.md allows talking only to services
            // the user configured, and a disabled one is not configured.
            if notch_core::paused() || !enabled(&app, id) {
                continue;
            }
            poll(app.clone()).await;
        }
    })
}

/// One-shot refresh from the Refresh buttons in the island.
pub async fn poll_once(app: AppHandle, id: &str) {
    match id {
        "integration_stripe" => poll_stripe(app).await,
        "integration_github" => {
            wake_github_pulse();
            github_refresh_if_stale("activity");
            poll_github(app).await
        }
        "integration_vercel" => poll_vercel(app).await,
        "integration_n8n" => poll_n8n(app).await,
        "integration_resend" => poll_resend(app).await,
        "integration_notion" => poll_notion(app).await,
        "integration_linear" => poll_linear(app).await,
        "integration_calcom" => poll_calcom(app).await,
        _ => {}
    }
}

/// Remembers the newest id per integration so an event fires once, not on every poll.
struct Seen(Mutex<std::collections::HashMap<&'static str, String>>);

static SEEN: std::sync::LazyLock<Seen> =
    std::sync::LazyLock::new(|| Seen(Mutex::new(std::collections::HashMap::new())));

/// Returns true the first time a given id is seen (and false on the very first
/// load, which only fills the card).
fn is_new(key: &'static str, id: &str) -> bool {
    let mut map = SEEN.0.lock().unwrap();
    match map.insert(key, id.to_string()) {
        Some(previous) => previous != id,
        None => false, // first poll: populate silently, like the Swift pollers
    }
}

/// The card's error line, in the interface language (i18n.rs);
/// `unauthorised_hint` comes translated.
fn status_error(code: u16, unauthorised_hint: &str) -> String {
    match code {
        401 => crate::i18n::t("Invalid API key (401)"),
        403 => unauthorised_hint.to_string(),
        _ => crate::i18n::tf("API error {code}", &[("code", &code.to_string())]),
    }
}

// ── Stripe ────────────────────────────────────────────────────────────────────

async fn poll_stripe(app: AppHandle) {
    let Some(key) = secrets::get("stripe-api-key") else { return };
    let auth = format!("Basic {}", crate::claude::base64_for(format!("{key}:").as_bytes()));
    let http = client();

    let balance = http
        .get("https://api.stripe.com/v1/balance")
        .header("Authorization", &auth)
        .send()
        .await;

    let (amount, currency) = match balance {
        Ok(r) if r.status().is_success() => {
            let json: Value = r.json().await.unwrap_or(json!({}));
            let mut buckets: Vec<Value> = Vec::new();
            for k in ["available", "pending"] {
                if let Some(arr) = json.get(k).and_then(Value::as_array) {
                    buckets.extend(arr.iter().cloned());
                }
            }
            let currency = buckets
                .first()
                .and_then(|b| b.get("currency"))
                .and_then(Value::as_str)
                .unwrap_or("eur")
                .to_string();
            let amount: i64 = buckets
                .iter()
                .filter_map(|b| b.get("amount").and_then(Value::as_i64))
                .sum();
            (amount, currency)
        }
        Ok(r) => {
            let code = r.status().as_u16();
            emit(&app, IntegrationUpdate {
                id: "integration_stripe",
                data: json!({}),
                error: Some(status_error(code, &crate::i18n::t("Use a secret key (sk_live_… not pk_live_…)"))),
                event: None,
            });
            return;
        }
        Err(e) => {
            emit(&app, IntegrationUpdate {
                id: "integration_stripe",
                data: json!({}),
                error: Some(crate::i18n::tf("No connection: {error}", &[("error", &e.to_string())])),
                event: None,
            });
            return;
        }
    };

    let charges = http
        .get("https://api.stripe.com/v1/charges?limit=3")
        .header("Authorization", &auth)
        .send()
        .await;
    let Ok(response) = charges else { return };
    if !response.status().is_success() {
        return;
    }
    let json: Value = response.json().await.unwrap_or(json!({}));
    let payments: Vec<Value> = json
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let description = c
                        .get("description")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            c.get("billing_details")
                                .and_then(|b| b.get("name"))
                                .and_then(Value::as_str)
                        })
                        .map(str::to_string);
                    Some(json!({
                        "id": c.get("id")?.as_str()?,
                        "amount": c.get("amount")?.as_i64()?,
                        "currency": c.get("currency")?.as_str()?,
                        "description": description,
                        "createdAt": c.get("created").and_then(Value::as_i64).unwrap_or(0) * 1000,
                        "status": c.get("status").and_then(Value::as_str).unwrap_or("succeeded"),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let newest = payments
        .first()
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let event = if !newest.is_empty() && is_new("stripe", &newest) {
        let label = payments[0]
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                let cents = payments[0].get("amount").and_then(Value::as_i64).unwrap_or(0);
                format!("{:.2}", cents as f64 / 100.0)
            });
        Some(IntegrationEvent { success: true, label, detail: None })
    } else {
        None
    };

    emit(&app, IntegrationUpdate {
        id: "integration_stripe",
        data: json!({ "balance": amount, "currency": currency, "payments": payments }),
        error: None,
        event,
    });
}

// ── GitHub ────────────────────────────────────────────────────────────────────

async fn poll_github(app: AppHandle) {
    if blocked("api.github.com") {
        return;
    }
    let Some(token) = github_token().await else { return };
    let Some(http) = api_client("api.github.com") else { return };

    let user = http
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", notch_brand::SLUG)
        .send()
        .await;
    let Ok(response) = user else {
        note_failure("api.github.com");
        return;
    };
    note_response("api.github.com", &response);
    if !response.status().is_success() {
        emit(&app, IntegrationUpdate {
            id: "integration_github",
            data: json!({}),
            error: Some(status_error(response.status().as_u16(), &crate::i18n::t("Token lacks the needed scope"))),
            event: None,
        });
        return;
    }
    let json: Value = response.json().await.unwrap_or(json!({}));
    let public = json.get("public_repos").and_then(Value::as_i64).unwrap_or(0);
    let private = json
        .get("owned_private_repos")
        .or_else(|| json.get("total_private_repos"))
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let repos = http
        .get("https://api.github.com/user/repos?per_page=100&affiliation=owner&sort=pushed")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", notch_brand::SLUG)
        .send()
        .await;
    let stars: i64 = match repos {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|list| {
                list.iter()
                    .filter_map(|r| r.get("stargazers_count").and_then(Value::as_i64))
                    .sum()
            })
            .unwrap_or(0),
        _ => 0,
    };

    GITHUB.cache.lock().unwrap().stats = Some((public + private, stars));
    emit_github(&app);
}

// ── GitHub pulse and activity (GithubPoller.swift) ────────────────────────────
//
// Two more loops next to the stats above, with the Mac cadence: the pulse (my
// pull requests and their CI, reviews waiting for me, default-branch CI) 10 s
// after launch, then every 60 s while some CI is running and every 5 min
// otherwise; the contribution calendar 15 s after launch, then every 30 min.
// Neither touches the network while the pill is off or Lintel is paused, and
// the island wakes them when the card is opened on stale data.
//
// All three results are kept here and always sent together, so one poll never
// wipes what another one reported.

const GITHUB_ID: &str = "integration_github";

#[derive(Default)]
struct GitHubCache {
    /// (repositories, stars)
    stats: Option<(i64, i64)>,
    pulse: Option<GitHubPulse>,
    activity: Option<GitHubActivity>,
    /// The watch list `pulse` was fetched with: a changed list starts silent.
    watch: Vec<String>,
}

struct GitHubLoops {
    cache: Mutex<GitHubCache>,
    /// Bumped when the token changes: a response for the old token is dropped.
    generation: AtomicU64,
    pulse_wake: Notify,
    activity_wake: Notify,
    pulse_busy: AtomicBool,
    activity_busy: AtomicBool,
}

static GITHUB: std::sync::LazyLock<GitHubLoops> = std::sync::LazyLock::new(|| GitHubLoops {
    cache: Mutex::new(GitHubCache::default()),
    generation: AtomicU64::new(0),
    pulse_wake: Notify::new(),
    activity_wake: Notify::new(),
    pulse_busy: AtomicBool::new(false),
    activity_busy: AtomicBool::new(false),
});

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What the island's GitHub card receives: whichever of the three is known.
fn github_card_data(cache: &GitHubCache) -> Value {
    let mut data = serde_json::Map::new();
    if let Some((repos, stars)) = cache.stats {
        data.insert("totalRepos".into(), json!(repos));
        data.insert("totalStars".into(), json!(stars));
    }
    if let Some(pulse) = &cache.pulse {
        data.insert("pulse".into(), serde_json::to_value(pulse).unwrap_or(Value::Null));
    }
    if let Some(activity) = &cache.activity {
        data.insert("activity".into(), serde_json::to_value(activity).unwrap_or(Value::Null));
    }
    Value::Object(data)
}

fn emit_github(app: &AppHandle) {
    let data = github_card_data(&GITHUB.cache.lock().unwrap());
    emit(app, IntegrationUpdate { id: GITHUB_ID, data, error: None, event: None });
}

fn spawn_github_loops(app: AppHandle) -> [JoinHandle<()>; 2] {
    let pulse_app = app.clone();
    let pulse = tauri::async_runtime::spawn(async move {
        let mut wait = Duration::from_secs(10);
        loop {
            // Sleeps for `wait`, or less when the island asks for fresh data.
            let _ = tokio::time::timeout(wait, GITHUB.pulse_wake.notified()).await;
            let pending = run_github_pulse(&pulse_app).await;
            wait = Duration::from_secs(if pending { 60 } else { 300 });
        }
    });
    let activity = tauri::async_runtime::spawn(async move {
        let mut wait = Duration::from_secs(15);
        loop {
            let _ = tokio::time::timeout(wait, GITHUB.activity_wake.notified()).await;
            run_github_activity(&app).await;
            wait = Duration::from_secs(1800);
        }
    });
    [pulse, activity]
}

/// One authenticated GraphQL call. Partial errors are logged (as a count) and the
/// data parsed anyway; only a response without `data` is dropped.
async fn github_graphql(token: &str, query: &str, what: &str) -> Option<Value> {
    if blocked("api.github.com") {
        return None;
    }
    let response = api_client("api.github.com")?
        .post("https://api.github.com/graphql")
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", notch_brand::SLUG)
        .json(&json!({ "query": query }))
        .send()
        .await;
    let Ok(response) = response else {
        note_failure("api.github.com");
        log::line(format!("github {what}: no connection"));
        return None;
    };
    note_response("api.github.com", &response);
    if !response.status().is_success() {
        log::line(format!("github {what} HTTP {}", response.status().as_u16()));
        return None;
    }
    let root: Value = response.json().await.ok()?;
    let errors = github::graphql_error_count(&root);
    if errors > 0 {
        log::line(format!("github {what} GraphQL errors: {errors}"));
    }
    root.get("data").filter(|d| d.is_object())?;
    Some(root)
}

/// Returns whether some CI is still running, which sets the next interval.
async fn run_github_pulse(app: &AppHandle) -> bool {
    if notch_core::paused() || !enabled(app, GITHUB_ID) {
        return false;
    }
    let Some(token) = github_token().await else { return false };
    let generation = GITHUB.generation.load(Ordering::SeqCst);
    let watch = github::watch_repos();
    GITHUB.pulse_busy.store(true, Ordering::SeqCst);
    let root = github_graphql(&token, &github::pulse_query(&watch), "pulse").await;
    drop(token);
    GITHUB.pulse_busy.store(false, Ordering::SeqCst);
    let Some(mut pulse) = root.and_then(|r| GitHubPulse::parse(&r, now_ms())) else { return false };
    pulse.retain_watched(&watch);
    github::forge_into(&mut pulse).await;
    let pending = pulse.has_pending();

    let (events, notices) = {
        let mut cache = GITHUB.cache.lock().unwrap();
        // The token changed while this was in flight: it answers for someone else.
        if GITHUB.generation.load(Ordering::SeqCst) != generation {
            return pending;
        }
        // Another watch list: what it brings in was already there, not news.
        let old = if cache.watch == watch { cache.pulse.as_ref() } else { None };
        let events = GitHubPulse::events(old, &pulse);
        let notices = github::alerts(old, &pulse, &events);
        cache.pulse = Some(pulse);
        cache.watch = watch;
        (events, notices)
    };
    emit_github(app);
    if !events.is_empty() {
        let _ = app.emit_to(WINDOW_LABEL, "github-alerts", &events);
    }
    emit_alerts(app, notices);
    pending
}

async fn run_github_activity(app: &AppHandle) {
    if notch_core::paused() || !enabled(app, GITHUB_ID) {
        return;
    }
    let Some(token) = github_token().await else { return };
    let generation = GITHUB.generation.load(Ordering::SeqCst);
    GITHUB.activity_busy.store(true, Ordering::SeqCst);
    let root = github_graphql(&token, github::ACTIVITY_QUERY, "activity").await;
    drop(token);
    GITHUB.activity_busy.store(false, Ordering::SeqCst);
    let Some(activity) = root.and_then(|r| GitHubActivity::parse(&r, now_ms())) else { return };
    {
        let mut cache = GITHUB.cache.lock().unwrap();
        if GITHUB.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        cache.activity = Some(activity);
    }
    emit_github(app);
}

fn wake_github_pulse() {
    if !GITHUB.pulse_busy.load(Ordering::SeqCst) {
        GITHUB.pulse_wake.notify_one();
    }
}

/// The card was opened: fetch now if what it shows is older than the Mac's
/// limits (1 min for the pulse, 5 min for the activity grid). No-op while a
/// request is already in flight.
pub fn github_refresh_if_stale(section: &str) {
    let now = now_ms();
    let cache = GITHUB.cache.lock().unwrap();
    match section {
        "pulse" => {
            let fetched = cache.pulse.as_ref().map(|p| p.fetched_at);
            if !GITHUB.pulse_busy.load(Ordering::SeqCst) && github::is_stale(fetched, now, 60) {
                GITHUB.pulse_wake.notify_one();
            }
        }
        "activity" => {
            let fetched = cache.activity.as_ref().map(|a| a.fetched_at);
            if !GITHUB.activity_busy.load(Ordering::SeqCst) && github::is_stale(fetched, now, 300) {
                GITHUB.activity_wake.notify_one();
            }
        }
        _ => {}
    }
}

/// The GitHub token was saved or removed: forget what the old one fetched, drop
/// its requests still in flight, and fetch again right away.
pub fn github_token_changed(app: &AppHandle) {
    GITHUB.generation.fetch_add(1, Ordering::SeqCst);
    let has_token = secrets::present("github-token") || github::gh_login_on();
    {
        let mut cache = GITHUB.cache.lock().unwrap();
        cache.pulse = None;
        cache.activity = None;
        if !has_token {
            cache.stats = None;
        }
    }
    emit_github(app);
    GITHUB.pulse_wake.notify_one();
    GITHUB.activity_wake.notify_one();
    if has_token && !notch_core::paused() && enabled(app, GITHUB_ID) {
        tauri::async_runtime::spawn(poll_github(app.clone()));
    }
}

/// Switching the pill off forgets the pulse, so switching it back on starts
/// silent instead of alerting on everything that changed in between.
pub fn settings_saved(app: &AppHandle, active_integrations: &[String]) {
    if !active_integrations.iter().any(|id| id == "integration_vercel") {
        *VERCEL_SEEN.lock().unwrap() = None;
    }
    if active_integrations.iter().any(|id| id == GITHUB_ID) {
        return;
    }
    let had_data = {
        let mut cache = GITHUB.cache.lock().unwrap();
        let had = cache.pulse.is_some() || cache.activity.is_some();
        cache.pulse = None;
        cache.activity = None;
        had
    };
    if had_data {
        emit_github(app);
    }
}

// ── Vercel ────────────────────────────────────────────────────────────────────

async fn poll_vercel(app: AppHandle) {
    if blocked("api.vercel.com") {
        return;
    }
    let Some(token) = secrets::get("vercel-token") else { return };
    let Some(http) = api_client("api.vercel.com") else { return };
    let response = http
        .get("https://api.vercel.com/v6/deployments?limit=20")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .send()
        .await;
    drop(token);
    let Ok(response) = response else {
        note_failure("api.vercel.com");
        return;
    };
    note_response("api.vercel.com", &response);
    if !response.status().is_success() {
        emit(&app, IntegrationUpdate {
            id: "integration_vercel",
            data: json!({}),
            error: Some(status_error(response.status().as_u16(), &crate::i18n::t("Token lacks access"))),
            event: None,
        });
        return;
    }
    let json: Value = response.json().await.unwrap_or(json!({}));
    vercel_notices(&app, &json);
    let terminal = ["READY", "ERROR", "CANCELED"];
    let deployments: Vec<Value> = json
        .get("deployments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|d| {
                    let state = d.get("state")?.as_str()?;
                    if !terminal.contains(&state) {
                        return None;
                    }
                    let meta = d.get("meta");
                    let pick = |keys: [&str; 3]| {
                        meta.and_then(|m| keys.iter().find_map(|k| m.get(*k).and_then(Value::as_str)))
                            .map(str::to_string)
                    };
                    Some(json!({
                        "id": d.get("uid")?.as_str()?,
                        "projectName": d.get("name")?.as_str()?,
                        "url": d.get("url").and_then(Value::as_str).unwrap_or(""),
                        "state": state,
                        "createdAt": d.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0),
                        "commitMessage": pick(["githubCommitMessage", "gitlabCommitMessage", "bitbucketCommitMessage"]),
                        "branch": pick(["githubCommitRef", "gitlabCommitRef", "bitbucketBranch"]),
                    }))
                })
                .take(5)
                .collect()
        })
        .unwrap_or_default();

    // With the notices on, the pill's own badge and sound would say it twice.
    let event = deployments.first().filter(|_| !github::vercel_alerts_on()).and_then(|latest| {
        let id = latest.get("id")?.as_str()?;
        if !is_new("vercel", id) {
            return None;
        }
        let success = latest.get("state")?.as_str()? == "READY";
        Some(IntegrationEvent {
            success,
            label: latest.get("projectName")?.as_str()?.to_string(),
            detail: None,
        })
    });

    emit(&app, IntegrationUpdate {
        id: "integration_vercel",
        data: json!({ "deployments": deployments }),
        error: None,
        event,
    });
}

/// Deploys that turned Ready or Failed since the last poll, as notices, when
/// the user opted in. The state is followed either way, so switching the option
/// on later does not announce what has been there.
fn vercel_notices(app: &AppHandle, json: &Value) {
    let text = |d: &Value, key: &str| d.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let deps: Vec<alerts::Dep> = json
        .get("deployments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|d| {
                    let meta = d.get("meta");
                    let pick = |keys: [&str; 3]| {
                        meta.and_then(|m| keys.iter().find_map(|k| m.get(*k).and_then(Value::as_str))).unwrap_or("").to_string()
                    };
                    let branch = pick(["githubCommitRef", "gitlabCommitRef", "bitbucketBranch"]);
                    let message = pick(["githubCommitMessage", "gitlabCommitMessage", "bitbucketCommitMessage"]);
                    Some(alerts::Dep {
                        id: d.get("uid")?.as_str()?.to_string(),
                        project: text(d, "name"),
                        state: d.get("state")?.as_str()?.to_string(),
                        created_ms: d.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0) as u64,
                        host: text(d, "url"),
                        detail: if branch.is_empty() { message.lines().next().unwrap_or("").to_string() } else { branch },
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let notices = alerts::vercel_alerts(
        &mut VERCEL_SEEN.lock().unwrap(),
        &deps,
        &github::watch_projects(),
        *STARTED_MS,
        &mut ALERTED.lock().unwrap(),
    );
    if github::vercel_alerts_on() {
        for alert in notices {
            let _ = app.emit_to(WINDOW_LABEL, "dev-alert", &alert);
        }
    }
}

// ── Resend ────────────────────────────────────────────────────────────────────

async fn poll_resend(app: AppHandle) {
    let Some(key) = secrets::get("resend-api-key") else { return };
    let response = client()
        .get("https://api.resend.com/emails?limit=100")
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .send()
        .await;
    let Ok(response) = response else { return };
    if !response.status().is_success() {
        emit(&app, IntegrationUpdate {
            id: "integration_resend",
            data: json!({}),
            error: Some(status_error(response.status().as_u16(), &crate::i18n::t("Key lacks access"))),
            event: None,
        });
        return;
    }
    let json: Value = response.json().await.unwrap_or(json!({}));
    let total = json
        .get("total")
        .or_else(|| json.get("count"))
        .and_then(Value::as_i64);
    let emails: Vec<Value> = json
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(5)
                .filter_map(|e| {
                    let to = match e.get("to") {
                        Some(Value::Array(a)) => a.clone(),
                        Some(Value::String(s)) => vec![Value::String(s.clone())],
                        _ => vec![],
                    };
                    Some(json!({
                        "id": e.get("id")?.as_str()?,
                        "to": to,
                        "subject": e.get("subject").and_then(Value::as_str).unwrap_or(""),
                        "createdAt": e.get("created_at").and_then(Value::as_str).unwrap_or(""),
                        "lastEvent": e.get("last_event").and_then(Value::as_str).unwrap_or(""),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    emit(&app, IntegrationUpdate {
        id: "integration_resend",
        data: json!({ "emails": emails, "total": total }),
        error: None,
        event: None,
    });
}

// ── Notion ────────────────────────────────────────────────────────────────────

async fn poll_notion(app: AppHandle) {
    let Some(token) = secrets::get("notion-api-key") else { return };
    let response = client()
        .post("https://api.notion.com/v1/search")
        .header("Authorization", format!("Bearer {token}"))
        .header("Notion-Version", "2022-06-28")
        .header("Content-Type", "application/json")
        .json(&json!({
            "sort": { "direction": "descending", "timestamp": "last_edited_time" },
            "page_size": 3
        }))
        .send()
        .await;
    let Ok(response) = response else { return };
    if !response.status().is_success() {
        emit(&app, IntegrationUpdate {
            id: "integration_notion",
            data: json!({}),
            error: Some(status_error(response.status().as_u16(), &crate::i18n::t("Integration lacks access"))),
            event: None,
        });
        return;
    }
    let json: Value = response.json().await.unwrap_or(json!({}));
    let pages: Vec<Value> = json
        .get("results")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(parse_notion_page).collect())
        .unwrap_or_default();

    emit(&app, IntegrationUpdate {
        id: "integration_notion",
        data: json!({ "pages": pages }),
        error: None,
        event: None,
    });
}

fn parse_notion_page(obj: &Value) -> Option<Value> {
    let id = obj.get("id")?.as_str()?;
    let is_database = obj.get("object").and_then(Value::as_str) == Some("database");

    let mut title = "Untitled".to_string();
    if is_database {
        if let Some(text) = obj
            .get("title")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|t| t.get("plain_text"))
            .and_then(Value::as_str)
        {
            if !text.is_empty() {
                title = text.to_string();
            }
        }
    } else if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for prop in props.values() {
            if prop.get("type").and_then(Value::as_str) != Some("title") {
                continue;
            }
            if let Some(text) = prop
                .get("title")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|t| t.get("plain_text"))
                .and_then(Value::as_str)
            {
                if !text.is_empty() {
                    title = text.to_string();
                    break;
                }
            }
        }
    }

    let emoji = obj
        .get("icon")
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("emoji"))
        .and_then(|i| i.get("emoji"))
        .and_then(Value::as_str);

    Some(json!({
        "id": id,
        "title": title,
        "emoji": emoji,
        "lastEditedAt": obj.get("last_edited_time").and_then(Value::as_str)?,
        "url": obj.get("url").and_then(Value::as_str).unwrap_or("https://notion.so"),
    }))
}

// ── Linear ────────────────────────────────────────────────────────────────────

/// The unread inbox: the newest unread notifications about issues. A new one (never seen
/// before in this run) badges the pill. The personal API key goes as it is in `Authorization`.
async fn poll_linear(app: AppHandle) {
    let Some(key) = secrets::get("linear-api-key") else { return };
    let query = "{ notifications(first: 30) { nodes { id readAt createdAt type         ... on IssueNotification { issue { identifier title url } actor { name } } } } }";
    let response = client()
        .post("https://api.linear.app/graphql")
        .header("Authorization", key)
        .json(&json!({ "query": query }))
        .send()
        .await;
    let Ok(response) = response else { return };
    if !response.status().is_success() {
        emit(&app, IntegrationUpdate {
            id: "integration_linear",
            data: json!({}),
            error: Some(status_error(response.status().as_u16(), &crate::i18n::t("Key lacks access"))),
            event: None,
        });
        return;
    }
    let body: Value = response.json().await.unwrap_or(json!({}));
    let nodes = body.pointer("/data/notifications/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
    let unread: Vec<Value> = nodes
        .iter()
        .filter(|n| n.get("readAt").is_none_or(Value::is_null))
        .filter_map(|n| {
            let issue = n.get("issue").filter(|i| !i.is_null())?;
            Some(json!({
                "id": n.get("id")?.as_str()?,
                "identifier": issue.get("identifier").and_then(Value::as_str).unwrap_or(""),
                "title": issue.get("title").and_then(Value::as_str).unwrap_or(""),
                "url": issue.get("url").and_then(Value::as_str)?,
                "actor": n.pointer("/actor/name").and_then(Value::as_str),
                "kind": n.get("type").and_then(Value::as_str).unwrap_or(""),
                "createdAt": n.get("createdAt").and_then(Value::as_str),
            }))
        })
        .collect();
    let event = unread.first().and_then(|first| {
        let id = first.get("id")?.as_str()?;
        if !is_new("linear", id) {
            return None;
        }
        let label = format!("{} {}", first["identifier"].as_str()?, first["title"].as_str()?);
        Some(IntegrationEvent { success: true, label, detail: None })
    });
    emit(&app, IntegrationUpdate {
        id: "integration_linear",
        data: json!({ "unread": unread }),
        error: None,
        event,
    });
}

// ── Cal.com ───────────────────────────────────────────────────────────────────

async fn poll_calcom(app: AppHandle) {
    let Some(key) = secrets::get("calcom-api-key") else { return };
    let response = client()
        .get("https://api.cal.com/v2/bookings?status=upcoming")
        .header("Authorization", format!("Bearer {key}"))
        .header("cal-api-version", "2024-08-13")
        .send()
        .await;
    let Ok(response) = response else { return };
    if !response.status().is_success() {
        emit(&app, IntegrationUpdate {
            id: "integration_calcom",
            data: json!({}),
            error: Some(status_error(response.status().as_u16(), &crate::i18n::t("Key lacks access"))),
            event: None,
        });
        return;
    }
    let json: Value = response.json().await.unwrap_or(json!({}));
    let bookings: Vec<Value> = json
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|b| {
                    let start = b
                        .get("start")
                        .or_else(|| b.get("startTime"))
                        .and_then(Value::as_str)?;
                    let attendee = b.get("attendees").and_then(Value::as_array).and_then(|a| a.first());
                    let notes = b
                        .get("responses")
                        .and_then(|r| r.get("notes"))
                        .and_then(|n| n.get("value"))
                        .and_then(Value::as_str)
                        .or_else(|| b.get("description").and_then(Value::as_str))
                        .filter(|s| !s.is_empty());
                    Some(json!({
                        "id": b.get("id").map(|v| v.to_string()).unwrap_or_default(),
                        "title": b.get("title").and_then(Value::as_str).unwrap_or("Meeting"),
                        "start": start,
                        "status": b.get("status").and_then(Value::as_str).unwrap_or("accepted"),
                        "attendeeName": attendee.and_then(|a| a.get("name")).and_then(Value::as_str),
                        "attendeeEmail": attendee.and_then(|a| a.get("email")).and_then(Value::as_str),
                        "attendeeNotes": notes,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    emit(&app, IntegrationUpdate {
        id: "integration_calcom",
        data: json!({ "bookings": bookings }),
        error: None,
        event: None,
    });
}

// ── n8n ───────────────────────────────────────────────────────────────────────
//
// The instance's address is typed by the user, so the key only goes where
// `net::keyed_url` allows (https, or plain http to this machine) and is never
// sent on after a redirect. Ported from upstream
// `windows/src-tauri/src/integrations.rs`, pull request 427.

/// True while the n8n card shows an error set here, so that it is cleared once
/// the address is fixed, without emitting again on every poll.
static N8N_ERROR_SHOWN: AtomicBool = AtomicBool::new(false);

/// `log_line` is written when the error is set, not on every poll (every 15 s):
/// repeated, it would rotate the log and lose the useful lines. It never names
/// the address, which can carry credentials.
fn show_n8n_error(app: &AppHandle, error: String, log_line: &str) {
    if !N8N_ERROR_SHOWN.swap(true, Ordering::Relaxed) {
        log::line(log_line);
    }
    emit(app, IntegrationUpdate { id: "integration_n8n", data: json!({}), error: Some(error), event: None });
}

fn clear_n8n_error(app: &AppHandle) {
    if N8N_ERROR_SHOWN.swap(false, Ordering::Relaxed) {
        emit(app, IntegrationUpdate { id: "integration_n8n", data: json!({}), error: None, event: None });
    }
}

/// Capped like the chat providers' answers: whatever listens at the configured
/// address must not be able to fill the memory.
async fn n8n_json(response: reqwest::Response) -> Option<Value> {
    let bytes = net::read_capped(response, net::MAX_BODY).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn poll_n8n(app: AppHandle) {
    let (Some(key), Some(raw_base)) = (secrets::get("n8n-api-key"), secrets::get("n8n-url")) else {
        // Address or key removed: the card must not keep the previous error.
        clear_n8n_error(&app);
        return;
    };
    let base_url = match net::keyed_url(&raw_base) {
        Ok(url) => url,
        Err(error) => {
            show_n8n_error(&app, error, "n8n: address refused, key not sent");
            return;
        }
    };
    let Ok(http) = net::keyed_client(&base_url, TIMEOUT) else { return };
    let base = base_url.as_str().trim_end_matches('/').to_string();

    // Same two shapes as the Swift poller: the public API first, then /rest.
    let list_urls = [
        format!("{base}/api/v1/executions?limit=1&includeData=false"),
        format!("{base}/rest/executions?limit=1&includeData=false"),
    ];

    let mut items: Option<Vec<Value>> = None;
    let mut redirected = false;
    for url in &list_urls {
        let Ok(response) = http.get(url).header("X-N8N-API-KEY", &key).header("Accept", "application/json").send().await
        else {
            continue;
        };
        if response.status().is_redirection() {
            // Logged once, along with the card's error (show_n8n_error).
            redirected = true;
            continue;
        }
        if !response.status().is_success() {
            // Only the status: a self-hosted base URL can carry credentials.
            log::line(format!("n8n list HTTP {}", response.status()));
            continue;
        }
        let Some(json) = n8n_json(response).await else { continue };
        items = match &json {
            Value::Object(o) => o.get("data").and_then(Value::as_array).cloned(),
            Value::Array(a) => Some(a.clone()),
            _ => None,
        };
        if items.is_some() {
            break;
        }
    }

    if items.is_none() && redirected {
        show_n8n_error(
            &app,
            crate::i18n::t("The server redirects elsewhere, and the key is not sent on. Enter the final address."),
            "n8n: redirected, key not sent on",
        );
        return;
    }
    // Address accepted and no redirect: an error set earlier no longer applies.
    clear_n8n_error(&app);

    let Some(first) = items.and_then(|list| list.into_iter().next()) else { return };
    let id = match first.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return,
    };

    let status = first.get("status").and_then(Value::as_str).unwrap_or("");
    if !["success", "error", "crashed", "canceled", "failed"].contains(&status) {
        return;
    }
    if !is_new("n8n", &id) {
        return;
    }
    let success = status == "success";

    let detail_urls = [
        format!("{base}/api/v1/executions/{id}?includeData=true"),
        format!("{base}/api/v1/executions/{id}"),
        format!("{base}/rest/executions/{id}?includeData=true"),
        format!("{base}/rest/executions/{id}"),
    ];
    let mut name = crate::i18n::t("Workflow");
    let mut detail = None;
    for url in &detail_urls {
        let Ok(response) = http.get(url).header("X-N8N-API-KEY", &key).header("Accept", "application/json").send().await
        else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        // A detail too large to read (includeData=true) falls through to the
        // next address, which does not ask for the data.
        let Some(json) = n8n_json(response).await else { continue };
        name = json
            .get("workflowData")
            .and_then(|w| w.get("name"))
            .and_then(Value::as_str)
            .or_else(|| json.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| crate::i18n::t("Workflow"));
        detail = n8n_detail(&json, success);
        break;
    }

    log::line(format!("n8n execution {id} {status} · {name}"));
    emit(&app, IntegrationUpdate {
        id: "integration_n8n",
        data: json!({ "workflow": name, "status": status }),
        error: None,
        event: Some(IntegrationEvent { success, label: name, detail }),
    });
}

fn n8n_detail(json: &Value, success: bool) -> Option<String> {
    let result = json.get("data")?.get("resultData")?;
    if !success {
        if let Some(error) = result.get("error") {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            if let Some(node) = error.get("node").and_then(|n| n.get("name")).and_then(Value::as_str) {
                if !node.is_empty() {
                    return Some(format!("{node}\n{message}"));
                }
            }
            return Some(message.to_string());
        }
        let runs = result.get("runData")?.as_object()?;
        for (node, value) in runs {
            if let Some(message) = value
                .as_array()
                .and_then(|a| a.first())
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
            {
                return Some(format!("{node}\n{message}"));
            }
        }
        return None;
    }

    let last_node = result.get("lastNodeExecuted")?.as_str()?;
    let items = result
        .get("runData")?
        .get(last_node)?
        .as_array()?
        .first()?
        .get("data")?
        .get("main")?
        .as_array()?
        .first()?
        .as_array()?;
    let count = items.len();
    let header = format!("→ {last_node} · {count} item{}", if count == 1 { "" } else { "s" });

    let fields = items
        .first()
        .and_then(|i| i.get("json"))
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .take(4)
                .map(|(k, v)| format!("{k}: {}", fmt_value(v)))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty());

    Some(match fields {
        Some(f) => format!("{header}\n{f}"),
        None => header,
    })
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.chars().take(50).collect(),
        Value::Array(a) => format!("[{}]", a.len()),
        Value::Object(_) => "{…}".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_card_data_sends_only_what_is_known() {
        let mut cache = GitHubCache::default();
        assert_eq!(github_card_data(&cache), json!({}));
        cache.stats = Some((12, 3400));
        assert_eq!(github_card_data(&cache), json!({ "totalRepos": 12, "totalStars": 3400 }));
        cache.activity = Some(GitHubActivity { total: 5, weeks: vec![], fetched_at: 9 });
        let data = github_card_data(&cache);
        assert_eq!(data["totalRepos"], json!(12));
        assert_eq!(data["activity"], json!({ "total": 5, "weeks": [], "fetchedAt": 9 }));
        assert!(data.get("pulse").is_none());
    }
}
