// The stages the feature puts on the input hub (svc-input), and the thread
// that does their slow work. No Tauri in here: the probe runs it as it is.
//
// A stage runs on the hook thread, between a key and the window it is for. It
// reads the configuration as one shared pointer, keeps its own small state
// (touched by that thread only) and hands anything that waits (typing an
// expansion, pressing a mapped combination) to the worker. A stage that
// swallowed a press swallows its release too, and forgets what it holds when
// the release never comes: a missed release must not leave a key dead.
//
// The debounce rules are ported from KeyboardDebounceSupport.swift and
// MouseClickDebounceSupport.swift, the side wheel gate from
// MouseButtonShortcutSupport.swift, the wheel rules from ScrollWheelSupport.swift
// and the typing buffer from TextSnippetSupport.swift, each changed where
// Windows reports input differently (see the notes on the stages).

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;

use svc_input::Verdict::{self, Pass, Swallow};
use svc_input::{Button, Key, Mouse, MouseKind, Sub};
use windows::Win32::Foundation::{CloseHandle, POINT, RECT};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, GetKeyboardLayout, ToUnicodeEx};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, WindowFromPoint, GA_ROOT, GUITHREADINFO, GWL_STYLE,
};

use crate::keys::{self, Chord};
use crate::text;

// ── Configuration ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Snippet {
    pub id: String,
    pub name: String,
    /// What is typed: 2 to 40 characters, no spaces.
    pub trigger: String,
    pub text: String,
    /// Expands on the trigger's last character instead of after a space, Tab or Enter.
    pub immediate: bool,
    pub ignore_case: bool,
    pub folder: String,
    pub library: bool,
    pub on: bool,
}

/// What the stages do, already checked and parsed. A stage exists only while
/// its part of this asks for it.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub snippets: Vec<Snippet>,
    pub key_debounce: bool,
    /// Milliseconds. 0 leaves a key alone.
    pub key_ms: u32,
    /// Keys with a window of their own: (`key_slot`, milliseconds).
    pub key_overrides: Vec<(u16, u32)>,
    pub click_debounce: bool,
    pub click_ms: u32,
    pub invert_vertical: bool,
    pub invert_horizontal: bool,
    /// The same for a touchpad (small steps), apart from the mouse wheel.
    pub pad_vertical: bool,
    pub pad_horizontal: bool,
    /// The one modifier (a bit of `keys`) that turns the wheel sideways while held.
    pub sideways: Option<u8>,
    /// Program names in lower case (`chrome.exe`), as every list of apps here.
    pub scroll_exceptions: Vec<String>,
    /// The keys each input presses: Back, Forward, side wheel left, side wheel right. Empty: not mapped.
    pub buttons: [Vec<u16>; 4],
    pub button_exceptions: Vec<String>,
    /// From a combination to the keys pressed for it (none: the combination does nothing).
    pub remaps: Vec<(Chord, Vec<u16>)>,
    pub guard_apps: Vec<String>,
    pub guard_keys: Vec<Chord>,
    /// For the probe, which is its own user: input another program sent counts
    /// as typed by hand, and the app's own windows are typed into like any other.
    pub probe: bool,
}

/// Where a key is on the keyboard, for `Config::key_overrides`.
pub fn key_slot(scan: u16, extended: bool) -> u16 {
    (scan & 0xFF) | (extended as u16) << 8
}

/// What the page is told.
#[derive(Debug, Clone)]
pub enum Event {
    /// A snippet was typed out.
    Expanded { id: String },
    /// The recorder heard a combination.
    Recorded(String),
}

type Sink = Arc<dyn Fn(Event) + Send + Sync>;

enum Job {
    Expand { cfg: Arc<Config>, index: usize, deletes: usize, trailing: Option<char> },
    Keys { cfg: Arc<Config>, slot: usize },
    Recorded(Chord),
    Stop,
}

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
struct KeySlot {
    down: bool,
    /// The press was a bounce: its release is swallowed too. When the press was swallowed.
    eaten: u64,
    /// When the last accepted press was released, plus one (0: never).
    released: u64,
}

#[derive(Clone, Copy, Default)]
struct Click {
    /// When the press that was let through came, plus one (0: the button is up).
    accepted: u64,
    /// When a press was held back as a bounce, plus one (0: none).
    suppressed: u64,
    released: u64,
}

