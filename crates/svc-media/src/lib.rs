// Media — a shared service (docs/ARCHITECTURE.md §8): what the system says is
// playing, and the transport of the player that plays it.
//
// Not a feature: the features that need it list this crate in their own
// Cargo.toml. Refcounted: nothing here runs without a live subscriber, and the
// handle `subscribe` returns unsubscribes on drop.
//
// One worker thread owns Windows' media session manager (Windows.Media.Control,
// public, unelevated). It sleeps on a channel: the manager's and the sessions'
// own events wake it, it waits 120 ms for the burst to end (a track change
// fires several), reads the followed session once and tells the subscribers
// when something a listener can see has changed. Commands travel on the same
// channel, so they return at once and never block a caller.
//
// Which player: the one the user prefers (`pick_source`), and nobody else while
// one is preferred, unless the user lets whoever plays stand in for it while it
// is quiet (`set_fallback`). Without a preference, whoever plays. A command goes
// to the player the subscribers were last told about, never to one that was
// only read in passing (after vorssaint-utils pull request 1967 and
// boring.notch pull requests 1366 and 1536; no code of theirs).

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{Result as WinResult, RuntimeType, HSTRING, PCWSTR};
use windows::ApplicationModel::AppInfo;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    CurrentSessionChangedEventArgs, GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
    GlobalSystemMediaTransportControlsSessionPlaybackInfo as PlaybackInfo,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status, MediaPropertiesChangedEventArgs,
    PlaybackInfoChangedEventArgs, SessionsChangedEventArgs, TimelinePropertiesChangedEventArgs,
};
use windows::Media::{MediaPlaybackAutoRepeatMode, MediaPlaybackType};
use windows::Storage::Streams::{Buffer, DataReader, InputStreamOptions};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Shell::{ApplicationActivationManager, IApplicationActivationManager, AO_NONE};

/// Events of one change arrive in a burst: read once it has been quiet this long…
const DEBOUNCE: Duration = Duration::from_millis(120);
/// …but never wait longer than this on a player that does not stop talking.
const DEBOUNCE_MAX: Duration = Duration::from_millis(500);
/// Between two songs a player reports nothing for a moment: the last one stays this long.
const GAP: Duration = Duration::from_millis(1500);
/// Between two songs a paused player may be all there is to read (a browser
/// hands its one session to another, paused, tab until the next song starts):
/// the song that was playing stays this long before the paused one is believed.
const HANDOVER: Duration = Duration::from_millis(2500);
/// A cover larger than this is not read.
const ART_MAX: u64 = 12 << 20;
/// WinRT time is counted in 100 ns ticks.
const TICKS: f64 = 10_000_000.0;
/// From 1601 (FILETIME, WinRT DateTime) to 1970, in ticks.
const EPOCH_1970: i64 = 116_444_736_000_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Repeat {
    #[default]
    Off,
    Track,
    List,
}

/// What the followed player plays.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Playback {
    /// Which session plays it: the app's id (a packaged app's AUMID, a plain
    /// program's exe name), and for a second session of the same app (another
    /// browser tab) that id with `#2`, `#3`… Stable while the session lives.
    pub source_aumid: String,
    pub source_name: String,
    /// Not the preferred player but the one that stands in while it is quiet (`set_fallback`).
    pub fallback: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// The cover as the player published it (PNG or JPEG). Shared, not copied.
    pub art: Option<Arc<Vec<u8>>>,
    /// Changes whenever `art` does; 0 while there is none.
    pub art_rev: u64,
    pub playing: bool,
    /// Seconds into the track at `at_ms`; while `playing` it moves on at `rate`.
    pub position: f64,
    /// Seconds. 0 when the player publishes no timeline: show no slider.
    pub duration: f64,
    pub rate: f64,
    /// When this was read, in Unix milliseconds.
    pub at_ms: u64,
    pub can_play_pause: bool,
    pub can_next: bool,
    pub can_prev: bool,
    pub can_seek: bool,
    pub can_shuffle: bool,
    pub can_repeat: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
}

impl Playback {
    /// Where the track is at `ms` (Unix milliseconds), carried on from the reading.
    pub fn position_at(&self, ms: u64) -> f64 {
        if !self.playing {
            return self.position;
        }
        let moved = self.position + ms.saturating_sub(self.at_ms) as f64 / 1000.0 * self.rate;
        if self.duration > 0.0 { moved.min(self.duration) } else { moved }
    }

