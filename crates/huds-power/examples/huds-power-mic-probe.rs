// Proves the microphone-in-use detector (src/mic.rs) without the app:
// `cargo run -p notch-feat-huds-power --example huds-power-mic-probe -- other` (or `self`).
//
// A child process of its own opens the default microphone for three seconds while the watcher
// runs here, and what the watcher reports is printed with the time: before, during and after.
// The child starts a shared-mode capture stream and never reads it: no sample leaves the audio
// engine, nothing is saved, nothing is played.
//   other  the child is a copy of this program under another name, as any app would be:
//          it must be announced, and let go when it ends.
//   self   the child is this very program, as Lintel's own dictation is Lintel's:
//          it must not be announced, though the registry shows it holding the microphone.

use std::process::Command;
use std::time::{Duration, Instant};

use notch_feat_huds_power::mic;
use windows::Win32::Media::Audio::{eCapture, eConsole, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

/// How long the child holds the microphone: three seconds, or `MIC_HOLD_SECS`.
fn hold() -> Duration {
    Duration::from_secs(std::env::var("MIC_HOLD_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3))
}

/// The child: the default microphone, open and running for `hold()`, its samples never read.
fn hold_microphone() -> windows::core::Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let client: IAudioClient = devices.GetDefaultAudioEndpoint(eCapture, eConsole)?.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let opened = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 10_000_000, 0, format, None);
        CoTaskMemFree(Some(format as *const _));
        opened?;
        client.Start()?;
        std::thread::sleep(hold());
        client.Stop()
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "--hold" {
        if let Err(err) = hold_microphone() {
            eprintln!("child: the microphone did not open: {err}");
            std::process::exit(1);
        }
        return;
    }
    let me = std::env::current_exe().expect("this program's path");
    let child = match mode.as_str() {
        "other" => {
            let copy = me.with_file_name("huds-power-mic-holder.exe");
            std::fs::copy(&me, &copy).expect("a copy of this program");
            copy
        }
        "self" => me.clone(),
        _ => return println!("usage: huds-power-mic-probe other | self"),
    };

    let start = Instant::now();
    let at = move || format!("{:5.2}s", start.elapsed().as_secs_f32());
    let read = |when: &str| println!("{} {when}: announced {:?}, with this program {:?}", at(), mic::in_use(), mic::everyone());

    read("before");
    // The watcher as the app runs it: one line whenever the list changes (and once at the start).
    let watch = mic::watch(move |apps, cam| println!("{:5.2}s detector: {apps:?} camera {cam:?}", start.elapsed().as_secs_f32()));
    std::thread::sleep(Duration::from_millis(500));

    println!("{} child starts ({mode}): {}", at(), child.display());
    let mut held = Command::new(&child).arg("--hold").spawn().expect("the child");
    for _ in 0..2 {
        std::thread::sleep(Duration::from_secs(1));
        read("during");
    }
    let ended = held.wait().map(|s| s.code());
    println!("{} child ended: {ended:?}", at());
    std::thread::sleep(Duration::from_secs(1));
    read("after");

    drop(watch);
    if child != me {
        let _ = std::fs::remove_file(&child);
    }
    println!("{} watcher dropped", at());
}