/// A remapped key that is down: what was pressed for it and what was lifted.
struct Held {
    source: u16,
    cfg: Arc<Config>,
    rule: usize,
    /// Which keys of the rule's output this press put down (bit i: key i).
    pressed: u8,
    /// The user's own modifiers taken up for the output; 0 is an empty place.
    lifted: [u16; 8],
}

impl Held {
    fn keys(&self) -> impl DoubleEndedIterator<Item = u16> + '_ {
        let out = &self.cfg.remaps[self.rule].1;
        out.iter().enumerate().filter(|(i, _)| self.pressed & (1 << i) != 0).map(|(_, k)| *k)
    }
}

#[derive(Default)]
struct Buttons {
    /// Side buttons whose press was swallowed (bit 0 Back, bit 1 Forward).
    consumed: u8,
    wheel_at: u64,
    wheel_fired: u8,
}

#[derive(Default)]
struct Under {
    root: isize,
    rect: RECT,
    at: u64,
    exe: String,
}

#[derive(Default)]
struct Recorder {
    /// Listening until then (0: not listening).
    until: u64,
    /// A combination was heard at this time: its tail is swallowed for a second.
    done: u64,
    mods: u8,
}

struct Shared {
    cfg: RwLock<Arc<Config>>,
    jobs: mpsc::Sender<Job>,
    key_state: Mutex<Box<[KeySlot; 512]>>,
    click_state: Mutex<[Click; 3]>,
    /// Blocked keys that are down, with the time of their last event.
    guard_state: Mutex<Vec<(u16, u64)>>,
    remap_state: Mutex<Vec<Held>>,
    /// The last 64 characters typed.
    typed: Mutex<Vec<char>>,
    button_state: Mutex<Buttons>,
    under: Mutex<Under>,
    recorder: Mutex<Recorder>,
    /// The caret moved (a click, another window): what was typed before does not count.
    reset: AtomicBool,
    front_own: AtomicBool,
    front_exe: Mutex<String>,
    guard_live: AtomicBool,
    last_front: AtomicIsize,
    elevated: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn config(&self) -> Arc<Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn job(&self, job: Job) {
        let _ = self.jobs.send(job);
    }
}

/// Another program's input is not the user's hand: the stages that judge or
/// rewrite what the user does leave it alone.
fn foreign(cfg: &Config, injected: bool) -> bool {
    injected && !cfg.probe
}

/// A key no keyboard has and no program reads. Sent when a combination with a
/// modifier is swallowed: a modifier released with nothing in between is a
/// command of its own (Alt opens the menu bar, Win the Start menu).
const MASK: u16 = 0xE8;

/// How long a swallowed press is waited on for its release, in microseconds.
/// The longest repeat delay Windows has is one second.
const STALE: u64 = 2_000_000;

// ── Keyboard stages ───────────────────────────────────────────────────────────

/// Order 0: while Settings records a combination, every key is the recorder's.
fn recorder(shared: &Shared, key: &Key) -> Verdict {
    let mut r = lock(&shared.recorder);
    if r.until == 0 || key.t > r.until {
        return Pass;
    }
    if r.done != 0 {
        return if key.t.saturating_sub(r.done) < 1_000_000 { Swallow } else { Pass };
    }
    let bit = keys::modifier_of(key.vk);
    let chord = if key.down {
        if bit != 0 {
            r.mods |= bit;
            return Swallow;
        }
        Chord { mods: r.mods, vk: key.vk }
    } else if bit != 0 && r.mods & bit != 0 {
        // A modifier let go with no key pressed: the combination ends on it ("Ctrl", "Ctrl+Shift").
        r.mods &= !bit;
        Chord { mods: r.mods, vk: keys::generic(key.vk) }
    } else {
        // The release of a key that was down before the recording began.
        return Pass;
    };
    if keys::known(chord.vk) {
        r.done = key.t.max(1);
        shared.job(Job::Recorded(chord));
    }
    Swallow
}