    pub fn position_now(&self) -> f64 {
        self.position_at(now_ms())
    }

    /// `image/png` or `image/jpeg`, from the cover's first bytes.
    pub fn art_mime(&self) -> &'static str {
        match self.art.as_deref() {
            Some(bytes) if bytes.starts_with(b"\x89PNG") => "image/png",
            _ => "image/jpeg",
        }
    }
}

/// A player the system knows about.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    /// The session's key (`Playback::source_aumid`).
    pub aumid: String,
    pub name: String,
    /// What the session plays (a browser tab's page title); empty while unknown.
    pub title: String,
    pub playing: bool,
}

type Listener = Arc<dyn Fn(Option<&Playback>) + Send + Sync>;

struct Shared {
    subs: Vec<(u64, Listener)>,
    last_id: u64,
    /// The worker's channel while a subscriber exists.
    tx: Option<Sender<Msg>>,
    now: Option<Playback>,
    sources: Vec<Source>,
    picked: Option<String>,
    fallback: bool,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    subs: Vec::new(),
    last_id: 0,
    tx: None,
    now: None,
    sources: Vec::new(),
    picked: None,
    fallback: false,
});

fn shared() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

// ── The API (§8) ──────────────────────────────────────────────────────────────

/// Keeps the service running; dropping it unsubscribes, and the last one ends the worker.
pub struct Sub(u64);

/// `on_change`: `f` is called, on the worker thread, whenever what plays changes
/// (track, cover, playing, a seek, the controls, the list of players). Position
/// moving on by itself is not a change: carry it on with `Playback::position_at`.
pub fn subscribe(f: impl Fn(Option<&Playback>) + Send + Sync + 'static) -> Sub {
    let mut s = shared();
    s.last_id += 1;
    let id = s.last_id;
    s.subs.push((id, Arc::new(f)));
    if s.tx.is_none() {
        let (tx, rx) = mpsc::channel();
        s.tx = Some(tx.clone());
        let spawned = std::thread::Builder::new().name("svc-media".into()).spawn(move || worker(tx, rx));
        if let Err(e) = spawned {
            notch_core::log::line(format!("[media] no worker thread: {e}"));
        }
    }
    Sub(id)
}

impl Drop for Sub {
    fn drop(&mut self) {
        let mut s = shared();
        s.subs.retain(|(id, _)| *id != self.0);
        if s.subs.is_empty() {
            if let Some(tx) = s.tx.take() {
                let _ = tx.send(Msg::Quit);
            }
            s.now = None;
            s.sources.clear();
        }
    }
}

/// What plays right now; `None` when nothing does or nobody subscribed.
pub fn state() -> Option<Playback> {
    shared().now.clone()
}

/// Every player with a session, in the system's order.
pub fn sources() -> Vec<Source> {
    shared().sources.clone()
}

/// The cover numbered `rev` (`Playback::art_rev`) with its type. `None` once
/// the track has moved on: a cover asked for one song is never handed out for
/// the next.
pub fn art(rev: u64) -> Option<(Arc<Vec<u8>>, &'static str)> {
    let s = shared();
    let p = s.now.as_ref().filter(|p| rev != 0 && p.art_rev == rev)?;
    Some((p.art.clone()?, p.art_mime()))
}

/// The player `pick_source` chose, if any.
pub fn picked() -> Option<String> {
    shared().picked.clone()
}

/// The preferred player: the only one followed, paused or not, and the only one
/// commands go to; while it is not running nothing plays as far as the
/// subscribers are told (but see `set_fallback`). `None` goes back to automatic
/// (music that plays, then anything that plays, then the one followed last).
/// Kept for the life of the process: the caller remembers it across restarts.
pub fn pick_source(aumid: Option<String>) {
    shared().picked = aumid;
    send(Msg::Changed);
}

/// With a preferred player: whether a player that plays stands in for it while
/// it is paused or closed (`Playback::fallback` says when one does). Off unless asked for.
pub fn set_fallback(on: bool) {
    if std::mem::replace(&mut shared().fallback, on) != on {
        send(Msg::Changed);
    }
}

/// Starts the followed player; one that already plays is left alone.
pub fn play() {
    send(Msg::Cmd(Cmd::Play));
}

/// Pauses the followed player; one that is already paused is left alone.
pub fn pause() {
    send(Msg::Cmd(Cmd::Pause));
}

