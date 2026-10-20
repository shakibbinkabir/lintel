// Lintel runs without a console window: Mascot is the whole UI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `lintel.exe --uninstall-hooks [--home <dir>]`: what the uninstaller runs
    // before it removes the relay. No window, no tray, nothing else starts.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--uninstall-hooks") {
        std::process::exit(notch_lib::uninstall_hooks(&args));
    }
    // `lintel.exe --watch <pid>`: the crash watchdog, which only sleeps (watchdog.rs).
    if let Some(at) = args.iter().position(|a| a == "--watch") {
        notch_lib::watchdog::run(args.get(at + 1).and_then(|p| p.parse().ok()).unwrap_or(0));
        return;
    }
    notch_lib::run()
}
