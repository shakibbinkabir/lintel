// Notifications and accessories — a Lintel feature. The contract is docs/ARCHITECTURE.md §2:
// nothing runs before `start`, everything is gone after `stop`, and every
// command of the feature arrives through `call`.
//
// Two halves, each with its own switch in the settings:
// - accessories (`bt`): a Bluetooth device watcher. While it is on it is the
//   only thing that runs, and it only waits for Windows to call.
// - notifications (`mirror`): the toasts of other apps. Off by default. The
//   Windows listener needs a signed, packaged app (identity package, Windows'
//   question); where it is not allowed, `toast` reads the pop-ups as they show.
//   What is missing is a state the page shows, never an error.
//
// Events: `accessory` { name, kind, audio, connected, battery: { percent, left, right } | null, tone },
// `notification` { id, app, aumid, title, body }, `mirror` { state, registered, error? } with
// state off | identity | restart | consent | denied | unavailable | popups | on.

use std::sync::{Arc, Mutex};

use notch_core::{Ctx, Feature, Group, Manifest};
use serde_json::{json, Value};

pub mod bt;
pub mod mirror;
pub mod toast;

const ID: &str = "notifications-accessories";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Notifications and accessories",
    group: Group::Island,
    default_on: true,
    uses: &[],
    shortcuts: &[],
    tray: &[],
    defaults: r#"{ "accessories": true, "notifications": false }"#,
};

/// What is reading notifications now: the Windows listener where Windows allows it, otherwise the pop-up reader.
#[derive(Default)]
pub struct Held {
    listener: Option<mirror::Listener>,
    popups: Option<toast::Watch>,
}

type Slot = Arc<Mutex<Held>>;

#[derive(Default)]
pub struct NotificationsAccessories {
    watch: Mutex<Option<bt::Watch>>,
    mirror: Slot,
}

pub fn device_json(d: &bt::Device) -> Value {
    json!({
        "name": d.name,
        "kind": d.kind,
        "audio": d.kind.is_audio(),
        "connected": d.connected,
        "battery": d.battery,
        "tone": d.battery.and_then(|b| b.tone()),
    })
}

/// What the log says of a connect or a disconnect: the name and the battery, never the address.
pub fn log_line(d: &bt::Device) -> String {
    let battery = d.battery.and_then(|b| b.lowest()).map_or("no battery reported".into(), |p| format!("battery {p}%"));
    if d.connected { format!("{} connected, {battery}", d.name) } else { format!("{} disconnected", d.name) }
}

fn wants_mirror(cx: &Ctx) -> bool {
    cx.settings(ID)["notifications"].as_bool().unwrap_or(false)
}

/// Where the mirror stands, for the page. Windows is asked nothing while the mirror is not wanted.
fn mirror_json(wanted: bool, held: &Held) -> Value {
    let state = match (wanted, held.listener.is_some(), held.popups.is_some()) {
        (false, ..) => "off",
        (true, true, _) => "on",
        // Read as they pop up: no signing, no identity.
        (true, false, true) => "popups",
        (true, false, false) => "unavailable",
    };
    json!({ "state": state, "registered": wanted && mirror::registered() })
}

/// The listener follows the setting and what Windows allows: there only while
/// both say yes. Where it is not allowed (every unsigned build), the pop-up
/// reader takes its place. Never asks for anything. Tells the page where things stand.
fn sync_mirror(cx: &Ctx, slot: &Slot, wanted: bool) {
    let mut held = slot.lock().unwrap();
    if !wanted {
        *held = Held::default();
    } else if held.listener.is_none() && mirror::missing().is_none() {
        let cx = cx.clone();
        held.listener = mirror::listen(move |toast| cx.emit(ID, "notification", toast)).ok();
    }
    if held.listener.is_some() {
        held.popups = None;
    } else if wanted && held.popups.is_none() {
        let cx = cx.clone();
        match toast::watch(move |toast| cx.emit(ID, "notification", toast)) {
            Ok(watch) => {
                notch_core::log::line(format!("{ID}: reading pop-up notifications"));
                held.popups = Some(watch);
            }
            Err(err) => notch_core::log::line(format!("{ID}: no pop-up reader: {err}")),
        }
    }
    let status = mirror_json(wanted, &held);
    drop(held);
    cx.emit(ID, "mirror", status);
}