pub fn play_pause() {
    send(Msg::Cmd(Cmd::PlayPause));
}

pub fn next() {
    send(Msg::Cmd(Cmd::Next));
}

pub fn prev() {
    send(Msg::Cmd(Cmd::Prev));
}

/// Seconds from the start of the track, kept inside it. Nothing happens on a
/// track without an end (a live stream, a player that publishes no timeline).
pub fn seek(seconds: f64) {
    send(Msg::Cmd(Cmd::Seek(seconds)));
}

pub fn shuffle(on: bool) {
    send(Msg::Cmd(Cmd::Shuffle(on)));
}

pub fn repeat(mode: Repeat) {
    send(Msg::Cmd(Cmd::Repeat(mode)));
}

/// Brings the followed player's window forward. Never starts playback.
pub fn activate_player() {
    send(Msg::Cmd(Cmd::Activate));
}

fn send(msg: Msg) {
    if let Some(tx) = &shared().tx {
        let _ = tx.send(msg);
    }
}

// ── The worker ────────────────────────────────────────────────────────────────

enum Msg {
    /// The list of sessions changed: hook them again.
    Sessions,
    /// Title, artist or cover changed: read the cover again.
    Props,
    /// Playing, timeline, or which player to follow.
    Changed,
    Cmd(Cmd),
    Quit,
}

enum Cmd {
    Play,
    Pause,
    PlayPause,
    Next,
    Prev,
    Seek(f64),
    Shuffle(bool),
    Repeat(Repeat),
    Activate,
}

/// A session with the registrations of its three events.
struct Hooked {
    session: Session,
    /// The app's id, and the session's own key (see `Playback::source_aumid`).
    aumid: String,
    key: String,
    title: String,
    tokens: [i64; 3],
}

/// The cover that was read last and what it belongs to.
#[derive(Default)]
struct Art {
    key: String,
    bytes: Option<Arc<Vec<u8>>>,
    rev: u64,
}

fn worker(tx: Sender<Msg>, rx: Receiver<Msg>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    if let Err(e) = run(&tx, &rx) {
        notch_core::log::line(format!("[media] the session manager is not available: {e}"));
    }
    unsafe { CoUninitialize() };
}

/// An event handler that only wakes the worker.
fn wake<S: RuntimeType + 'static, A: RuntimeType + 'static>(tx: &Sender<Msg>, msg: fn() -> Msg) -> TypedEventHandler<S, A> {
    let tx = tx.clone();
    TypedEventHandler::new(move |_, _| {
        let _ = tx.send(msg());
        Ok(())
    })
}

fn run(tx: &Sender<Msg>, rx: &Receiver<Msg>) -> WinResult<()> {
    // debt: `get()` waits without a limit; a session manager that never answers parks this thread.
    let manager = Manager::RequestAsync()?.get()?;
    let on_sessions = manager.SessionsChanged(&wake::<Manager, SessionsChangedEventArgs>(tx, || Msg::Sessions))?;
    let on_current =
        manager.CurrentSessionChanged(&wake::<Manager, CurrentSessionChangedEventArgs>(tx, || Msg::Changed))?;

    let mut hooked: Vec<Hooked> = Vec::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut followed = String::new();
    let mut clocks: HashMap<String, Clock> = HashMap::new();
    let mut art = Art::default();
    let mut gap: Option<Instant> = None;
    let (mut sessions_dirty, mut props_dirty) = (true, true);

    'life: loop {
        if sessions_dirty {
            rehook(&manager, &mut hooked, tx);
        }
        if props_dirty {
            // Each session's own title: two tabs of one browser are told apart by it.
            for h in hooked.iter_mut() {
                h.title = h.session.TryGetMediaPropertiesAsync().and_then(|p| p.get()).and_then(|p| p.Title()).map(|t| t.to_string()).unwrap_or_default();
            }
        }
        let was_followed = followed.clone();
        let (reading, sources) =
            read(&manager, &hooked, &mut names, &mut followed, &mut clocks, &mut art, props_dirty);
        (sessions_dirty, props_dirty) = (false, false);

        // Between two songs the player says nothing, names nothing, or only a
        // paused one is left to read: keep the song that played for a moment.
        let wait = {
            let s = shared();
            hold_for(s.now.as_ref(), reading.as_ref(), s.picked.as_deref())
        };
        match wait {
            Some(wait) if gap.is_none_or(|until| Instant::now() < until) => {
                gap.get_or_insert(Instant::now() + wait);
                // Still the player that was playing: it is the one to go back to.
                if !was_followed.is_empty() {
                    followed = was_followed;
                }
            }
            _ => {
                gap = None;
                publish(reading, sources);
            }
        }

        // Asleep until an event, a command, or the end of the gap.
        let mut msg = match gap {
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break 'life,
            },
            Some(until) => match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break 'life,
            },
        };
        // Then the rest of the burst.
        let give_up = Instant::now() + DEBOUNCE_MAX;
        while let Some(m) = msg.take() {
            match m {
                Msg::Quit => break 'life,
                Msg::Sessions => (sessions_dirty, props_dirty) = (true, true),
                Msg::Props => props_dirty = true,
                Msg::Changed => {}
                Msg::Cmd(cmd) => command(&hooked, cmd),
            }
            let left = give_up.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(DEBOUNCE.min(left)) {
                Ok(m) => msg = Some(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break 'life,
            }
        }
    }

    unhook(&mut hooked);
    let _ = manager.RemoveSessionsChanged(on_sessions);
    let _ = manager.RemoveCurrentSessionChanged(on_current);
    Ok(())
}

