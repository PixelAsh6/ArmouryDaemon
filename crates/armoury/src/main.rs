//! ArmouryDaemon — single executable, six entry paths.
//! Without arguments: the resident daemon (tray, hotkey, pipe server, UI lifecycle).
//! With `--ui`: the on-demand GUI process (spawned by the daemon from this same file).
//! The console launcher is **not** here: it is a pack, its exe comes from the catalogue, and
//! this binary carries no code that could draw its band.
//! With `--autostart`: the scheduled task's start — identical to the daemon, except that an
//! already-running instance is left completely alone (no dashboard pops up on resume from sleep).
//! With `--install`: register the autostart task and exit — the elevated copy of the daemon,
//! started by the daemon itself when there is no task yet.
//! With `--catalog-list` / `--catalog-install <id>`: pack-catalog maintenance, print and exit.
//! With `--version` / `--help`: print and exit. Any other argument is an error, never a daemon.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod art;
mod catalog;
mod console;
mod daemon;
mod diagnostics;
mod dpi;
mod fonts;
mod hotkey;
mod helpers;
mod install;
mod ipc;
mod killswitch;
mod net;
mod overlay;
mod pad;
mod packs;
mod pick;
mod script;
mod trust;
mod ui;
mod update;
mod watch;

/// The scratch folders the tests write into. Only there while the tests run.
#[cfg(test)]
pub(crate) mod testutil;

