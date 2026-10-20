//! ArmouryDaemon — single executable, five entry paths.
//! Without arguments: the resident daemon (tray, hotkey, pipe server, UI lifecycle).
//! With `--ui`: the on-demand GUI process (spawned by the daemon from this same file).
//! With `--console`: the pad-driven console launcher (spawned by the daemon on a controller
//! connect; the tray menu and the settings preview can open it too).
//! With `--autostart`: the scheduled task's start — identical to the daemon, except that an
//! already-running instance is left completely alone (no dashboard pops up on resume from sleep).
//! With `--catalog-list` / `--catalog-install <id>`: pack-catalog maintenance, print and exit.
//! With `--version` / `--help`: print and exit. Any other argument is an error, never a daemon.

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
    match parse_args(args.iter().cloned()) {
        Action::Run => {
            // The scheduler (logon, resume) must never disturb a daemon that is already up.
            daemon::run(!is_autostart(&args));
        }
        Action::Ui => ui::run(),
        Action::Console => console_ui::run(),
        Action::CatalogList => catalog_list(catalog_url(&args)),
        Action::CatalogInstall(ref id) => catalog_install(id, catalog_url(&args)),
        Action::CatalogUrl(ref url) => catalog_list(Some(url.clone())),
        Action::Version => print_line(&format!(
            "{} {}",
            env!("CARGO_BIN_NAME"),
            env!("CARGO_PKG_VERSION")
        )),
        Action::Help => print_line(&usage()),
        Action::Error(ref message) => {
            eprintln!("{message}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}

/// What the command line asks for. An argument the binary does not know is an error: the
/// daemon must never start on a typo.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// The resident daemon.
    Run,
    /// The settings window.
    Ui,
    /// The pad-driven console.
    Console,
    /// List the packs of a catalog.
    CatalogList,
    /// Install one pack of a catalog.
    CatalogInstall(String),
    /// Read the catalog from this manifest URL.
    CatalogUrl(String),
    /// Print the version.
    Version,
    /// Print the usage.
    Help,
    /// Something the binary does not understand.
    Error(String),
}

/// Read the command line. Pure: the same arguments always give the same action.
fn parse_args(args: impl Iterator<Item = String>) -> Action {
    let args: Vec<String> = args.collect();
    let mut list = false;
    let mut install: Option<String> = None;
    let mut url: Option<String> = None;
    let mut ui = false;
    let mut console = false;
    let mut help = false;
    let mut version = false;
    // A flag whose value is missing: reported after the scan, so that an argument the binary
    // does not know is named first.
    let mut missing: Option<String> = None;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            // `--autostart` only changes how the daemon starts, `main` reads it from the
            // arguments.
            "--autostart" => {}
            "--ui" => ui = true,
            "--console" => console = true,
            "--catalog-list" => list = true,
            "--version" | "-V" => version = true,
            "--help" | "-h" => help = true,
            "--catalog-install" => match args.get(index + 1) {
                Some(value) if !value.starts_with('-') => {
                    install = Some(value.clone());
                    index += 1;
                }
                _ => missing = missing.or_else(|| Some(format!("{arg} needs a pack id"))),
            },
            "--catalog-url" => match args.get(index + 1) {
                Some(value) if !value.starts_with('-') => {
                    url = Some(value.clone());
                    index += 1;
                }
                _ => missing = missing.or_else(|| Some(format!("{arg} needs a manifest URL"))),
            },
            other => return Action::Error(format!("unknown argument: {other}")),
        }
        index += 1;
    }

    if let Some(message) = missing {
        return Action::Error(message);
    }
    if help {
        return Action::Help;
    }
    if version {
        return Action::Version;
    }
    if let Some(id) = install {
        return Action::CatalogInstall(id);
    }
    if list {
        return Action::CatalogList;
    }
    if let Some(url) = url {
        return Action::CatalogUrl(url);
    }
    if console {
        return Action::Console;
    }
    if ui {
        return Action::Ui;
    }
    Action::Run
}

/// `--autostart` starts the daemon without the dashboard.
fn is_autostart(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--autostart")
}

/// The manifest URL of `--catalog-url`, if the flag was given with one.
fn catalog_url(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "--catalog-url")
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// The flags this binary accepts, one line of help each. `parse_args` matches on the same
/// names, and a test keeps the two in step.
const FLAGS: &[(&str, &str)] = &[
    ("--autostart", "start the daemon without opening the dashboard"),
    ("--ui", "run the settings window"),
    ("--console", "run the pad-driven console"),
    ("--catalog-list", "list the packs of the catalog and exit"),
    ("--catalog-install <id>", "install one pack from the catalog and exit"),
    ("--catalog-url <url>", "read the catalog from this manifest URL"),
    ("--version, -V", "print the version and exit"),
    ("--help, -h", "print this usage and exit"),
];