/// Sessions that are still there keep their key and registrations; the others are
/// hooked or let go. Windows hands out one session per browser tab, so the key is
/// the app's id for the first and `id#2`, `id#3`… for the next.
fn rehook(manager: &Manager, hooked: &mut Vec<Hooked>, tx: &Sender<Msg>) {
    let Ok(list) = manager.GetSessions() else { return };
    let mut old = std::mem::take(hooked);
    for i in 0..list.Size().unwrap_or(0) {
        let Ok(session) = list.GetAt(i) else { continue };
        if let Some(at) = old.iter().position(|h| h.session == session) {
            hooked.push(old.remove(at));
            continue;
        }
        let aumid = session.SourceAppUserModelId().map(|s| s.to_string()).unwrap_or_default();
        let taken = |key: &str| hooked.iter().chain(&old).any(|h| h.key == key);
        let key = (1..).map(|n| if n == 1 { aumid.clone() } else { format!("{aumid}#{n}") }).find(|k| !taken(k)).unwrap_or_default();
        let tokens = [
            session
                .MediaPropertiesChanged(&wake::<Session, MediaPropertiesChangedEventArgs>(tx, || Msg::Props))
                .unwrap_or(0),
            session
                .PlaybackInfoChanged(&wake::<Session, PlaybackInfoChangedEventArgs>(tx, || Msg::Changed))
                .unwrap_or(0),
            session
                .TimelinePropertiesChanged(&wake::<Session, TimelinePropertiesChangedEventArgs>(tx, || Msg::Changed))
                .unwrap_or(0),
        ];
        hooked.push(Hooked { session, aumid, key, title: String::new(), tokens });
    }
    unhook(&mut old);
}

fn unhook(hooked: &mut Vec<Hooked>) {
    for h in hooked.drain(..) {
        let _ = h.session.RemoveMediaPropertiesChanged(h.tokens[0]);
        let _ = h.session.RemovePlaybackInfoChanged(h.tokens[1]);
        let _ = h.session.RemoveTimelinePropertiesChanged(h.tokens[2]);
    }
}

/// Tells the subscribers, unless nothing they can see has changed.
fn publish(now: Option<Playback>, sources: Vec<Source>) {
    let subs: Vec<Listener> = {
        let mut s = shared();
        if same(&s.now, &now) && s.sources == sources {
            return;
        }
        s.now = now.clone();
        s.sources = sources;
        s.subs.iter().map(|(_, f)| f.clone()).collect()
    };
    for f in subs {
        f(now.as_ref());
    }
}