fn main() {
    // Per-Monitor-V2 awareness first: the daemon, the GUI and the band all measure the
    // screen before their window exists, and Win32 only hands out physical pixels to a
    // process that says so. Winit sets the same context later, inside `EventLoop::new()`,
    // which is far too late for those measurements.
    dpi::set();

    // The release build is a windows-subsystem binary: when it is launched from a terminal the
    // maintenance modes still need that console, or their output would vanish.
    attach_parent_console();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(args.iter().cloned()) {
        Action::Run => {
            // The scheduler (logon, resume) must never disturb a daemon that is already up.
            daemon::run(if is_autostart(&args) {
                daemon::Start::Autostart
            } else {
                daemon::Start::Manual
            });
        }
        Action::Ui => ui::run(),
        Action::Install => install::install_self(),
        Action::CatalogList(ref url) => catalog_list(url.clone()),
        Action::CatalogInstall(ref id, ref url) => catalog_install(id, url.clone()),
        Action::CatalogUrl(ref url) => catalog_list(Some(url.clone())),
    Action::ModuleList => module_list(),
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
    /// Register the autostart task and exit: the elevated copy of the daemon.
    Install,
    /// List the packs of a catalog. The URL is the one `--catalog-url` gave, if it was given.
    CatalogList(Option<String>),
    /// Install one pack of a catalog. The URL is the one `--catalog-url` gave, if it was
    /// given.
    CatalogInstall(String, Option<String>),
    /// Read the catalog from this manifest URL.
    CatalogUrl(String),
    /// List what the daemon has installed: packs, modes and helpers, one line each.
    ModuleList,
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
    let mut modules = false;
    // The pack id of `--catalog-install`. It is not called `install`: that name belongs to
    // the module above, and a local with the same name is a trap for the next reader.
    let mut install_id: Option<String> = None;
    let mut url: Option<String> = None;
    let mut ui = false;
    let mut install_task = false;
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
            "--install" => install_task = true,
            "--catalog-list" => list = true,
            "--module-list" => modules = true,
            "--version" | "-V" => version = true,
            "--help" | "-h" => help = true,
            "--catalog-install" => match args.get(index + 1) {
                Some(value) if !value.starts_with('-') => {
                    install_id = Some(value.clone());
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
    if let Some(id) = install_id {
        return Action::CatalogInstall(id, url);
    }
    if list {
        return Action::CatalogList(url);
    }
    if modules {
        return Action::ModuleList;
    }
    if let Some(url) = url {
        return Action::CatalogUrl(url);
    }
    if ui {
        return Action::Ui;
    }
    if install_task {
        return Action::Install;
    }
    Action::Run
}

/// `--autostart` starts the daemon without the dashboard.
fn is_autostart(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--autostart")
}

/// The flags this binary accepts, one line of help each. `parse_args` matches on the same
/// names, and a test keeps the two in step.
const FLAGS: &[(&str, &str)] = &[
    ("--autostart", "start the daemon without opening the dashboard"),
    ("--ui", "run the settings window"),
    ("--install", "register the autostart task (logon + resume) and exit"),
    ("--catalog-list", "list the packs of the catalog and exit"),
    ("--catalog-install <id>", "install one pack from the catalog and exit"),
    ("--catalog-url <url>", "read the catalog from this manifest URL"),
    ("--module-list", "print the installed modules and exit"),
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
            for pack in list.entries() {
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

/// Print what the daemon has installed: packs from `packs/` (labeled by kind) and
/// helpers from `modules/`, one line each, sorted by id. The list the "the helper
/// is still found after a restart" story is checked against: a fresh process reads
/// it back from disk, nothing was carried in memory.
fn module_list() {
    let dirs = daemon::pack_search_dirs();
    let mut rows = Vec::new();
    for installed in packs::load(&dirs) {
        let kind = if installed.pack.kind == armoury_core::pack::PackKind::Mode {
            "mode"
        } else {
            "pack"
        };
        rows.push((
            installed.pack.id.clone(),
            kind.to_string(),
            installed.pack.version.clone(),
            installed.pack.name.clone(),
        ));
    }
    for helper in helpers::installed(&dirs) {
        rows.push((helper.id, "helper".to_string(), helper.version, helper.name));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        println!("modules: nothing installed");
    }
    for (id, kind, version, name) in rows {
        println!("  {kind:<7} {id:<20} v{version:<8} {name}");
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
        assert_eq!(parse(&["--install"]), Action::Install);
    }

    #[test]
    fn the_catalog_modes_are_recognised() {
        assert_eq!(parse(&["--catalog-list"]), Action::CatalogList(None));
        assert_eq!(parse(&["--module-list"]), Action::ModuleList);
        assert_eq!(
            parse(&["--catalog-install", "g-helper"]),
            Action::CatalogInstall("g-helper".to_string(), None)
        );
        assert_eq!(
            parse(&["--catalog-url", "https://host/catalog/index.json"]),
            Action::CatalogUrl("https://host/catalog/index.json".to_string())
        );
    }

    #[test]
    fn the_catalog_url_is_the_one_the_parser_took() {
        // The URL travels with the action, so the reader and the parser cannot disagree.
        let url = "https://host/catalog/index.json";
        assert_eq!(
            parse(&["--catalog-list", "--catalog-url", url]),
            Action::CatalogList(Some(url.to_string()))
        );
        assert_eq!(
            parse(&["--catalog-install", "g-helper", "--catalog-url", url]),
            Action::CatalogInstall("g-helper".to_string(), Some(url.to_string()))
        );
        // given twice, the last one wins — in the parser and in the reader alike
        assert_eq!(
            parse(&[
                "--catalog-url",
                "https://first/index.json",
                "--catalog-url",
                "https://second/index.json"
            ]),
            Action::CatalogUrl("https://second/index.json".to_string())
        );
        assert_eq!(
            parse(&[
                "--catalog-list",
                "--catalog-url",
                "https://first/index.json",
                "--catalog-url",
                "https://second/index.json"
            ]),
            Action::CatalogList(Some("https://second/index.json".to_string()))
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
            "--install",
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

    #[test]
    fn the_catalog_id_does_not_shadow_the_install_module() {
        // The local of `--catalog-install` used to be called `install` - the same name as
        // the module. It compiles (they live in different namespaces) and it is a trap for
        // the next reader, so the rename is kept honest by this test.
        //
        // The lines are looked at one by one: a plain `contains` would find this test's
        // own needles and prove nothing.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("main.rs");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let lines: Vec<&str> = source.lines().map(|line| line.trim_start()).collect();
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("let mut install:")),
            "the local must not be called `install` any more: it shadows `mod install`"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("let mut install_id:")),
            "the local must be called `install_id`"
        );
        assert!(
            lines.iter().any(|line| *line == "mod install;"),
            "the module must stay"
        );
    }
}
