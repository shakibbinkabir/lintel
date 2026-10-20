// `cargo run -p notch-feat-notifications-accessories --example notifications-toast-probe`
// Raises test toasts from PowerShell (its own app id), prints the UIA tree of
// the toast window the first time, and what the reader captured. Counts and
// field names only for anything that is not the probe's own text.

use std::process::Command;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use notch_feat_notifications_accessories::toast;

const SCRIPT: &str = r#"
[void][Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
[void][Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime]
$xml = New-Object Windows.Data.Xml.Dom.XmlDocument
$xml.LoadXml('<toast><visual><binding template="ToastGeneric"><text>PROBE-TITLE-{r}</text><text>PROBE-BODY-ONE</text><text>PROBE-BODY-TWO</text></binding></visual></toast>')
$id = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($id).Show([Windows.UI.Notifications.ToastNotification]::new($xml))
"#;

fn cpu_ms() -> u128 {
    // Process CPU time in 100 ns units since start, via PowerShell-free std: read it from the OS.
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Ft(u32, u32);
    extern "system" {
        fn GetProcessTimes(h: *mut core::ffi::c_void, c: *mut Ft, e: *mut Ft, k: *mut Ft, u: *mut Ft) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }
    let _ = std::io::stdout().as_raw_handle();
    let (mut c, mut e, mut k, mut u) = (Ft::default(), Ft::default(), Ft::default(), Ft::default());
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let t = |f: Ft| ((f.1 as u128) << 32 | f.0 as u128) / 10_000;
    t(k) + t(u)
}

fn main() {
    let (tx, rx) = channel();
    let watch = toast::watch(move |t| {
        let _ = tx.send(t);
    })
    .expect("hook");
    println!("hook installed");

    let before = cpu_ms();
    std::thread::sleep(Duration::from_secs(10));
    println!("idle for 10 s with the hook on: {} ms of CPU", cpu_ms() - before);

    // Rounds 4 and 5 follow each other at once: a second pop-up while the first shows.
    for round in 1..=5 {
        let asked = Instant::now();
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &SCRIPT.replace("{r}", &round.to_string())]).output().expect("powershell");
        if !out.status.success() {
            println!("round {round}: powershell failed: {}", String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>());
            continue;
        }
        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(t) => println!(
                "round {round}: captured after {} ms: app={:?} title={:?} body={:?} (id {})",
                asked.elapsed().as_millis(),
                t.app,
                t.title,
                t.body,
                t.id
            ),
            Err(_) => println!("round {round}: nothing captured (Focus on, or the layout differs)"),
        }
        if round != 4 {
            std::thread::sleep(Duration::from_secs(7));
        }
    }
    drop(watch);
    println!("hook removed");
}