/// How long a new reading waits before the subscribers hear of it, if at all.
/// `shown` is what they were last told. A change of song is not instant: the
/// player first says nothing, or names nothing (`GAP`), or, when the session
/// that played goes quiet, a paused one is read in its place (`HANDOVER`): a
/// browser with a paused tab and a playing one shows the paused tab's details
/// for about a second each time the playing one skips. None of these replaces
/// a song that was playing until the wait is over. Pausing the song itself, a
/// new song that plays, and a player the user picked are told at once. A player
/// that stood in for the preferred one is no such pick: when it goes quiet
/// between two songs the preferred one, paused, waits like any other.
fn hold_for(shown: Option<&Playback>, reading: Option<&Playback>, picked: Option<&str>) -> Option<Duration> {
    let shown = shown.filter(|p| !p.title.is_empty())?;
    let Some(reading) = reading.filter(|p| !p.title.is_empty()) else { return Some(GAP) };
    let same_song = (&shown.source_aumid, &shown.title, &shown.artist) == (&reading.source_aumid, &reading.title, &reading.artist);
    // debt: picking another player while one stands in waits out the handover too.
    let just_picked =
        picked == Some(reading.source_aumid.as_str()) && shown.source_aumid != reading.source_aumid && !shown.fallback;
    (shown.playing && !reading.playing && !same_song && !just_picked).then_some(HANDOVER)
}

/// The same thing playing, and the position where the last report said it would be.
fn same(a: &Option<Playback>, b: &Option<Playback>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            let carried = a.position_at(b.at_ms);
            let mut a = a.clone();
            (a.position, a.at_ms) = (b.position, b.at_ms);
            a == *b && (carried - b.position).abs() < 1.0
        }
        _ => false,
    }
}

// ── Reading ───────────────────────────────────────────────────────────────────

/// Picks the session to follow and reads it. Returns what it plays, and every player.
fn read(
    manager: &Manager,
    hooked: &[Hooked],
    names: &mut HashMap<String, String>,
    followed: &mut String,
    clocks: &mut HashMap<String, Clock>,
    art: &mut Art,
    fresh_art: bool,
) -> (Option<Playback>, Vec<Source>) {
    // (playing, plays music) of every session. Each one's clock moves on with
    // every look, followed or not: a player that is preferred later has not
    // counted the time it stood still meanwhile.
    let flags: Vec<(bool, bool)> = hooked
        .iter()
        .map(|h| {
            let info = h.session.GetPlaybackInfo().ok();
            let playing = info.as_ref().and_then(|i| i.PlaybackStatus().ok()).is_some_and(plays);
            if let Some(now) = info.as_ref().and_then(|i| clock(&h.session, i, playing, clocks.get(&h.key)).ok()) {
                clocks.insert(h.key.clone(), now);
            }
            let kind = info.and_then(|i| i.PlaybackType().ok()).and_then(|k| k.Value().ok());
            (playing, kind == Some(MediaPlaybackType::Music) || music_app(&h.aumid))
        })
        .collect();
    let sources: Vec<Source> = hooked
        .iter()
        .zip(&flags)
        .map(|(h, (playing, _))| Source {
            aumid: h.key.clone(),
            name: names.entry(h.aumid.clone()).or_insert_with(|| display_name(&h.aumid)).clone(),
            title: h.title.clone(),
            playing: *playing,
        })
        .collect();

    // Whoever plays: music first. Else the player followed last (pausing a video
    // must not jump to older paused music), then the system's own choice, which
    // is what Windows itself calls now playing.
    let (picked, fallback) = {
        let s = shared();
        (s.picked.clone(), s.fallback)
    };
    let at = |aumid: &str| hooked.iter().position(|h| h.key == aumid);
    let at_session = |current: Session| hooked.iter().position(|h| h.session == current);
    let playing = || {
        flags.iter().position(|(playing, music)| *playing && *music).or_else(|| flags.iter().position(|(playing, _)| *playing))
    };
    let resting = || {
        at(followed.as_str())
            .or_else(|| at_session(manager.GetCurrentSession().ok()?))
            .or(if hooked.is_empty() { None } else { Some(0) })
    };
    let chosen = match picked.as_deref() {
        None => playing().or_else(resting),
        // The preferred player and nobody else: closed, there is nothing to tell.
        Some(preferred) if !fallback => at(preferred),
        // …unless the user lets whoever plays stand in for it while it is quiet.
        Some(preferred) => match at(preferred) {
            Some(i) if flags[i].0 => Some(i),
            own => playing().or(own).or_else(resting),
        },
    };
    let Some(i) = chosen else { return (None, sources) };
    let h = &hooked[i];
    *followed = h.key.clone();

    // No clock: the player's timeline could not be read, and so nothing of it is told.
    let playback = clocks.get(&h.key).and_then(|clock| snapshot(&h.session, art, fresh_art, clock).ok()).map(|mut p| {
        p.source_aumid = h.key.clone();
        p.source_name = sources[i].name.clone();
        p.fallback = picked.as_deref().is_some_and(|preferred| preferred != h.key);
        p
    });
    (playback, sources)
}

