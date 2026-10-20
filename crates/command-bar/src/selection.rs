// The bar beside selected text: after the mouse button goes up on a drag or a
// double click, the text selected in the app in front is read (UI Automation
// TextPattern, on a worker thread, never in a password field) and a small
// window that never takes focus appears next to the pointer. It acts only on a
// click, and the text is held here and in the bar's page, nowhere else.
//
// Nothing runs while the option is off: the mouse hook exists only while the
// watch does, and the worker ends when the watch is dropped.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notch_core::{Ctx, Monitor, WindowSpec};
use serde_json::json;
use svc_input::{Button, MouseKind, Sub, Verdict};
use tauri::{Manager, PhysicalPosition};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId};

use crate::ID;

pub const LABEL: &str = "selection-bar";
/// Logical pixels.
const SIZE: (f64, f64) = (176.0, 40.0);
/// A drag shorter than this (pixels) is a click.
const DRAG: i32 = 8;
/// Longer selections are not offered: the bar is for words and links.
const LONGEST: i32 = 4000;

enum Msg {
    Try(i32, i32),
    Hide(i32, i32),
}

/// Alive while the option is on.
pub struct Watch {
    _sub: Sub,
    _tx: Sender<Msg>,
    pub text: Arc<Mutex<String>>,
}

pub fn watch(cx: &Ctx) -> Watch {
    let (tx, rx) = channel::<Msg>();
    let text = Arc::new(Mutex::new(String::new()));
    let (cx2, text2) = (cx.clone(), text.clone());
    std::thread::spawn(move || work(cx2, rx, text2));
    let sender = Mutex::new(tx.clone());
    // (where the left button went down, when and where it last came up)
    let seen = Mutex::new(((0, 0), None::<(Instant, i32, i32)>));
    let sub = svc_input::mouse(50, false, move |m| {
        // Runs on the hook thread: a few comparisons and a send, nothing else.
        if let MouseKind::Down(Button::Left) | MouseKind::Up(Button::Left) = m.kind {
            let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
            let send = |msg| {
                let _ = sender.lock().unwrap_or_else(|e| e.into_inner()).send(msg);
            };
            if m.kind == MouseKind::Down(Button::Left) {
                s.0 = (m.x, m.y);
                send(Msg::Hide(m.x, m.y));
            } else {
                let dragged = (m.x - s.0 .0).abs() > DRAG || (m.y - s.0 .1).abs() > DRAG;
                let twice = s.1.is_some_and(|(t, x, y)| t.elapsed() < Duration::from_millis(450) && (x - m.x).abs() <= DRAG && (y - m.y).abs() <= DRAG);
                s.1 = Some((Instant::now(), m.x, m.y));
                if dragged || twice {
                    send(Msg::Try(m.x, m.y));
                }
            }
        }
        Verdict::Pass
    });
    Watch { _sub: sub, _tx: tx, text }
}

/// The text selected in the control that has the keyboard, or None (nothing selected, a password field, no TextPattern).
fn selected(auto: &IUIAutomation) -> Option<String> {
    unsafe {
        let el = auto.GetFocusedElement().ok()?;
        if el.CurrentIsPassword().ok()?.as_bool() {
            return None;
        }
        let pattern: IUIAutomationTextPattern = el.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
        let ranges = pattern.GetSelection().ok()?;
        if ranges.Length().ok()? < 1 {
            return None;
        }
        let text = ranges.GetElement(0).ok()?.GetText(LONGEST).ok()?.to_string();
        (!text.trim().is_empty()).then_some(text)
    }
}

fn work(cx: Ctx, rx: Receiver<Msg>, text: Arc<Mutex<String>>) {
    let auto: Option<IUIAutomation> = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
    };
    let Some(auto) = auto else { return };
    // Where the bar is on screen now (physical pixels), to leave clicks on it alone.
    let mut rect: Option<(i32, i32, i32, i32)> = None;
    let inside = |r: &Option<(i32, i32, i32, i32)>, x: i32, y: i32| r.is_some_and(|(l, t, w, h)| x >= l && x < l + w && y >= t && y < t + h);
    // Ends when the watch is dropped and the sender goes.
    while let Ok(first) = rx.recv() {
        let mut msg = first;
        // The selection settles a moment after the button; the last thing said is the one that counts.
        if matches!(msg, Msg::Try(..)) {
            std::thread::sleep(Duration::from_millis(140));
        }
        while let Ok(next) = rx.try_recv() {
            msg = next;
        }
        match msg {
            Msg::Hide(x, y) => {
                if rect.is_some() && !inside(&rect, x, y) {
                    hide(&cx, &text);
                    rect = None;
                }
            }
            Msg::Try(x, y) => {
                if inside(&rect, x, y) {
                    continue;
                }
                match selected(&auto) {
                    Some(found) => {
                        *text.lock().unwrap_or_else(|e| e.into_inner()) = found.clone();
                        rect = show(&cx, x, y);
                        cx.emit(ID, "selection", json!({ "text": found }));
                    }
                    None if rect.is_some() => {
                        hide(&cx, &text);
                        rect = None;
                    }
                    None => {}
                }
            }
        }
    }
}

pub fn hide(cx: &Ctx, text: &Mutex<String>) {
    text.lock().unwrap_or_else(|e| e.into_inner()).clear();
    if let Some(win) = cx.app().get_webview_window(LABEL) {
        let _ = win.hide();
    }
}

/// Above the pointer, or below it near the top of the screen; never taking focus.
fn show(cx: &Ctx, x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
    let spec = WindowSpec {
        label: LABEL.into(),
        feature: ID.into(),
        page: "selection".into(),
        w: SIZE.0,
        h: SIZE.1,
        frameless: true,
        topmost: true,
        no_activate: true,
        transparent: true,
        exclude_from_capture: true,
        monitor: Monitor::Cursor,
    };
    let win = cx.window().open(spec).ok()?;
    let scale = win.scale_factor().unwrap_or(1.0);
    let (w, h) = ((SIZE.0 * scale) as i32, (SIZE.1 * scale) as i32);
    let gap = (14.0 * scale) as i32;
    let top = if y - h - gap < 0 { y + gap * 2 } else { y - h - gap };
    let left = (x - w / 2).max(0);
    let _ = win.set_position(PhysicalPosition::new(left, top));
    Some((left, top, w, h))
}