/// Order 10. A key pressed again within the window of its own release is a
/// bounce: that press and its release are swallowed. Windows repeats a held key
/// as presses with no release between, so a press while the key is down is
/// always a repeat and always passes (the upstream rule would eat the first
/// repeat). Keys are known by their place, not their letter; modifiers are left alone.
fn key_debounce(shared: &Shared, key: &Key) -> Verdict {
    let cfg = shared.config();
    if foreign(&cfg, key.injected) || keys::modifier_of(key.vk) != 0 || matches!(key.vk, 0xE5 | 0xE7) {
        return Pass;
    }
    let slot = key_slot(key.scan, key.extended) as usize;
    let mut state = lock(&shared.key_state);
    let s = &mut state[slot];
    if s.eaten != 0 && key.t.saturating_sub(s.eaten) > STALE {
        s.eaten = 0;
    }
    if !key.down {
        if s.eaten != 0 {
            s.eaten = 0;
            return Swallow;
        }
        s.down = false;
        s.released = key.t + 1;
        return Pass;
    }
    if s.eaten != 0 {
        s.eaten = key.t.max(1);
        return Swallow;
    }
    if s.down {
        return Pass;
    }
    let ms = cfg.key_overrides.iter().find(|o| o.0 as usize == slot).map_or(cfg.key_ms, |o| o.1);
    if s.released != 0 && (key.t + 1).saturating_sub(s.released) < ms as u64 * 1000 {
        s.eaten = key.t.max(1);
        return Swallow;
    }
    s.down = true;
    Pass
}

/// Order 20: the chosen combinations do nothing while a chosen app is in front.
fn guard(shared: &Shared, key: &Key) -> Verdict {
    let vk = keys::generic(key.vk);
    let mut eaten = lock(&shared.guard_state);
    eaten.retain(|k| key.t.saturating_sub(k.1) <= STALE);
    let at = eaten.iter().position(|k| k.0 == vk);
    if !key.down {
        return match at {
            Some(at) => {
                eaten.swap_remove(at);
                Swallow
            }
            None => Pass,
        };
    }
    if let Some(at) = at {
        eaten[at].1 = key.t;
        return Swallow;
    }
    if !shared.guard_live.load(Relaxed) {
        return Pass;
    }
    let cfg = shared.config();
    let chord = keys::of_event(key);
    if foreign(&cfg, key.injected) || !cfg.guard_keys.contains(&chord) {
        return Pass;
    }
    eaten.push((vk, key.t));
    if chord.mods != 0 {
        svc_input::inject_keys(&[(MASK, true), (MASK, false)]);
    }
    Swallow
}

/// Order 30: a key or a combination stands for another, for as long as it is held.
fn remap(shared: &Shared, key: &Key) -> Verdict {
    let cfg = shared.config();
    if foreign(&cfg, key.injected) {
        return Pass;
    }
    let vk = keys::generic(key.vk);
    let mut held = lock(&shared.remap_state);
    if !key.down {
        if keys::modifier_of(key.vk) != 0 {
            // The user let go of a modifier that was lifted for an output: it is not pressed back.
            for slot in held.iter_mut().flat_map(|h| h.lifted.iter_mut()).filter(|s| **s == key.vk) {
                *slot = 0;
            }
        }
        let Some(at) = held.iter().position(|h| h.source == vk) else { return Pass };
        release(&held.remove(at), true);
        return Swallow;
    }
    if let Some(h) = held.iter().find(|h| h.source == vk) {
        // The key repeats while it is held: so does the key it stands for.
        if let Some(last) = h.keys().last().filter(|k| keys::modifier_of(*k) == 0) {
            svc_input::inject_keys(&[(last, true)]);
        }
        return Swallow;
    }
    let chord = keys::of_event(key);
    let Some(rule) = cfg.remaps.iter().position(|r| r.0 == chord) else { return Pass };
    let out = &cfg.remaps[rule].1;
    let wanted = out.iter().fold(0, |mods, k| mods | keys::modifier_of(*k));
    let mut strokes = [(0u16, false); 16];
    let mut n = 0;
    let mut push = |stroke: (u16, bool)| {
        if n < strokes.len() {
            strokes[n] = stroke;
            n += 1;
        }
    };
    // The modifiers the user holds that the output does not have come up for as long.
    let lift = if out.is_empty() { 0 } else { chord.mods & !wanted };
    if chord.mods != 0 && (out.is_empty() || lift != 0) {
        push((MASK, true));
        push((MASK, false));
    }
    let mut lifted = [0u16; 8];
    for (slot, vk) in lifted.iter_mut().zip(keys::held_keys(lift)) {
        *slot = vk;
        push((vk, false));
    }
    let mut pressed = 0u8;
    for (i, k) in out.iter().enumerate().take(8) {
        // A modifier the user already holds is theirs to release.
        if keys::modifier_of(*k) & chord.mods == 0 {
            pressed |= 1 << i;
            push((*k, true));
        }
    }
    svc_input::inject_keys(&strokes[..n]);
    held.push(Held { source: vk, cfg: cfg.clone(), rule, pressed, lifted });
    Swallow
}