/// Where a player's track is, kept from one look at the player to the next:
/// position from timestamps. A player reports its position now and then, with
/// the time it did. A new report is believed, and carried on from its time
/// stamp while the player plays. Without a new one the last position is carried
/// on, for as long as the player was seen playing and no longer: a pause is not
/// counted, whether or not the player says where it stood when it paused or
/// when it went on, and a report that arrives late corrects instead of jumping.
#[derive(Clone, Copy)]
struct Clock {
    /// The report this goes back to: when the player made it, and the position it gave (both in ticks).
    report: (i64, i64),
    /// Seconds into the track at `at_ms`.
    position: f64,
    /// Seconds; 0 without an end (a live stream, a player that publishes no timeline).
    duration: f64,
    rate: f64,
    playing: bool,
    at_ms: u64,
}

fn clock(session: &Session, info: &PlaybackInfo, playing: bool, was: Option<&Clock>) -> WinResult<Clock> {
    let timeline = session.GetTimelineProperties()?;
    // A rate of nothing on a player that plays is a player that never set one.
    let rate = info.PlaybackRate().and_then(|r| r.Value()).ok().filter(|r| *r > 0.0).unwrap_or(1.0).min(16.0);
    let start = timeline.StartTime()?.Duration;
    let duration = ((timeline.EndTime()?.Duration - start).max(0) as f64 / TICKS).min(7.0 * 86_400.0);
    let report = (timeline.LastUpdatedTime()?.UniversalTime, timeline.Position()?.Duration);
    let at_ms = now_ms();
    let stamped = report.0 > EPOCH_1970;
    let position = match was {
        Some(was) if stamped && was.report == report => {
            was.position + if was.playing { at_ms.saturating_sub(was.at_ms) as f64 / 1000.0 * was.rate } else { 0.0 }
        }
        _ => {
            let age = if playing && stamped { (at_ms as i64 - (report.0 - EPOCH_1970) / 10_000).max(0) } else { 0 };
            (report.1 - start).max(0) as f64 / TICKS + age as f64 / 1000.0 * rate
        }
    };
    let position = if duration > 0.0 { position.min(duration) } else { position };
    Ok(Clock { report, position, duration, rate, playing, at_ms })
}

fn snapshot(session: &Session, art: &mut Art, fresh_art: bool, clock: &Clock) -> WinResult<Playback> {
    // debt: no time limit on `get()`; a player that never answers stalls the worker.
    let props = session.TryGetMediaPropertiesAsync()?.get()?;
    let info = session.GetPlaybackInfo()?;
    let controls = info.Controls()?;
    let Clock { position, duration, rate, playing, at_ms, .. } = *clock;

    let text = |s: WinResult<HSTRING>| s.map(|s| s.to_string()).unwrap_or_default();
    let title = text(props.Title());
    let mut artist = text(props.Artist());
    if artist.is_empty() {
        artist = text(props.AlbumArtist());
    }
    let album = text(props.AlbumTitle());

    // The cover is read when the track changes or the player says its properties did
    // (it often arrives a moment after the title), not on every tick of the timeline.
    let key = format!("{}\n{title}\n{artist}\n{album}", text(session.SourceAppUserModelId()));
    if fresh_art || key != art.key {
        let bytes = thumbnail(&props);
        if bytes.as_ref() != art.bytes.as_deref() {
            art.bytes = bytes.map(Arc::new);
            art.rev += 1;
        }
        art.key = key;
    }

    let on = |flag: WinResult<bool>| flag.unwrap_or(false);
    Ok(Playback {
        source_aumid: String::new(),
        source_name: String::new(),
        fallback: false,
        title,
        artist,
        album,
        art_rev: if art.bytes.is_some() { art.rev } else { 0 },
        art: art.bytes.clone(),
        playing,
        position,
        duration,
        rate,
        at_ms,
        can_play_pause: on(controls.IsPlayPauseToggleEnabled()) || on(controls.IsPlayEnabled()) || on(controls.IsPauseEnabled()),
        can_next: on(controls.IsNextEnabled()),
        can_prev: on(controls.IsPreviousEnabled()),
        can_seek: duration > 0.0 && on(controls.IsPlaybackPositionEnabled()),
        can_shuffle: on(controls.IsShuffleEnabled()),
        can_repeat: on(controls.IsRepeatEnabled()),
        shuffle: info.IsShuffleActive().and_then(|r| r.Value()).unwrap_or(false),
        repeat: match info.AutoRepeatMode().and_then(|r| r.Value()) {
            Ok(MediaPlaybackAutoRepeatMode::Track) => Repeat::Track,
            Ok(MediaPlaybackAutoRepeatMode::List) => Repeat::List,
            _ => Repeat::Off,
        },
    })
}

