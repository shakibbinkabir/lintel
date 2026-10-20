// A power timer: sleep at a time, or ask to shut down at a time. One parked thread while armed.
// Arming is the click for sleep; shutdown only raises "powerDue" and the page asks (a click runs it).

use notch_core::Ctx;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ID: &str = "timers-downloads";

#[derive(Default)]
pub struct Power {
    cur: Mutex<Option<(String, u64)>>,
    turn: Arc<AtomicU64>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Power {
    pub fn json(&self) -> Value {
        match &*self.cur.lock().unwrap() {
            Some((a, at)) => json!({ "action": a, "at": at }),
            None => json!({ "action": null }),
        }
    }

    pub fn cancel(&self, cx: &Ctx) {
        self.turn.fetch_add(1, Relaxed);
        if self.cur.lock().unwrap().take().is_some() {
            cx.emit(ID, "power", self.json());
        }
    }

    pub fn arm(self: &Arc<Self>, cx: &Ctx, action: &str, at: u64) -> Result<Value, String> {
        if !matches!(action, "sleep" | "shutdown") {
            return Err("unknown action".into());
        }
        if at <= now_ms() || at > now_ms() + 24 * 3_600_000 {
            return Err("Pick a time within the next 24 hours.".into());
        }
        let mine = self.turn.fetch_add(1, Relaxed) + 1;
        *self.cur.lock().unwrap() = Some((action.to_string(), at));
        cx.emit(ID, "power", self.json());
        let (me, cx, action) = (self.clone(), cx.clone(), action.to_string());
        std::thread::spawn(move || {
            // Wall clock, in short waits: a sleep of the PC itself cannot stretch it.
            while now_ms() < at {
                std::thread::sleep(Duration::from_millis((at - now_ms()).min(15_000)));
                if me.turn.load(Relaxed) != mine {
                    return;
                }
            }
            if me.turn.load(Relaxed) != mine {
                return;
            }
            me.cur.lock().unwrap().take();
            cx.emit(ID, "power", me.json());
            if action == "sleep" {
                run("sleep");
            } else {
                cx.emit(ID, "powerDue", json!({ "action": action }));
            }
        });
        Ok(self.json())
    }
}

/// Runs the action now. Called for sleep at its time, and for shutdown only after the person's click.
pub fn run(action: &str) {
    if action == "sleep" {
        unsafe { windows::Win32::System::Power::SetSuspendState(false, false, false) };
    } else {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("shutdown").args(["/s", "/t", "0"]).creation_flags(0x0800_0000).spawn();
    }
}