/// Lets go of what a remapped key put down; `restore` presses the lifted modifiers back.
fn release(h: &Held, restore: bool) {
    let mut ups = [(0u16, false); 8];
    let mut n = 0;
    for k in h.keys().rev() {
        ups[n] = (k, false);
        n += 1;
    }
    svc_input::inject_keys(&ups[..n]);
    if restore {
        let mut downs = [(0u16, true); 8];
        let mut n = 0;
        for k in h.lifted.iter().filter(|k| **k != 0) {
            downs[n] = (*k, true);
            n += 1;
        }
        svc_input::inject_keys(&downs[..n]);
    }
}

fn release_all(shared: &Shared) {
    for h in lock(&shared.remap_state).drain(..) {
        release(&h, false);
    }
}

enum Typed {
    Char(char),
    /// A dead key: the accent waits for its letter.
    Dead,
    Nothing,
}

/// The character a key press types in the window in front, without touching
/// the keyboard state (flag 4 of ToUnicodeEx), so dead keys keep working.
fn typed(key: &Key, mods: u8) -> Typed {
    let mut state = [0u8; 256];
    for (bit, vk) in [(keys::SHIFT, 0x10), (keys::CTRL, 0x11), (keys::ALT, 0x12)] {
        if mods & bit != 0 {
            state[vk] = 0x80;
        }
    }
    let mut out = [0u16; 8];
    let n = unsafe {
        if GetKeyState(0x14) & 1 != 0 {
            state[0x14] = 1;
        }
        let layout = GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None));
        ToUnicodeEx(key.vk as u32, key.scan as u32, &state, &mut out, 4, Some(layout))
    };
    match n {
        n if n < 0 => Typed::Dead,
        0 => Typed::Nothing,
        n => match char::decode_utf16(out[..(n as usize).min(out.len())].iter().copied()).next() {
            Some(Ok(c)) if !c.is_control() || matches!(c, '\t' | '\r') => Typed::Char(c),
            _ => Typed::Nothing,
        },
    }
}

/// The text cursor is in a password field of the classic kind.
// debt: UI Automation (IsPassword) for browsers and modern apps; this sees Win32 edit fields only.
fn password_focus() -> bool {
    unsafe {
        let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        if GetGUIThreadInfo(thread, &mut info).is_err() || info.hwndFocus.is_invalid() {
            return false;
        }
        let mut class = [0u16; 32];
        let len = GetClassNameW(info.hwndFocus, &mut class) as usize;
        let edit = String::from_utf16_lossy(&class[..len]).to_ascii_lowercase().contains("edit");
        // ES_PASSWORD
        edit && GetWindowLongW(info.hwndFocus, GWL_STYLE) as u32 & 0x20 != 0
    }
}

fn ends_with(before: &[char], s: &Snippet) -> bool {
    let n = s.trigger.chars().count();
    n > 0
        && before.len() >= n
        && before[before.len() - n..]
            .iter()
            .zip(s.trigger.chars())
            .all(|(a, b)| *a == b || (s.ignore_case && a.to_lowercase().eq(b.to_lowercase())))
}

/// Order 40: remembers what was typed and, when it ends with a trigger,
/// swallows the key that completed it and has the worker type the text.
/// A dead key empties the memory: a trigger cannot hold an accent typed in two keys.
fn snippets(shared: &Shared, key: &Key) -> Verdict {
    if !key.down || keys::modifier_of(key.vk) != 0 {
        return Pass;
    }
    let mut buffer = lock(&shared.typed);
    if shared.reset.swap(false, Relaxed) {
        buffer.clear();
    }
    let cfg = shared.config();
    // The app's own windows (the library, the command bar) are not typed into for the user.
    if shared.front_own.load(Relaxed) && !cfg.probe {
        buffer.clear();
        return Pass;
    }
    match key.vk {
        0x08 => {
            buffer.pop();
            return Pass;
        }
        // An input method at work, Esc, the arrows and their block, Delete: the caret is elsewhere now.
        0xE5 | 0x1B | 0x21..=0x28 | 0x2E => {
            buffer.clear();
            return Pass;
        }
        _ => {}
    }
    let mods = keys::held();
    // Ctrl with Alt is AltGr and types; Ctrl, Alt or Win otherwise make a command, not text.
    if mods & keys::WIN != 0 || (mods & keys::CTRL != 0) != (mods & keys::ALT != 0) {
        buffer.clear();
        return Pass;
    }
    // 0xE7: a character sent as itself (the on-screen keyboard, another typing tool).
    let c = if key.vk == 0xE7 {
        char::from_u32(key.scan as u32)
    } else {
        match typed(key, mods) {
            Typed::Char(c) => Some(c),
            Typed::Dead => {
                buffer.clear();
                None
            }
            Typed::Nothing => None,
        }
    };
    let Some(c) = c else { return Pass };
    buffer.push(c);
    if buffer.len() > 64 {
        buffer.remove(0);
    }
    let delimiter = matches!(c, ' ' | '\t' | '\r' | '\n');
    let before = &buffer[..buffer.len() - delimiter as usize];
    // The longest trigger wins: ";email2" before ";email".
    let found = cfg
        .snippets
        .iter()
        .enumerate()
        .filter(|(_, s)| s.on && s.immediate != delimiter && ends_with(before, s))
        .max_by_key(|(_, s)| s.trigger.chars().count());
    let Some((index, s)) = found else { return Pass };
    if password_focus() {
        buffer.clear();
        return Pass;
    }
    // The key that completed an immediate trigger is swallowed, so it is not on screen to erase.
    let deletes = s.trigger.chars().count() - s.immediate as usize;
    buffer.clear();
    shared.job(Job::Expand { cfg: cfg.clone(), index, deletes, trailing: delimiter.then_some(c) });
    Swallow
}