/// The usage summary.
fn usage() -> String {
    let mut text = format!("usage: {} [flags]\n", env!("CARGO_BIN_NAME"));
    for (flag, help) in FLAGS {
        text.push_str(&format!("  {flag:<24} {help}\n"));
    }
    text.trim_end().to_string()
}

/// Print one line, for `--version` and `--help`.
///
/// The release build is a windows-subsystem binary: it attaches to the parent's console (see
/// `attach_parent_console`) and the standard output is that console. When it is not a console
/// — the output was redirected, or there is no console at all — the line also goes to stderr,
/// so it shows up wherever the caller is looking.
fn print_line(text: &str) {
    println!("{text}");
    if !stdout_is_console() {
        eprintln!("{text}");
    }
}

/// True when the standard output is a console.
fn stdout_is_console() -> bool {
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE};

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() {
            return false;
        }
        let mut mode = 0u32;
        GetConsoleMode(handle, &mut mode) != 0
    }
}

/// The pack-catalog maintenance mode: list the packs of a manifest.
fn catalog_list(url: Option<String>) {
    let url = url.unwrap_or_else(configured_catalog_url);
    let dirs = daemon::pack_search_dirs();

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
}

/// The pack-catalog maintenance mode: install one pack of a manifest.
fn catalog_install(id: &str, url: Option<String>) {
    let url = url.unwrap_or_else(configured_catalog_url);
    let dirs = daemon::pack_search_dirs();

    match catalog::install(&dirs, &url, id) {
        Ok((id, updated)) => println!(
            "catalog: {} {id}",
            if updated { "updated" } else { "installed" }
        ),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

/// The manifest URL: the configured one, else the official one.
fn configured_catalog_url() -> String {
    let cfg = armoury_core::config::load_or_create().unwrap_or_default();
    catalog::url_or_default(cfg.catalog_url.as_deref())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Action {
        parse_args(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn no_arguments_start_the_daemon() {
        assert_eq!(parse(&[]), Action::Run);
        assert_eq!(parse(&["--autostart"]), Action::Run);
        assert!(is_autostart(&["--autostart".to_string()]));
        assert!(!is_autostart(&[]));
    }

    #[test]
    fn the_entry_paths_are_recognised() {
        assert_eq!(parse(&["--ui"]), Action::Ui);
        assert_eq!(parse(&["--console"]), Action::Console);
    }

    #[test]
    fn the_catalog_modes_are_recognised() {
        assert_eq!(parse(&["--catalog-list"]), Action::CatalogList);
        assert_eq!(
            parse(&["--catalog-install", "g-helper"]),
            Action::CatalogInstall("g-helper".to_string())
        );
        assert_eq!(
            parse(&["--catalog-url", "https://host/catalog/index.json"]),
            Action::CatalogUrl("https://host/catalog/index.json".to_string())
        );
    }

    #[test]
    fn a_flag_without_its_value_is_an_error() {
        assert!(matches!(parse(&["--catalog-install"]), Action::Error(_)));
        assert!(matches!(parse(&["--catalog-url"]), Action::Error(_)));
        assert!(matches!(
            parse(&["--catalog-install", "--catalog-list"]),
            Action::Error(_)
        ));
    }

    #[test]
    fn version_and_help_are_recognised() {
        assert_eq!(parse(&["--version"]), Action::Version);
        assert_eq!(parse(&["-V"]), Action::Version);
        assert_eq!(parse(&["--help"]), Action::Help);
        assert_eq!(parse(&["-h"]), Action::Help);
    }

    #[test]
    fn an_unknown_argument_is_an_error() {
        match parse(&["--bogus"]) {
            Action::Error(ref message) => {
                assert!(message.contains("--bogus"), "the message must name it: {message}");
            }
            ref other => panic!("expected an error, got {other:?}"),
        }
        assert!(
            matches!(parse(&["--version", "--bogus"]), Action::Error(_)),
            "an unknown argument outranks --version"
        );
    }

    #[test]
    fn the_usage_lists_every_flag_the_parser_accepts() {
        let usage = usage();
        for flag in [
            "--autostart",
            "--ui",
            "--console",
            "--catalog-list",
            "--catalog-install",
            "--catalog-url",
            "--version",
            "-V",
            "--help",
            "-h",
        ] {
            assert!(usage.contains(flag), "the usage must mention {flag}");
        }
    }
}
