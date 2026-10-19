//! ArmouryDaemon — single executable, five entry paths.
//! Without arguments: the resident daemon (tray, hotkey, pipe server, UI lifecycle).
//! With `--ui`: the on-demand GUI process (spawned by the daemon from this same file).
//! With `--console`: the pad-driven console launcher (spawned by the daemon on a controller
//! connect; the tray menu and the settings preview can open it too).
//! With `--autostart`: the scheduled task's start — identical to the daemon, except that an
//! already-running instance is left completely alone (no dashboard pops up on resume from sleep).
//! With `--catalog-list` / `--catalog-install <id>`: pack-catalog maintenance, print and exit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalog;
mod console;
mod console_ui;
mod daemon;
mod ipc;
mod killswitch;
mod overlay;
mod pad;
mod packs;
mod pick;
mod script;
mod ui;
mod watch;

fn main() {
    // The release build is a windows-subsystem binary: when it is launched from a terminal the
    // maintenance modes still need that console, or their output would vanish.
    attach_parent_console();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    // Pack-catalog maintenance: same functions the Settings panel will call later.
    if has("--catalog-list") || value("--catalog-install").is_some() {
        let url = value("--catalog-url").unwrap_or_else(|| {
            let cfg = armoury_core::config::load_or_create().unwrap_or_default();
            catalog::url_or_default(cfg.catalog_url.as_deref())
        });
        let dirs = daemon::pack_search_dirs();

        if let Some(id) = value("--catalog-install") {
            match catalog::install(&dirs, &url, &id) {
                Ok((id, updated)) => println!(
                    "catalog: {} {id}",
                    if updated { "updated" } else { "installed" }
                ),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            }
            return;
        }

        match catalog::fetch(&url) {
            Ok(list) => {
                let installed = catalog::installed_ids(&dirs);
                println!("catalog: {url} (manifest updated {})", list.updated);
                for pack in &list.packs {
                    let state = if installed.iter().any(|id| id == &pack.id) {
                        "installed"
                    } else {
                        ""
                    };
                    println!(
                        "  {:<20} v{:<8} {:<9} {} — {}",
                        pack.id, pack.version, state, pack.name, pack.description
                    );
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        return;
    }

    if has("--console") {
        console_ui::run();
    } else if has("--ui") {
        ui::run();
    } else {
        // The scheduler (logon, resume) must never disturb a daemon that is already up.
        daemon::run(!has("--autostart"));
    }
}

/// A windows-subsystem binary started from a terminal has no console of its own; attach to the
/// parent's so the catalog modes can print. Harmless (and a no-op) for the daemon paths.
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
