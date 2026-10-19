//! ArmouryDaemon — single executable, five entry paths.
//! Without arguments: the resident daemon (tray, hotkey, pipe server, UI lifecycle).
//! With `--ui`: the on-demand GUI process (spawned by the daemon from this same file).
//! With `--console`: the pad-driven console launcher (spawned by the daemon on a controller
//! connect; the tray menu and the settings preview can open it too).
//! With `--autostart`: the scheduled task's start — identical to the daemon, except that an
//! already-running instance is left completely alone (no dashboard pops up on resume from sleep).
//! With `--catalog-list` / `--catalog-install <id>`: pack-catalog maintenance, print and exit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod art;
mod catalog;
mod console;
mod console_ui;
mod daemon;
mod hotkey;
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
    // Per-Monitor-V2 awareness first: the daemon, the GUI and the band all measure the
    // screen before their window exists, and Win32 only hands out physical pixels to a
    // process that says so. Winit sets the same context later, inside `EventLoop::new()`,
    // which is far too late for those measurements.
    set_dpi_awareness();

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

/// Make the process Per-Monitor-V2 DPI aware, before any window, COM object or font exists:
/// a DPI-unaware process gets virtualised screen metrics and 96-DPI GDI text. Failing is not
/// fatal — the process may already be aware (ERROR_ACCESS_DENIED) or the system may not know
/// the context — the mode still runs either way.
///
/// The error is kept instead of logged: there is no logger yet at this point, so the message
/// would be lost. `dpi_error()` lets the mode report it once `logging::init` has run.
fn set_dpi_awareness() {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };

    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            DPI_ERROR.store(GetLastError(), std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The Win32 error of the DPI switch (0 = it worked).
static DPI_ERROR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The error of the DPI switch, if it failed. Read it after the logger exists.
pub fn dpi_error() -> Option<u32> {
    match DPI_ERROR.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        code => Some(code),
    }
}