impl NotificationsAccessories {
    fn sync(&self, cx: &Ctx, settings: &Value) {
        let mut watch = self.watch.lock().unwrap();
        if !settings["accessories"].as_bool().unwrap_or(true) {
            watch.take();
        } else if watch.is_none() {
            let cx = cx.clone();
            let told = move |device: bt::Device| {
                notch_core::log::line(format!("{ID}: {}", log_line(&device)));
                cx.emit(ID, "accessory", device_json(&device));
            };
            match bt::watch(told) {
                Ok(started) => *watch = Some(started),
                // A PC without Bluetooth: no accessory notices, quietly.
                Err(err) => notch_core::log::line(format!("{ID}: no Bluetooth watcher: {}", err.message())),
            }
        }
        drop(watch);
        sync_mirror(cx, &self.mirror, settings["notifications"].as_bool().unwrap_or(false));
    }

    fn status(&self, cx: &Ctx, error: Option<String>) -> Value {
        let mut status = mirror_json(wants_mirror(cx), &self.mirror.lock().unwrap());
        status["error"] = json!(error);
        status
    }
}

impl Feature for NotificationsAccessories {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        self.sync(cx, &cx.settings(ID));
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        self.watch.lock().unwrap().take();
        *self.mirror.lock().unwrap() = Held::default();
    }

    fn settings_changed(&self, cx: &Ctx, settings: &Value) {
        self.sync(cx, settings);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        match method {
            // The accessories connected now, each with a battery read afresh.
            "devices" => {
                let devices = bt::devices().unwrap_or_default();
                Ok(devices.iter().filter(|d| d.connected).map(device_json).collect())
            }
            "mirror" => Ok(self.status(cx, None)),
            // The user's click in Settings: the next step that is missing, and only that one.
            "setup" => {
                if !wants_mirror(cx) {
                    return Ok(self.status(cx, None));
                }
                let mut error = None;
                match mirror::missing() {
                    Some("identity") => error = mirror::register(&cx.data_dir(ID)).err(),
                    // Windows asks on the UI thread; the answer comes back as a `mirror` event.
                    Some("consent") => {
                        let (cx, slot) = (cx.clone(), self.mirror.clone());
                        let app = cx.app().clone();
                        let _ = app.run_on_main_thread(move || {
                            let done = cx.clone();
                            let asked = mirror::request(move |_| sync_mirror(&done, &slot, wants_mirror(&done)));
                            if let Err(err) = asked {
                                cx.emit(ID, "mirror", json!({ "state": "consent", "registered": true, "error": err.message() }));
                            }
                        });
                    }
                    _ => sync_mirror(cx, &self.mirror, wants_mirror(cx)),
                }
                Ok(self.status(cx, error))
            }
            "remove" => {
                self.mirror.lock().unwrap().listener = None;
                Ok(self.status(cx, mirror::unregister().err()))
            }
            // A click on a mirrored notification: the app that sent it comes forward.
            "open" => {
                if let Some(app) = args["app"].as_str().filter(|_| args["aumid"].as_str().unwrap_or_default().is_empty()) {
                    return if toast::open_app(app) { Ok(Value::Null) } else { Err("that app is not running".into()) };
                }
                let aumid = args["aumid"].as_str().unwrap_or_default();
                if aumid.is_empty() || aumid.chars().any(|c| c == '"' || c.is_control()) {
                    return Err("no app to open".into());
                }
                // The apps folder opens an app by its id and nothing else.
                notch_core::win::reveal_folder(&format!("shell:AppsFolder\\{aumid}"));
                Ok(Value::Null)
            }
            "privacy" => {
                notch_core::win::open_url("ms-settings:privacy-notifications");
                Ok(Value::Null)
            }
            "bluetooth" => {
                notch_core::win::open_url("ms-settings:bluetooth");
                Ok(Value::Null)
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/notifications-accessories/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(NotificationsAccessories::default())
}
