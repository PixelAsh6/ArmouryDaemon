//! ArmouryDaemon — single executable, three entry paths.
//! Without arguments: the resident daemon (tray, hotkey, pipe server, UI lifecycle).
//! With `--ui`: the on-demand GUI process (spawned by the daemon from this same file).
//! With `--autostart`: the scheduled task's start — identical to the daemon, except that an
//! already-running instance is left completely alone (no dashboard pops up on resume from sleep).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon;
mod ipc;
mod killswitch;
mod overlay;
mod packs;
mod script;
mod ui;
mod watch;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--ui") {
        ui::run();
    } else {
        // The scheduler (logon, resume) must never disturb a daemon that is already up.
        daemon::run(!has("--autostart"));
    }
}