// ── Mouse stages ──────────────────────────────────────────────────────────────

/// Order 10. A healthy click passes at once. A press inside the window after
/// an accepted release is a bounce: it is swallowed with its own release. The
/// release of an accepted press is never swallowed. Left, right and middle only.
fn click_debounce(shared: &Shared, m: &Mouse) -> Verdict {
    let (button, down) = match m.kind {
        MouseKind::Down(b) => (b, true),
        MouseKind::Up(b) => (b, false),
        _ => return Pass,
    };
    let i = match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        _ => return Pass,
    };
    let cfg = shared.config();
    if foreign(&cfg, m.injected) {
        return Pass;
    }
    let window = cfg.click_ms as u64 * 1000;
    let now = m.t + 1;
    let mut state = lock(&shared.click_state);
    let s = &mut state[i];
    if !down {
        if s.suppressed != 0 {
            s.suppressed = 0;
            return Swallow;
        }
        if s.accepted != 0 {
            s.accepted = 0;
            s.released = now;
        }
        // A release with no press of ours passes: it cannot leave a button held.
        return Pass;
    }
    // More presses while one is held are noise of the same switch, for as long
    // as the window; later than that, the release was missed and this is a new click.
    if s.suppressed != 0 && now.saturating_sub(s.suppressed) < window.max(STALE) {
        return Swallow;
    }
    if s.accepted != 0 && now.saturating_sub(s.accepted) < window {
        return Swallow;
    }
    s.suppressed = 0;
    if s.released != 0 && now.saturating_sub(s.released) < window {
        s.suppressed = now;
        return Swallow;
    }
    s.accepted = now;
    Pass
}

/// Order 15, for the snippets: a click moves the caret.
fn click_resets(shared: &Shared, m: &Mouse) -> Verdict {
    if matches!(m.kind, MouseKind::Down(_)) {
        shared.reset.store(true, Relaxed);
    }
    Pass
}

