// Media — now playing on the island (docs/ARCHITECTURE.md §2). The contract:
// nothing runs before `start`, everything is gone after `stop`, and every
// command of the feature arrives through `call`.
//
// All the work is svc-media's: this holds one subscription to it while the
// feature is on, forwards each change to the page as `media:state`, and hands
// the page's clicks to the player. The player the user prefers is kept in the
// feature's settings and given to the service at `start`, so it is remembered.

use std::sync::Mutex;

use base64::Engine;
use notch_core::{secrets, Ctx, Feature, Group, Manifest};
use serde_json::{json, Value};
use svc_media::{Playback, Repeat};

static MANIFEST: Manifest = Manifest {
    id: "media",
    name: "Now playing",
    group: Group::Media,
    default_on: true,
    uses: &[],
    shortcuts: &[],
    tray: &[],
    // `player`: the preferred player's id, "" = whoever plays; `playerName` is what to call it while it is closed.
    defaults: r#"{ "trackNotice": true, "artTint": true, "artGlow": true, "player": "", "playerName": "", "fallback": false }"#,
};

#[derive(Default)]
pub struct Media {
    /// The subscription keeps the service's worker alive: held from `start` to `stop`.
    sub: Mutex<Option<svc_media::Sub>>,
}

fn track(p: &Playback) -> Value {
    json!({
        "source": { "aumid": p.source_aumid, "name": p.source_name },
        // Not the preferred player: the one that stands in for it while it is quiet.
        "fallback": p.fallback,
        "title": p.title,
        "artist": p.artist,
        "album": p.album,
        // The cover itself is fetched with `art` when this number changes; 0 = none.
        "art": p.art_rev,
        "playing": p.playing,
        "position": p.position,
        "duration": p.duration,
        "rate": p.rate,
        "at": p.at_ms,
        "canPlayPause": p.can_play_pause,
        "canNext": p.can_next,
        "canPrev": p.can_prev,
        "canSeek": p.can_seek,
        "canShuffle": p.can_shuffle,
        "canRepeat": p.can_repeat,
        "shuffle": p.shuffle,
        "repeat": match p.repeat {
            Repeat::Off => "off",
            Repeat::Track => "track",
            Repeat::List => "list",
        },
    })
}

/// What the page draws from: the track, every player, and the one the user picked.
fn snapshot() -> Value {
    json!({
        "track": svc_media::state().as_ref().map(track),
        "sources": svc_media::sources()
            .iter()
            .map(|s| json!({ "aumid": s.aumid, "name": s.name, "title": s.title, "playing": s.playing }))
            .collect::<Vec<_>>(),
        "picked": svc_media::picked(),
    })
}

/// From the settings to the service: the preferred player and whether another
/// may stand in for it while it is quiet.
fn apply(settings: &Value) {
    let player = settings["player"].as_str().filter(|id| !id.is_empty()).map(str::to_owned);
    if svc_media::picked() != player {
        svc_media::pick_source(player);
    }
    svc_media::set_fallback(settings["fallback"].as_bool() == Some(true));
}

impl Feature for Media {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let mut sub = self.sub.lock().unwrap_or_else(|e| e.into_inner());
        if sub.is_none() {
            // The Spotify sign-in is gone: a token saved by an earlier build is removed (a few bytes in Credential Manager).
            for key in ["media/spotify-client-id", "media/spotify-refresh-token"] {
                let _ = secrets::clear(key);
            }
            // Before the first reading: the player preferred last time is preferred from the start.
            apply(&cx.settings("media"));
            let cx = cx.clone();
            *sub = Some(svc_media::subscribe(move |_| cx.emit("media", "state", snapshot())));
        }
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        self.sub.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    fn settings_changed(&self, _cx: &Ctx, settings: &Value) {
        apply(settings);
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        match method {
            "state" => return Ok(snapshot()),
            // The cover numbered `rev`, the number the page was told with the
            // track: null once the track has moved on, so a cover that arrives
            // late is never shown on the next song.
            "art" => {
                let url = svc_media::art(args["rev"].as_u64().unwrap_or(0)).map(|(bytes, mime)| {
                    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes.as_slice()))
                });
                return Ok(json!(url));
            }
            "play" => svc_media::play(),
            "pause" => svc_media::pause(),
            "playPause" => svc_media::play_pause(),
            "next" => svc_media::next(),
            "prev" => svc_media::prev(),
            "seek" => svc_media::seek(args["position"].as_f64().ok_or("seek needs a position")?),
            "shuffle" => svc_media::shuffle(args["on"].as_bool().unwrap_or(false)),
            "repeat" => svc_media::repeat(match args["mode"].as_str() {
                Some("track") => Repeat::Track,
                Some("list") => Repeat::List,
                _ => Repeat::Off,
            }),
            // Kept in the settings, which hand it to the service (`settings_changed`)
            // now and at every start; null goes back to whoever plays.
            "pick" => {
                let aumid = args["aumid"].as_str().unwrap_or_default();
                let name = svc_media::sources().into_iter().find(|s| s.aumid == aumid).map(|s| s.name);
                cx.patch_settings("media", json!({ "player": aumid, "playerName": name.unwrap_or_default() }));
            }
            "open" => svc_media::activate_player(),
            _ => return Err("unknown method".into()),
        }
        Ok(Value::Null)
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/media/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(Media::default())
}