fn thumbnail(props: &MediaProperties) -> Option<Vec<u8>> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > ART_MAX {
        return None;
    }
    let buffer = Buffer::Create(size as u32).ok()?;
    let filled = stream.ReadAsync(&buffer, size as u32, InputStreamOptions::None).ok()?.get().ok()?;
    let mut bytes = vec![0u8; filled.Length().ok()? as usize];
    DataReader::FromBuffer(&filled).ok()?.ReadBytes(&mut bytes).ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// Playing, or on its way from one song to the next: that is not a pause.
fn plays(status: Status) -> bool {
    status == Status::Playing || status == Status::Changing
}

/// Players that play music whatever they say about themselves.
fn music_app(aumid: &str) -> bool {
    let id = aumid.to_ascii_lowercase();
    ["spotify", "applemusic", "zunemusic", "itunes", "tidal", "deezer", "foobar2000", "musicbee", "aimp"]
        .iter()
        .any(|known| id.contains(known))
}

/// A plain program's id is its exe name and a browser's a short name: the exe
/// name without `.exe`, lower case, that such an id stands for.
fn exe_stem(aumid: &str) -> String {
    let id = aumid.to_ascii_lowercase();
    let stem = id.strip_suffix(".exe").unwrap_or(&id);
    // Firefox registers under the hash of its install folder.
    if stem.ends_with("308046b0af4a39cb") { "firefox".into() } else { stem.to_string() }
}