fn exe_of(pid: u32) -> String {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut path = [0u16; 512];
        let mut len = path.len() as u32;
        let read = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, windows::core::PWSTR(path.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        if read.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&path[..len as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or_default().to_lowercase()
    }
}

/// The app under the pointer (or, with `front_too`, the one in front) is on
/// the list. With an empty list nothing is looked up at all; otherwise the
/// answer is kept while the pointer stays in that window, for half a second.
// debt: its own small resolver; switch to svc-windows `app_identity` once every package can use it.
fn excepted(shared: &Shared, list: &[String], m: &Mouse, front_too: bool) -> bool {
    if list.is_empty() {
        return false;
    }
    if front_too && list.contains(&lock(&shared.front_exe)) {
        return true;
    }
    // Where the pointer is as this process sees the screen (the event's own
    // point is in real pixels whatever the process is told about scaling).
    let mut at = POINT::default();
    if unsafe { GetCursorPos(&mut at) }.is_err() {
        return false;
    }
    let mut under = lock(&shared.under);
    let r = under.rect;
    let fresh = under.root != 0 && m.t.saturating_sub(under.at) < 500_000 && (r.left..r.right).contains(&at.x) && (r.top..r.bottom).contains(&at.y);
    if !fresh {
        unsafe {
            let root = GetAncestor(WindowFromPoint(at), GA_ROOT);
            let mut pid = 0u32;
            GetWindowThreadProcessId(root, Some(&mut pid));
            let mut rect = RECT::default();
            let _ = GetWindowRect(root, &mut rect);
            *under = Under { root: root.0 as isize, rect, at: m.t, exe: if pid == 0 { String::new() } else { exe_of(pid) } };
        }
    }
    list.contains(&under.exe)
}

/// Order 30: a side button or a turn of the side wheel presses its combination.
/// The whole press is swallowed, release included, even when the mapping went
/// away meanwhile.
fn buttons(shared: &Shared, m: &Mouse) -> Verdict {
    let slot = match m.kind {
        MouseKind::Down(Button::X1) => 0,
        MouseKind::Down(Button::X2) => 1,
        MouseKind::Up(b @ (Button::X1 | Button::X2)) => {
            let bit = if b == Button::X1 { 1 } else { 2 };
            let mut state = lock(&shared.button_state);
            let ours = state.consumed & bit != 0;
            state.consumed &= !bit;
            return if ours { Swallow } else { Pass };
        }
        MouseKind::HWheel if m.delta < 0 => 2,
        MouseKind::HWheel if m.delta > 0 => 3,
        _ => return Pass,
    };
    let cfg = shared.config();
    if foreign(&cfg, m.injected) || cfg.buttons[slot].is_empty() || excepted(shared, &cfg.button_exceptions, m, true) {
        return Pass;
    }
    let mut state = lock(&shared.button_state);
    if slot < 2 {
        state.consumed |= 1 << slot;
        shared.job(Job::Keys { cfg: cfg.clone(), slot });
        return Swallow;
    }
    // One move of a side wheel arrives as several packets: the first of each
    // direction counts, and a quarter second of quiet begins a new move.
    let bit = 1 << slot;
    if state.wheel_at == 0 || m.t.saturating_sub(state.wheel_at) > 250_000 {
        state.wheel_fired = 0;
    }
    state.wheel_at = m.t.max(1);
    if state.wheel_fired & bit == 0 {
        state.wheel_fired |= bit;
        shared.job(Job::Keys { cfg: cfg.clone(), slot });
    }
    Swallow
}

/// Order 40: the wheel of a mouse turned the other way, or sideways while the
/// chosen key is held. A hook cannot change an event, so the turn is swallowed
/// and sent again changed (marked as the app's own: it is not seen here twice).
fn wheel(shared: &Shared, m: &Mouse) -> Verdict {
    let horizontal = match m.kind {
        MouseKind::Wheel => false,
        MouseKind::HWheel => true,
        _ => return Pass,
    };
    let cfg = shared.config();
    // A touchpad scrolls in small steps and a wheel in whole notches of 120.
    // debt: tell the touchpad by Raw Input; a free-spinning wheel that sends part notches counts as a touchpad too.
    if foreign(&cfg, m.injected) || m.delta == 0 {
        return Pass;
    }
    let pad = m.delta % 120 != 0;
    let mods = keys::held();
    let sideways = !pad && !horizontal && cfg.sideways == Some(mods);
    // Ctrl with the wheel is zoom, and stays as Windows has it.
    if !sideways && mods & keys::CTRL != 0 {
        return Pass;
    }
    let delta = if sideways {
        // Away from the user is to the left, as Shift with the wheel is in a browser; then the sideways switch.
        if cfg.invert_horizontal { m.delta } else { -m.delta }
    } else if match (pad, horizontal) {
        (false, false) => cfg.invert_vertical,
        (false, true) => cfg.invert_horizontal,
        (true, false) => cfg.pad_vertical,
        (true, true) => cfg.pad_horizontal,
    } {
        -m.delta
    } else {
        return Pass;
    };
    if excepted(shared, &cfg.scroll_exceptions, m, false) {
        return Pass;
    }
    // Not sent (an administrator's window): the turn goes through as it came.
    if svc_input::inject_wheel(delta, sideways || horizontal) { Swallow } else { Pass }
}

// ── The worker ────────────────────────────────────────────────────────────────

fn clipboard_text() -> String {
    svc_inject::clip::open().ok().and_then(|c| c.text(1 << 20)).unwrap_or_default()
}

/// Erases `deletes` characters and puts `body` at the caret of the window in
/// front, then `trailing` as the key it was. A text of several lines is pasted
/// (editors indent typed lines), with the clipboard put back afterwards; if
/// the paste cannot be done it is typed.
pub fn insert(deletes: usize, body: &str, trailing: Option<char>) -> Result<(), String> {
    const REFUSED: &str = "Windows did not take the keys.";
    let tail: String = trailing.map(|c| if c == '\r' { '\n' } else { c }).into_iter().collect();
    let typed = |deletes: usize, text: &str| if svc_input::inject_text(deletes, text) { Ok(()) } else { Err(REFUSED.to_string()) };
    if svc_input::foreground_elevated() {
        return Err(svc_inject::ELEVATED.into());
    }
    if !body.contains('\n') {
        return typed(deletes, &format!("{body}{tail}"));
    }
    typed(deletes, "")?;
    match svc_inject::paste_text(body) {
        Ok(()) => typed(0, &tail),
        Err(_) => typed(0, &format!("{body}{tail}")),
    }
}

/// A snippet's text as it is inserted now.
pub fn expand(text: &str) -> String {
    text::expand(text, &clipboard_text)
}

fn work(jobs: mpsc::Receiver<Job>, sink: Sink) {
    for job in jobs {
        match job {
            Job::Expand { cfg, index, deletes, trailing } => {
                let Some(s) = cfg.snippets.get(index) else { continue };
                match insert(deletes, &expand(&s.text), trailing) {
                    Ok(()) => sink(Event::Expanded { id: s.id.clone() }),
                    // Never what was typed: only that it failed.
                    Err(why) => notch_core::log::line(format!("input-tools: a snippet was not typed: {why}")),
                }
            }
            Job::Keys { cfg, slot } => {
                // The keys the user still holds would join the combination.
                svc_inject::wait_modifiers_up();
                let keys = &cfg.buttons[slot];
                let strokes: Vec<(u16, bool)> = keys.iter().map(|k| (*k, true)).chain(keys.iter().rev().map(|k| (*k, false))).collect();
                svc_input::inject_keys(&strokes);
            }
            Job::Recorded(chord) => {
                if let Some(name) = keys::name(chord) {
                    sink(Event::Recorded(name));
                }
            }
            Job::Stop => break,
        }
    }
}

// ── The engine ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Subs {
    recorder: Option<Sub>,
    key_debounce: Option<Sub>,
    guard: Option<Sub>,
    remap: Option<Sub>,
    snippets: Option<(Sub, Sub)>,
    click_debounce: Option<Sub>,
    buttons: Option<Sub>,
    wheel: Option<Sub>,
}

