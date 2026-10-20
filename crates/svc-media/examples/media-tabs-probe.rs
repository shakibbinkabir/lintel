// Two sessions of one app, each playing, as the service sees them:
//
//   cargo run -p svc-media --example media-tabs-probe
//
// Part 1 starts Brave with a profile folder of its own in the temp folder and two
// local pages ("Brave page A", "Brave page B") that loop a quiet tone, and prints
// the sessions Windows reports for it (Chromium reports one session for the whole
// browser, the page it plays last: nothing to key by). The browser is closed after.
// Part 2 is the proof: two silent sessions with one app id in this process, as a
// browser that gave a session per tab would. The service must list both with
// different keys and titles, make each one the followed player, and a pause/play
// must reach that session only. Commands go only to those two sessions.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use svc_media::Source;
use windows::core::HSTRING;
use windows::Foundation::TypedEventHandler;
use windows::Media::Playback::MediaPlayer;
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls, SystemMediaTransportControlsButton as Button,
    SystemMediaTransportControlsButtonPressedEventArgs,
};

const BRAVE: &str = r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe";

fn wav() -> Vec<u8> {
    // 10 s (Chromium ignores media under 5 s), 8 kHz, mono, 8-bit: a 440 Hz tone at a tenth of full scale.
    let data: Vec<u8> = (0..80000).map(|i| (128.0 + 12.0 * (i as f64 * 440.0 * std::f64::consts::TAU / 8000.0).sin()) as u8).collect();
    let mut b = b"RIFF".to_vec();
    b.extend((36 + data.len() as u32).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend([1, 0, 1, 0]);
    b.extend(8000u32.to_le_bytes());
    b.extend(8000u32.to_le_bytes());
    b.extend([1, 0, 8, 0]);
    b.extend(b"data");
    b.extend((data.len() as u32).to_le_bytes());
    b.extend(data);
    b
}

fn tabs(list: &[Source]) -> Vec<Source> {
    list.iter().filter(|s| s.title.starts_with("Lintel tab")).cloned().collect()
}

fn wait(what: &str, secs: u64, mut ok: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if ok() {
            return true;
        }
        if end.saturating_duration_since(Instant::now()).as_millis() % 5000 < 100 {
            println!("  .. {:?}", svc_media::sources().iter().map(|s| (&s.name, &s.title, s.playing)).collect::<Vec<_>>());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("  gave up waiting for: {what}");
    false
}

fn main() {
    let dir: PathBuf = std::env::temp_dir().join(format!("media-tabs-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tone.wav"), wav()).unwrap();
    let mut urls = Vec::new();
    for (name, letter) in [("a", "A"), ("b", "B")] {
        let page = format!(
            "<title>Brave page {letter}</title><audio src=\"tone.wav\" loop autoplay></audio><p>tab {letter}</p>"
        );
        std::fs::write(dir.join(format!("{name}.html")), page).unwrap();
        urls.push(format!("file:///{}", dir.join(format!("{name}.html")).to_string_lossy().replace('\\', "/")));
    }
    let profile = dir.join("profile");

    let _sub = svc_media::subscribe(|_| {});
    let mut child = Command::new(BRAVE)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--autoplay-policy=no-user-gesture-required", "--new-window"])
        .arg(&urls[0])
        .spawn()
        .expect("brave starts");
    // The second page in a window of its own, through the instance that runs: a tab behind another may not start by itself.
    std::thread::sleep(Duration::from_secs(6));
    let _ = Command::new(BRAVE)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--autoplay-policy=no-user-gesture-required", "--new-window"])
        .arg(&urls[1])
        .status();

    let mut pass = true;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        pass &= ok;
    };

    wait("a page of Brave", 40, || svc_media::sources().iter().any(|s| s.title.starts_with("Brave page")));
    println!("Part 1, what Windows reports for the browser with two pages playing:");
    for s in svc_media::sources().iter().filter(|s| s.title.starts_with("Brave page")) {
        println!("  key={:?} app={:?} title={:?} playing={}", s.aumid, s.name, s.title, s.playing);
    }
    close_browser(&mut child);

    println!("Part 2, two sessions of one app:");
    let (one, two) = (fake("Lintel tab A"), fake("Lintel tab B"));
    wait("two sessions", 20, || tabs(&svc_media::sources()).len() == 2);
    let list = svc_media::sources();
    for s in &list {
        println!("  key={:?} app={:?} title={:?} playing={}", s.aumid, s.name, s.title, s.playing);
    }
    let mine = tabs(&list);
    check(mine.len() == 2 && mine[0].aumid != mine[1].aumid && mine[0].name == mine[1].name, "two sessions of one app are two sources with different keys");

    if let [a, b] = mine.as_slice() {
        let shown = |key: &str| svc_media::state().is_some_and(|p| p.source_aumid == key);
        let playing = |key: &str| svc_media::sources().iter().find(|s| s.aumid == key).is_some_and(|s| s.playing);
        for (this, other, label) in [(a, b, "A"), (b, a, "B")] {
            println!("→ pick_source({:?}) = tab {label}", this.aumid);
            svc_media::pick_source(Some(this.aumid.clone()));
            check(wait("the tab is shown", 10, || shown(&this.aumid)), &format!("tab {label} is the shown player"));
            println!("  shown: {:?}", svc_media::state().map(|p| (p.title, p.source_name)));
            svc_media::pause();
            check(wait("paused", 10, || !playing(&this.aumid)), &format!("pause paused tab {label}"));
            check(playing(&other.aumid), &format!("the other tab kept playing"));
            svc_media::play();
            check(wait("playing", 10, || playing(&this.aumid)), &format!("play started tab {label} again"));
        }
        svc_media::pick_source(None);
    }

    drop((one, two));
    std::thread::sleep(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&dir);
    println!("{}", if pass { "ALL PASS" } else { "SOME FAILED" });
}

/// The browser's processes carry the profile folder in their command line.
fn close_browser(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!(
            "Get-CimInstance Win32_Process -Filter \"Name='brave.exe'\" | Where-Object {{ $_.CommandLine -like '*media-tabs-probe-{}*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}",
            std::process::id()
        )])
        .status();
}

/// A silent session that plays `title`; pause and play from the system reach it and change its status.
fn fake(title: &str) -> MediaPlayer {
    let player = MediaPlayer::new().expect("a player");
    player.CommandManager().unwrap().SetIsEnabled(false).unwrap();
    let smtc = player.SystemMediaTransportControls().unwrap();
    smtc.SetIsEnabled(true).unwrap();
    smtc.SetIsPlayEnabled(true).unwrap();
    smtc.SetIsPauseEnabled(true).unwrap();
    let display = smtc.DisplayUpdater().unwrap();
    display.SetType(MediaPlaybackType::Music).unwrap();
    display.MusicProperties().unwrap().SetTitle(&HSTRING::from(title)).unwrap();
    display.Update().unwrap();
    smtc.SetPlaybackStatus(MediaPlaybackStatus::Playing).unwrap();
    smtc.ButtonPressed(&TypedEventHandler::<SystemMediaTransportControls, SystemMediaTransportControlsButtonPressedEventArgs>::new(
        |smtc, args| {
            if let (Some(smtc), Some(args)) = (smtc.as_ref(), args.as_ref()) {
                match args.Button()? {
                    Button::Play => smtc.SetPlaybackStatus(MediaPlaybackStatus::Playing)?,
                    Button::Pause => smtc.SetPlaybackStatus(MediaPlaybackStatus::Paused)?,
                    _ => {}
                }
            }
            Ok(())
        },
    ))
    .unwrap();
    player
}