/// The app's name as Start shows it, else its id made readable.
fn display_name(aumid: &str) -> String {
    let listed = AppInfo::GetFromAppUserModelId(&HSTRING::from(aumid))
        .and_then(|app| app.DisplayInfo())
        .and_then(|info| info.DisplayName())
        .map(|name| name.to_string())
        .unwrap_or_default();
    if !listed.is_empty() {
        return listed;
    }
    let stem = exe_stem(aumid);
    let mut letters = stem.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// Who a command is for: the player the subscribers were last told about, which
/// is the one the user sees. Not the session that was read last: while a song
/// is held over a gap that is another one. And with a preferred player that
/// nobody stands in for, that player or nobody.
fn target(hooked: &[Hooked]) -> Option<&Session> {
    let s = shared();
    let shown = s.now.as_ref()?.source_aumid.as_str();
    if !s.fallback && s.picked.as_deref().is_some_and(|preferred| preferred != shown) {
        return None;
    }
    hooked.iter().find(|h| h.key == shown).map(|h| &h.session)
}

/// Where a seek to `seconds` lands, in the player's ticks: inside the track,
/// and inside what the player says can be reached in it. `None` for a track
/// without an end and for a number that is none.
fn seek_ticks(seconds: f64, start: i64, end: i64, min: i64, max: i64) -> Option<i64> {
    if end <= start || !seconds.is_finite() {
        return None;
    }
    let (low, high) = if max > min { (min.max(start), max.min(end)) } else { (start, end) };
    // `as` stops at the largest number there is: a seek to the year 3000 is a seek to the end.
    Some(start.saturating_add((seconds.max(0.0) * TICKS) as i64).clamp(low, high.max(low)))
}

/// Sent and not waited for: the player's own events report what came of it, and
/// one that refuses says so with `false`, which nobody needs to see.
fn command(hooked: &[Hooked], cmd: Cmd) {
    let Some(session) = target(hooked) else { return };
    let sent = (|| -> WinResult<()> {
        match cmd {
            // Asked for by name, never as "the other state": a second click, or
            // one made on a report that is out of date, cannot start what was
            // just paused. A player that only offers the toggle gets it when
            // it is not yet where it was asked to be.
            Cmd::Play | Cmd::Pause => {
                let play = matches!(cmd, Cmd::Play);
                let info = session.GetPlaybackInfo()?;
                let controls = info.Controls()?;
                if play && controls.IsPlayEnabled().unwrap_or(false) {
                    session.TryPlayAsync()?;
                } else if !play && controls.IsPauseEnabled().unwrap_or(false) {
                    session.TryPauseAsync()?;
                } else if plays(info.PlaybackStatus()?) != play {
                    session.TryTogglePlayPauseAsync()?;
                }
            }
            Cmd::PlayPause => drop(session.TryTogglePlayPauseAsync()?),
            Cmd::Next => drop(session.TrySkipNextAsync()?),
            Cmd::Prev => drop(session.TrySkipPreviousAsync()?),
            Cmd::Seek(seconds) => {
                let t = session.GetTimelineProperties()?;
                let (start, end) = (t.StartTime()?.Duration, t.EndTime()?.Duration);
                let (min, max) = (t.MinSeekTime()?.Duration, t.MaxSeekTime()?.Duration);
                if let Some(to) = seek_ticks(seconds, start, end, min, max) {
                    session.TryChangePlaybackPositionAsync(to)?;
                }
            }
            Cmd::Shuffle(on) => drop(session.TryChangeShuffleActiveAsync(on)?),
            Cmd::Repeat(mode) => drop(session.TryChangeAutoRepeatModeAsync(match mode {
                Repeat::Off => MediaPlaybackAutoRepeatMode::None,
                Repeat::Track => MediaPlaybackAutoRepeatMode::Track,
                Repeat::List => MediaPlaybackAutoRepeatMode::List,
            })?),
            Cmd::Activate => activate(&session.SourceAppUserModelId()?.to_string()),
        }
        Ok(())
    })();
    if let Err(e) = sent {
        notch_core::log::line(format!("[media] command refused: {e}"));
    }
}

/// Brings the player forward: the window of a plain program or a browser, by its
/// exe name; a packaged app through the shell, which knows its window.
fn activate(aumid: &str) {
    let stem = exe_stem(aumid);
    let processes = notch_core::win::process_table();
    let window = notch_core::win::top_windows().into_iter().find(|(_, pid, _)| {
        processes.get(pid).is_some_and(|p| p.exe.to_ascii_lowercase().strip_suffix(".exe") == Some(stem.as_str()))
    });
    if let Some((hwnd, _, _)) = window {
        notch_core::win::bring_forward(hwnd);
        return;
    }
    // debt: a plain program hidden in the tray has no window to bring forward; nothing happens.
    unsafe {
        let shell: WinResult<IApplicationActivationManager> =
            CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER);
        let _ = shell.and_then(|s| s.ActivateApplication(&HSTRING::from(aumid), PCWSTR::null(), AO_NONE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(source: &str, title: &str, playing: bool) -> Playback {
        Playback { source_aumid: source.into(), title: title.into(), artist: "someone".into(), playing, ..Default::default() }
    }

    #[test]
    fn a_paused_session_never_replaces_the_song_that_plays_during_a_track_change() {
        let playing = song("browser", "Song one", true);
        // The browser hands its session to the paused tab while the next song loads.
        assert_eq!(hold_for(Some(&playing), Some(&song("browser", "The paused tab", false)), None), Some(HANDOVER));
        // The same from another player altogether, and with nothing readable in between.
        assert_eq!(hold_for(Some(&playing), Some(&song("spotify", "Old song", false)), None), Some(HANDOVER));
        assert_eq!(hold_for(Some(&playing), None, None), Some(GAP));
        assert_eq!(hold_for(Some(&playing), Some(&song("browser", "", false)), None), Some(GAP));
        // The next song, playing: told at once.
        assert_eq!(hold_for(Some(&playing), Some(&song("browser", "Song two", true)), None), None);
    }

    #[test]
    fn what_the_user_did_is_told_at_once() {
        let playing = song("browser", "Song one", true);
        // Pausing the song itself.
        assert_eq!(hold_for(Some(&playing), Some(&song("browser", "Song one", false)), None), None);
        // Picking another player, paused or not.
        assert_eq!(hold_for(Some(&playing), Some(&song("spotify", "Old song", false)), Some("spotify")), None);
        // Nothing was playing: whatever is read is the news.
        let paused = song("browser", "Song one", false);
        assert_eq!(hold_for(Some(&paused), Some(&song("browser", "The paused tab", false)), None), None);
        assert_eq!(hold_for(None, Some(&song("browser", "Song one", false)), None), None);
        assert_eq!(hold_for(None, None, None), None);
    }
}