/// Lives from the feature's `start` to its `stop`. Until `apply` asks for a
/// stage there is no subscription, and so no hook.
pub struct Engine {
    shared: Arc<Shared>,
    subs: Mutex<Subs>,
    worker: Option<JoinHandle<()>>,
    _front: svc_sysevents::Sub,
}

/// The on-screen keyboards: typing with them must not look like leaving the window typed into.
const KEYBOARDS: [&str; 3] = ["osk.exe", "tabtip.exe", "textinputhost.exe"];

fn fronted(shared: &Shared, front: &svc_sysevents::Foreground) {
    let own = front.pid == std::process::id();
    let exe = front.exe.to_lowercase();
    shared.front_own.store(own, Relaxed);
    if !own && front.hwnd != 0 {
        shared.last_front.store(front.hwnd, Relaxed);
    }
    if !KEYBOARDS.contains(&exe.as_str()) {
        shared.reset.store(true, Relaxed);
    }
    shared.guard_live.store(shared.config().guard_apps.contains(&exe), Relaxed);
    *lock(&shared.front_exe) = exe;
    // Going to or from a window that runs as administrator: the release of a
    // remapped key may never be seen there, so nothing stays held across it.
    let elevated = svc_input::foreground_elevated();
    if shared.elevated.swap(elevated, Relaxed) != elevated {
        release_all(shared);
    }
}

