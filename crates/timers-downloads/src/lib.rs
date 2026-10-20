// Timer — a Lintel feature (the id stays timers-downloads). Nothing runs before `start`;
// a timer exists only while one is running (one parked thread), and `stop` leaves nothing behind.

pub mod clock;
mod power;

use clock::Clock;
use notch_core::{Ctx, Feature, Group, Manifest, ShortcutDef};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use svc_sysevents::Session;

static MANIFEST: Manifest = Manifest {
    id: "timers-downloads",
    name: "Timer",
    group: Group::Island,
    default_on: true,
    uses: &[],
    shortcuts: &[ShortcutDef { id: "toggle", label: "Start, pause or resume the timer", default: None }],
    tray: &[],
    // auto: start the next Pomodoro phase by itself
    defaults: r#"{ "sound": true, "auto": false, "mode": "timer", "minutes": 25, "focus": 25, "short": 5, "long": 15, "every": 4, "sessions": 4 }"#,
};

pub struct TimersDownloads {
    power: Arc<power::Power>,
    run: Mutex<Option<(Clock, svc_sysevents::Sub)>>,
}

impl Feature for TimersDownloads {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let c = Clock::new(Arc::new({
            let cx = cx.clone();
            move |ev, v| cx.emit("timers-downloads", ev, v)
        }));
        // pause on sleep; the person resumes
        let sub = svc_sysevents::on_session({
            let c = c.clone();
            move |e| {
                if e == Session::Sleep {
                    c.pause();
                }
            }
        });
        *self.run.lock().unwrap() = Some((c, sub));
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        if let Some((c, _sub)) = self.run.lock().unwrap().take() {
            c.stop();
        }
        self.power.cancel(cx);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        // The power timer stands apart from the clock.
        match method {
            "powerState" => return Ok(self.power.json()),
            "powerArm" => return self.power.arm(cx, args["action"].as_str().unwrap_or(""), args["at"].as_u64().unwrap_or(0)),
            "powerCancel" => {
                self.power.cancel(cx);
                return Ok(Value::Null);
            }
            // Shutdown runs only from the click on the page's question.
            "powerNow" => {
                power::run(if args["action"].as_str() == Some("shutdown") { "shutdown" } else { "sleep" });
                return Ok(Value::Null);
            }
            _ => {}
        }
        let clock = self.run.lock().unwrap().as_ref().map(|r| r.0.clone()).ok_or("turned off")?;
        // the saved settings fill what a start leaves out (a shortcut start, the auto option)
        let start = |args: &Value| {
            let mut a = cx.settings("timers-downloads");
            if let (Some(o), Some(m)) = (args.as_object(), a.as_object_mut()) {
                m.extend(o.clone());
            }
            clock.start(&a)
        };
        Ok(match method {
            "state" => json!({ "clock": clock.snapshot() }),
            "start" => start(&args),
            "pause" => clock.pause(),
            "resume" => clock.resume(),
            "skip" => clock.skip(),
            "lap" => clock.lap(),
            "cancel" => clock.cancel(),
            "shortcut" => match (args["id"].as_str(), clock.snapshot()["status"].as_str()) {
                (Some("toggle"), Some("running")) => clock.pause(),
                (Some("toggle"), Some("paused")) => clock.resume(),
                (Some("toggle"), _) => start(&json!({})),
                _ => return Err("unknown method".into()),
            },
            _ => return Err("unknown method".into()),
        })
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/timers-downloads/strings.json")
    }
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(TimersDownloads { run: Mutex::new(None), power: Arc::default() })
}
