//! ArmouryDaemon — single executable, two modes.
//! Without arguments: the resident daemon (tray, hotkey, pipe server, UI lifecycle).
//! With `--ui`: the on-demand GUI process (spawned by the daemon from this same file).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon;
mod ipc;
mod packs;
mod ui;
mod watch;

fn main() {
    let is_ui = std::env::args().skip(1).any(|a| a == "--ui");
    if is_ui {
        ui::run();
    } else {
        daemon::run();
    }
}