impl Engine {
    pub fn start(sink: impl Fn(Event) + Send + Sync + 'static) -> Self {
        let (jobs, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            cfg: RwLock::new(Arc::new(Config::default())),
            jobs,
            key_state: Mutex::new(Box::new([KeySlot::default(); 512])),
            click_state: Mutex::new([Click::default(); 3]),
            guard_state: Mutex::new(Vec::with_capacity(8)),
            remap_state: Mutex::new(Vec::with_capacity(8)),
            typed: Mutex::new(Vec::with_capacity(65)),
            button_state: Mutex::default(),
            under: Mutex::default(),
            recorder: Mutex::default(),
            reset: AtomicBool::new(false),
            front_own: AtomicBool::new(false),
            front_exe: Mutex::default(),
            guard_live: AtomicBool::new(false),
            last_front: AtomicIsize::new(0),
            elevated: AtomicBool::new(false),
        });
        fronted(&shared, &svc_sysevents::foreground());
        let front = {
            let shared = shared.clone();
            svc_sysevents::on_foreground(move |front| fronted(&shared, front))
        };
        let sink: Sink = Arc::new(sink);
        Self { shared, subs: Mutex::default(), worker: Some(std::thread::spawn(move || work(rx, sink))), _front: front }
    }

    /// Takes the configuration and makes the subscriptions what it asks for.
    pub fn apply(&self, cfg: Config) {
        let cfg = Arc::new(cfg);
        *self.shared.cfg.write().unwrap_or_else(|e| e.into_inner()) = cfg.clone();
        let live = cfg.guard_apps.contains(&lock(&self.shared.front_exe));
        self.shared.guard_live.store(live, Relaxed);

        fn set<T>(slot: &mut Option<T>, on: bool, make: impl FnOnce() -> T) {
            if !on {
                *slot = None;
            } else if slot.is_none() {
                *slot = Some(make());
            }
        }
        let key = |order: i32, injects: bool, stage: fn(&Shared, &Key) -> Verdict| {
            let shared = self.shared.clone();
            svc_input::keyboard(order, injects, move |k| stage(&shared, k))
        };
        let mouse = |order: i32, injects: bool, stage: fn(&Shared, &Mouse) -> Verdict| {
            let shared = self.shared.clone();
            svc_input::mouse(order, injects, move |m| stage(&shared, m))
        };
        let mut subs = lock(&self.subs);
        set(&mut subs.key_debounce, cfg.key_debounce, || key(10, false, key_debounce));
        set(&mut subs.guard, !cfg.guard_apps.is_empty() && !cfg.guard_keys.is_empty(), || key(20, false, guard));
        set(&mut subs.remap, !cfg.remaps.is_empty(), || key(30, true, remap));
        set(&mut subs.snippets, cfg.snippets.iter().any(|s| s.on), || (key(40, true, snippets), mouse(15, false, click_resets)));
        set(&mut subs.click_debounce, cfg.click_debounce, || mouse(10, false, click_debounce));
        set(&mut subs.buttons, cfg.buttons.iter().any(|b| !b.is_empty()), || mouse(30, true, buttons));
        set(&mut subs.wheel, cfg.invert_vertical || cfg.invert_horizontal || cfg.pad_vertical || cfg.pad_horizontal || cfg.sideways.is_some(), || mouse(40, true, wheel));
        if subs.remap.is_none() {
            release_all(&self.shared);
        }
    }

    /// The stages that are subscribed right now, by name.
    pub fn stages(&self) -> Vec<&'static str> {
        let subs = lock(&self.subs);
        [
            ("recorder", subs.recorder.is_some()),
            ("keyDebounce", subs.key_debounce.is_some()),
            ("guard", subs.guard.is_some()),
            ("remaps", subs.remap.is_some()),
            ("snippets", subs.snippets.is_some()),
            ("clickDebounce", subs.click_debounce.is_some()),
            ("buttons", subs.buttons.is_some()),
            ("wheel", subs.wheel.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect()
    }

    /// Hears the next combination (ten seconds at most) and tells it as
    /// `Event::Recorded`. Every key is swallowed meanwhile, so the Windows key,
    /// Alt+F4 and keys that are remapped can be recorded like any other.
    pub fn record(&self) {
        *lock(&self.shared.recorder) = Recorder { until: svc_input::now() + 10_000_000, done: 0, mods: 0 };
        let mut subs = lock(&self.subs);
        if subs.recorder.is_none() {
            let shared = self.shared.clone();
            subs.recorder = Some(svc_input::keyboard(0, false, move |k| recorder(&shared, k)));
        }
    }

    pub fn record_stop(&self) {
        lock(&self.shared.recorder).until = 0;
        lock(&self.subs).recorder = None;
    }

    /// A blocked combination would be swallowed in the window in front.
    pub fn guard_live(&self) -> bool {
        self.shared.guard_live.load(Relaxed)
    }

    /// The last window in front that was not one of the app's own.
    pub fn last_front(&self) -> isize {
        self.shared.last_front.load(Relaxed)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // The subscriptions first: with the last one the hooks are gone.
        *lock(&self.subs) = Subs::default();
        release_all(&self.shared);
        self.shared.job(Job::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
