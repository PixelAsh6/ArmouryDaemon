//! Daemon mode: single instance, tray icon + menu, End/Home keyboard hook,
//! named-pipe server, UI process lifecycle (spawn/focus/kill), status broadcasts.

use crate::art;
use crate::catalog;
use crate::console;
use crate::helpers;
use crate::hotkey;
use crate::install;
use crate::ipc;
use crate::killswitch;
use crate::overlay;
use crate::pad;
use crate::packs;
use crate::watch;
use armoury_core::config::{
    AppConfig, CONSOLE_OPEN_ALWAYS, CONSOLE_OPEN_NEVER, ConsoleApp, TrackedApp,
    normalize_console_open,
};
use armoury_core::engine::{Engine, EngineState, Transition};
use armoury_core::pack::PackKind;
use armoury_core::protocol::{ArtListing, ArtSlot, Request, Response};
use armoury_core::status::{LastError, PackSettingStatus, PackStatus, RunState, StatusSnapshot};
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced, ListenerOptions};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use log::{error, info, warn};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{MsgWaitForMultipleObjectsEx, QS_ALLINPUT};

/// CREATE_NO_WINDOW: the child gets no console window, so nothing flashes on the desktop.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// DETACHED_PROCESS: the child is not tied to the daemon's console and outlives the daemon.
const DETACHED_PROCESS: u32 = 0x0000_0008;
/// The creation flags for a game the daemon starts.
const GAME_FLAGS: u32 = CREATE_NO_WINDOW | DETACHED_PROCESS;
/// How long a started process gets to stay up before the launch counts as failed. Long
/// enough to catch the games that die in their first second, short enough not to be felt.
const LAUNCH_GRACE: Duration = Duration::from_millis(600);
/// How long a fresh console launcher gets to show its band. Until the band is up the
/// launcher has no window on screen, and that is not yet a stale launcher.
const CONSOLE_GRACE: Duration = Duration::from_secs(10);
/// Entering and leaving idle mode are the user's own actions: they change the packs even
/// while the daemon is paused (the pause stops *automatic* switching, not the user's own
/// actions). Entering and leaving must behave the same, or the packs would stay reverted
/// after the user leaves idle mode.
const IDLE_IS_FORCED: bool = true;

/// How this daemon was started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    /// A user started it (a double-click, the tray, a second instance). The dashboard may
    /// open itself on the first run.
    Manual,
    /// The scheduled task started it (logon, resume from sleep). It must not disturb the
    /// user: nothing visible happens.
    Autostart,
}

/// Why the band is being opened.
///
/// An automatic open (the daemon starts, the machine resumes from sleep, the open mode
/// changes) must never interrupt a game: it is refused while a tracked app is running. An
/// explicit open (the tray's "Console mode", the dashboard's console button) always opens
/// the band - the user asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleOpen {
    /// The daemon did it on its own: startup, resume from sleep, a change of the mode.
    Automatic,
    /// The user asked for it.
    Explicit,
}

/// May the band open right now?
///
/// An automatic open is refused while a tracked app is running - the band would pop up
/// over the game. An explicit one always opens the band: the user asked for it, and the
/// user can close the game themselves.
fn console_open_allowed(reason: ConsoleOpen, tracked_running: bool) -> bool {
    match reason {
        ConsoleOpen::Automatic => !tracked_running,
        ConsoleOpen::Explicit => true,
    }
}

/// Does the dashboard open itself now?
///
/// The first run opens it — whatever is installed: a user who unpacked the zip has three
/// packs and still lands on the dashboard instead of having to know that there is a tray
/// icon. The marker is what ends the first run: it is written as the dashboard opens, so
/// the dashboard never comes back on its own and closing it is final. A scheduled start
/// (logon, resume from sleep) never opens anything.
fn first_run_opens_dashboard(start: Start, is_first_run: bool) -> bool {
    start == Start::Manual && is_first_run
}

pub fn run(start: Start) {
    // The scheduler (logon, resume) must never disturb a daemon that is already up, and
    // it must never open a window by itself.
    let show_ui_when_running = start == Start::Manual;
    if let Err(e) = armoury_core::logging::init("daemon") {
        eprintln!("logging init failed: {e}");
    }
    info!("daemon starting (v{})", armoury_core::VERSION);
    if let Some(code) = crate::dpi::error() {
        // The DPI switch failed before the logger existed; now it can be said.
        info!("DPI awareness was not set (error {code}): the process may already be aware");
    }

    // ---- single instance ----
    // Before anything with a side effect: overlay::recover_if_needed() unmutes every output
    // device, and a scheduler start (logon + resume) must never do that to a running daemon.
    let _mutex_guard = match acquire_singleton() {
        Singleton::AlreadyRunning => {
            if show_ui_when_running {
                // a user launch (tray / second Start-Process): surface the dashboard
                info!("another instance is already running — asking it to show the UI");
                nudge_existing_instance();
            } else {
                // the scheduler fired again (logon + resume): do nothing visible
                info!("another instance is already running — autostart ignored, no UI");
            }
            return;
        }
        Singleton::Acquired(h) => h,
    };

    // Now that we know we are the only instance: COM for the audio interfaces, and recover
    // from a previous run that died while idle.
    killswitch::init_com();
    overlay::recover_if_needed();

    // ---- the autostart task ----
    // The daemon registers the task for itself when there is none, so the executable
    // works on its own: a user who downloaded only ArmouryDaemon.exe still comes up at
    // every logon and after every resume from sleep. It is quick, it does nothing when
    // the task is already there, and it never touches a task that runs another program.
    if install::is_test_instance() {
        info!("autostart: skipped — this is a throw-away instance (ARMOURY_TEST_NS)");
    } else if let Some(exe) = std::env::current_exe().ok() {
        install::check_and_register(&exe);
    }

    // ---- config ----
    let mut cfg = armoury_core::config::load_or_create().unwrap_or_else(|e| {
        error!("config load failed ({e}); using in-memory defaults");
        AppConfig::default()
    });
    // A hand-edited config may carry any number, and the engine turns the cooldown into a
    // deadline: `Instant + Duration` panics on the overflow. The window the UI writes is
    // 5 to 3600 s, so the startup path holds the same range.
    if !(5..=3600).contains(&cfg.cooldown_seconds) {
        let clamped = cfg.cooldown_seconds.clamp(5, 3600);
        warn!(
            "config: cooldown {}s is outside 5 to 3600 - using {clamped}s",
            cfg.cooldown_seconds
        );
        cfg.cooldown_seconds = clamped;
    }
    info!(
        "config: cooldown {}s, {} tracked app(s)",
        cfg.cooldown_seconds,
        cfg.tracked_apps.len(),
    );

    // ---- named-pipe server ----
    let pipe_name = armoury_core::pipe_name();
    let name = match pipe_name.as_str().to_ns_name::<GenericNamespaced>() {
        Ok(n) => n,
        Err(e) => {
            error!("invalid pipe name: {e}");
            return;
        }
    };
    let mut options = ListenerOptions::new().name(name);
    if let Some(sd) = pipe_security_descriptor() {
        options = options.security_descriptor(sd);
    }
    let listener = match options.create_sync() {
        Ok(l) => l,
        Err(e) => {
            error!("failed to create pipe server: {e}");
            return;
        }
    };
    let (pipe_tx, pipe_rx) = crossbeam_channel::unbounded::<ipc::PipeEvent>();

    // ---- wake event: every producer signals it so the main loop reacts immediately ----
    let wake = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if wake.is_null() {
        error!("failed to create the wake event");
        return;
    }
    let wake_handle = wake as usize;

    ipc::start(listener, pipe_tx, wake_handle);
    info!("pipe server ready ({})", pipe_name);

    // ---- tracked-app watcher + trigger engine ----
    let tracked = Arc::new(Mutex::new(cfg.tracked_apps.clone()));
    let (watch_tx, watch_rx) = crossbeam_channel::unbounded::<watch::WatchEvent>();
    watch::start(tracked.clone(), watch_tx, wake_handle);

    // ---- controller watch (console mode) ----
    let (pad_tx, pad_rx) = crossbeam_channel::unbounded::<console::PadEvent>();
    // Cover-art worker -> main loop: a key with fresh art, or "" when a pass finished.
    let (art_tx, art_rx) = crossbeam_channel::unbounded::<String>();
    // The picker's worker threads -> main loop: the photos of a page, or a saved pick.
    let (art_query_tx, art_query_rx) = crossbeam_channel::unbounded::<ArtAnswer>();
    // Launch worker -> main loop: a finished launch and the client that asked for it.
    let (launch_tx, launch_rx) = crossbeam_channel::unbounded::<LaunchEvent>();
    console::start(pad_tx, wake_handle);
    let engine = {
        let mut e = Engine::new(Duration::from_secs(cfg.cooldown_seconds));
        if cfg.paused {
            // start frozen, exactly as the user left it
            e.set_paused(true, Instant::now());
        }
        e
    };

    // ---- packs ----
    let search = pack_search_dirs();
    // A fresh install has no packs yet: the folder is created so the packs have
    // somewhere to land, and the daemon starts with an empty list instead of failing.
    if let Some(created) = packs::ensure_packs_dir(&search) {
        info!(
            "packs: created {} — no packs are installed yet",
            created.display()
        );
    }
    let mut installed = packs::load(&search);
    let no_packs = installed.is_empty();
    if no_packs {
        info!("packs: none installed — install them from the settings window");
    }
    enrich_rainmeter(&mut installed);

    let pack_catalog: Vec<PackMeta> = build_catalog(&installed);
    let (pack_tx, pack_rx) = crossbeam_channel::unbounded::<packs::PackJob>();
    let (mode_tx, mode_rx) = crossbeam_channel::unbounded::<packs::PackJob>();
    let (pack_out_tx, pack_out_rx) = crossbeam_channel::unbounded::<packs::PackOutcome>();
    packs::start(installed, pack_rx, mode_rx, pack_out_tx, wake_handle);
    info!(
        "watching {} tracked app(s); cooldown {}s",
        cfg.tracked_apps.len(),
        cfg.cooldown_seconds
    );

    // A restart counts as "revert everything" (decision A3). The journal's last entry holds
    // the applied ids plus the values captured at apply time, so the revert is exact.
    if let Some(job) = packs::journal_pending_revert() {
        info!(
            "journal: {} pack(s) were applied when the daemon last stopped — reverting now",
            job.enabled.len()
        );
        let packs::PackJob { apply, enabled, values, captured, .. } = job;
        let (pack_enabled, mode_enabled) = split_enabled(enabled, &pack_catalog);
        let _ = pack_tx.send(packs::PackJob {
            apply,
            tag: None,
            enabled: pack_enabled,
            values: values.clone(),
            captured: captured.clone(),
        });
        if mode_enabled.values().any(|v| *v) {
            let _ = mode_tx.send(packs::PackJob {
                apply,
                tag: None,
                enabled: mode_enabled,
                values,
                captured,
            });
        }
    }

    // ---- tray icon + menu (created on this thread: it owns the message pump) ----
    let menu = Menu::new();
    let open_item = MenuItem::with_id("open", "Open dashboard", true, None);
    let pause_item = MenuItem::with_id("pause_all", "Pause all packs", true, None);
    let idle_item = MenuItem::with_id("idle", "Idle mode", true, None);
    let kill_item = MenuItem::with_id("killswitch", "Toggle killswitch", true, None);
    let console_item = MenuItem::with_id("console", "Console mode", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);
    if let Err(e) = menu.append_items(&[
        &open_item,
        &pause_item,
        &idle_item,
        &kill_item,
        &console_item,
        &quit_item,
    ])
    {
        warn!("failed to build tray menu: {e}");
    }
    let tray: Option<TrayIcon> = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ArmouryDaemon")
        .with_icon(make_tray_icon())
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(t) => {
            info!("tray icon created");
            Some(t)
        }
        Err(e) => {
            error!("tray icon creation failed: {e} (continuing without tray)");
            None
        }
    };

    // ---- keyboard hook (End = killswitch, Home = idle mode) ----
    // A low-level hook, not RegisterHotKey: the key must keep working in the foreground
    // application, and it has to be installed on this thread (the one that pumps the messages).
    let (hotkey_hook, hotkey_rx) = match hotkey::install() {
        Some((hook, rx)) => (Some(hook), Some(rx)),
        None => (None, None),
    };

    // ---- main loop ----
    // A single waitable event lets every producer (watcher, pipe threads) wake the loop
    // immediately; Win32 messages (tray, keyboard hook) wake it through the message queue.
    let tray_rx = TrayIconEvent::receiver();
    let menu_rx = MenuEvent::receiver();

    let mut app = App {
        cfg,
        pause_item: pause_item.clone(),
        idle_item: idle_item.clone(),
        kill_item: kill_item.clone(),
        tray,
        icon_normal: make_tray_icon(),
        icon_active: make_tray_icon_active(),
        killswitch: killswitch::Ledger::default(),
        running_pids: HashMap::new(),
        overlay: overlay::Overlay::default(),
        idle_engage_tick: 0,
        idle_deadline: None,
        last_idle_beat: None,
        tracked,
        running_paths: HashMap::new(),
        engine,
        search,
        pack_tx,
        mode_tx,
        pack_out_rx,
        pack_catalog,
        applied_packs: Vec::new(),
        captured_by_pack: HashMap::new(),
        pending_removal: None,
        started: Instant::now(),
        clients: HashMap::new(),
        ui_conn: None,
        ui_child: None,
        spawning_ui: false,
        console_child: None,
        console_spawned: None,
        pad_rx,
        art_tx,
        art_rx,
        art_query_tx,
        art_query_rx,
        art_busy: false,
        launch_tx,
        launch_rx,
        // A pass with the old (keyless) key may still be running: it leaves miss markers.
        art_retry_after_pass: false,
        art_pass_again: false,
        last_error: None,
        quit_requested: QuitRequest::default(),
        last_broadcast: Instant::now(),
        last_tooltip: String::new(),
        wake: wake_handle,
    };
    let mut last_housekeeping = Instant::now();
    if app.cfg.paused {
        app.pause_item.set_text("Resume all packs");
        info!("starting paused (packs will not switch until resumed)");
    }
    if !app.cfg.killswitch_enabled {
        // The config switch is off: the tray item says so instead of promising a toggle.
        app.kill_item.set_text("Killswitch off in config");
    }
    info!("daemon ready");
    // ---- the first run: the dashboard opens itself ----
    // Every first run lands on the dashboard instead of having to know that there is a
    // tray icon. It opens once: the marker remembers it, so the dashboard never comes
    // back on its own and closing it is final. The packs make no difference — a zip
    // install has three of them and still opens the dashboard.
    let first_run_marker = install::first_run_marker();
    if first_run_opens_dashboard(start, install::is_first_run(&first_run_marker)) {
        match install::mark_first_run_done(&first_run_marker) {
            Ok(()) => {
                info!("first run: opening the dashboard");
                app.on_open();
            }
            Err(e) => warn!("first run: the marker could not be written ({e})"),
        }
    }
    app.ensure_art(); // tracked apps get their covers even before the console opens
    if app.cfg.console_open == CONSOLE_OPEN_ALWAYS {
        // The startup open is an automatic one: the band must not pop up over a game.
        // The engine knows nothing yet - the watcher's first samples reach the daemon
        // through the main loop - so the running tracked apps are looked up directly.
        info!("console: open mode 'always' — the launcher starts unless a tracked app is running");
        app.open_console(ConsoleOpen::Automatic);
    }

    let quit = loop {
        pump_win32_messages();

        // drain every source without blocking
        while let Ok(e) = tray_rx.try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                app.on_open();
            }
        }
        while let Ok(e) = menu_rx.try_recv() {
            match e.id.0.as_str() {
                "open" => app.on_open(),
                "pause_all" => {
                    let paused = app.cfg.paused;
                    app.set_paused(!paused);
                }
                "idle" => {
                    if app.overlay.is_active() {
                        info!("idle mode off (tray menu)");
                        app.exit_idle();
                    } else {
                        info!("idle mode on (tray menu)");
                        app.enter_idle();
                    }
                }
                "killswitch" => app.toggle_killswitch(),
                // The tray's "Console mode": the user asked for the band, so it opens even
                // while a game is running.
                "console" => app.open_console(ConsoleOpen::Explicit),
                "quit" => {
                    info!("quit requested from tray menu");
                    // "Quit means close everything" (user decision 2026-10-18): put the apps back
                    // before the daemon goes away, instead of leaving them in game mode until the
                    // next start reverts them. The shutdown sequence itself runs below, so the
                    // tray menu and a `quit` request over the pipe take the same path.
                    app.quit_requested.ask();
                }
                other => info!("tray menu '{other}' — not implemented yet"),
            }
        }
        if let Some(rx) = &hotkey_rx {
            while let Ok(vk) = rx.try_recv() {
                app.on_hotkey(vk);
            }
        }
        while let Ok(p) = pipe_rx.try_recv() {
            app.on_pipe(p);
        }
        while let Ok(w) = watch_rx.try_recv() {
            app.on_watch(w);
        }
        while let Ok(ev) = app.pad_rx.try_recv() {
            app.on_pad(ev);
        }
        while let Ok(key) = app.art_rx.try_recv() {
            app.on_art(key);
        }
        while let Ok(answer) = app.art_query_rx.try_recv() {
            app.on_art_answer(answer);
        }
        while let Ok(ev) = app.launch_rx.try_recv() {
            app.on_launch(ev);
        }
        loop {
            match app.pack_out_rx.try_recv() {
                Ok(o) => app.on_pack_outcome(o),
                Err(_) => break,
            }
        }
        // A pack may ask for the idle overlay. The pack worker cannot show it itself: the
        // black windows belong to the thread that pumps their messages, and that is this
        // one - so the daemon keeps owning the crash-recovery ledger and the keypress
        // dismissal. Only the newest request is waiting (it replaces the one before it), and
        // a request for the state the daemon is already in costs nothing.
        if let Some(request) = packs::take_overlay_request() {
            let active = app.overlay.is_active();
            match request {
                packs::OverlayRequest::Show if !active => {
                    info!("a pack asked for the idle overlay");
                    app.enter_idle();
                }
                packs::OverlayRequest::Hide if active => {
                    info!("a pack asked for the idle overlay to go away");
                    app.exit_idle();
                }
                _ => {}
            }
        }
        if app.quit_requested.asked() {
            // "Quit means close everything": the packs go back and the audio comes back before
            // the daemon goes away.
            app.revert_before_quit();
            app.release_killswitch_for_exit();
            app.leave_idle_for_exit();
            break true;
        }

        if last_housekeeping.elapsed() >= Duration::from_secs(1) {
            last_housekeeping = Instant::now();
            app.on_tick();
        }

        // Block until a message arrives, a producer signals the event, or the wait expires.
        // Idle cost is therefore ~zero (no polling tick); while the overlay is up we wake
        // more often so input dismisses it promptly.
        let wait_ms = if app.overlay.is_active() { 250 } else { 1000 };
        unsafe {
            MsgWaitForMultipleObjectsEx(1, &wake, wait_ms, QS_ALLINPUT, 0);
        }
    };

    if !quit {
        info!("main loop ended unexpectedly");
    }

    // ---- shutdown ----
    info!("shutting down");
    if let Some(hook) = hotkey_hook {
        hook.uninstall();
    }
    if let Some(mut child) = app.ui_child.take() {
        let _ = child.kill();
    }
    if let Some(mut child) = app.console_child.take() {
        let _ = child.kill();
    }
    log::logger().flush();
}

fn basename_lower(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| path.to_ascii_lowercase())
}

/// Is one of the tracked apps running right now?
///
/// The watcher walks the process list once a second and tells the daemon what it finds —
/// but its first samples only reach the daemon through the main loop, which has not started
/// yet when the daemon opens the band at startup. So the daemon reads the process list
/// itself, once, to answer the question that matters there: is a game running?
fn tracked_app_running_now(cfg: &AppConfig) -> bool {
    let wanted: Vec<String> = cfg
        .tracked_apps
        .iter()
        .map(|app| basename_lower(&app.exe))
        .collect();
    if wanted.is_empty() {
        return false;
    }
    running_process_names()
        .iter()
        .any(|name| wanted.iter().any(|w| w == name))
}

/// The file names of the running processes, lowercased.
///
/// The same walk the watcher does, only once: the daemon needs the answer before the main
/// loop has seen the watcher's first sample.
fn running_process_names() -> Vec<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut names = Vec::new();
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        names.push(String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase());
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    names
}

/// Is the transition performed right now? A manual action (force) always is; an automatic one
/// is suppressed while the packs are paused (the user's own switch, decision G12).
fn transition_allowed(paused: bool, force: bool) -> bool {
    !paused || force
}

/// Take the tracked app out of the list. Matching is by exe file name (the user's choice), so
/// a client may send the full path it stored or only the file name. Returns the entries that
/// were removed — empty when nothing matched.
fn remove_tracked_apps(list: &mut Vec<TrackedApp>, wanted: &str) -> Vec<TrackedApp> {
    let base = basename_lower(wanted);
    let mut removed = Vec::new();
    list.retain(|app| {
        if basename_lower(&app.exe) == base {
            removed.push(app.clone());
            false
        } else {
            true
        }
    });
    removed
}

/// Now, in unix seconds (the unit the journal and `LastError` use).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Remember a failure so the dashboard can show it. The most recent one wins.
fn remember_error(slot: &mut Option<LastError>, source: &str, message: &str) {
    *slot = Some(LastError {
        at: unix_now(),
        source: source.to_string(),
        message: message.to_string(),
    });
}

/// Forget a remembered failure that came from `prefix` ("" forgets any source).
fn forget_error(slot: &mut Option<LastError>, prefix: &str) {
    if slot
        .as_ref()
        .map(|e| e.source.starts_with(prefix))
        .unwrap_or(false)
    {
        *slot = None;
    }
}

/// Did any **key** get pressed since the previous call? Mouse buttons and wheels are skipped on
/// purpose: idle mode is meant to end on a deliberate keypress, not on a nudged mouse (the user's
/// decision on 2026-10-17, after a mouse twitch would have woken an AFK session).
///
/// `GetAsyncKeyState`'s low bit means "pressed since the previous call", so the caller must
/// **prime** this once when idle engages, or a keypress from before would dismiss it immediately.
fn keyboard_pressed_since_last_poll() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    let mut pressed = false;
    // 0x01..=0x07 are the mouse buttons, the wheel and VK_CANCEL: never treated as a wake key.
    for vk in 0x08..=0xFE {
        if unsafe { GetAsyncKeyState(vk) } & 0x0001 != 0 {
            pressed = true; // keep draining every flag, so nothing is reported twice
        }
    }
    pressed
}

fn tick_count() -> u32 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() }
}

/// Tray icon built from the shared procedural icon generator.
fn make_tray_icon() -> tray_icon::Icon {    let (rgba, w, h) = armoury_core::icon::rgba_icon(32);
    tray_icon::Icon::from_rgba(rgba, w, h).expect("tray icon rgba is 32x32")
}

/// The killswitch variant (red slash) so the state is visible without a Scroll Lock LED.
fn make_tray_icon_active() -> tray_icon::Icon {
    let (rgba, w, h) = armoury_core::icon::rgba_icon_active(32);
    tray_icon::Icon::from_rgba(rgba, w, h).expect("tray icon rgba is 32x32")
}

// ---------------------------------------------------------------------------

/// Where packs are looked for: beside the executable first (the install folder), then the
/// repository root for a dev build (`<repo>/target/release/ArmouryDaemon.exe`).
///
/// The second folder is only there for a dev build, so it is only used when it really is
/// one: a lone executable in `C:\Users\Someone\` must not fall back to `C:\` and use the
/// packs of a folder that has nothing to do with it.
pub(crate) fn pack_search_dirs() -> Vec<std::path::PathBuf> {
    let mut search: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            search.push(dir.to_path_buf());
            if let Some(up2) = dir.parent().and_then(|d| d.parent()) {
                search.push(up2.to_path_buf());
            }
        }
    }
    packs::usable_search_dirs(&search)
}

/// The file name the console launcher runs under, in a helper module's own record.
const CONSOLE_EXE: &str = "ArmouryConsole.exe";

/// Is a helper module's recorded `entry` this launcher? The entry may name a subfolder
/// (`console/ArmouryConsole.exe`); the file name alone decides, and Windows file names
/// are case-insensitive.
fn matches_console_entry(entry: &str, exe: &str) -> bool {
    entry
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|file| file.eq_ignore_ascii_case(exe))
}

/// The installed console helper to spawn: the first `modules/<id>/` whose recorded entry
/// runs the console exe and whose exe is actually on disk. `helpers::installed` sorts by
/// id, so the choice is deterministic: two console helpers pick the lower id.
fn console_helper(search: &[std::path::PathBuf]) -> Option<helpers::HelperModule> {
    helpers::installed(search).into_iter().find(|module| {
        matches_console_entry(&module.entry, CONSOLE_EXE) && module.path.is_file()
    })
}

/// Install a pack from a file and clean up after it.
///
/// The catalogue download leaves a verified copy of the pack in the temp folder: the
/// daemon reads it, installs the pack and removes the file again — whether the install
/// worked or not, so the temp folder does not fill up with one file per install. A file
/// that cannot be removed is not worth an error: the install is what matters.
fn install_pack_file(
    search: &[std::path::PathBuf],
    source: &std::path::Path,
) -> Result<(String, bool), String> {
    let result = packs::install_from_path(search, source);
    remove_catalog_temp(source);
    result
}

/// Install a helper module from the downloaded archive and clean up after it.
///
/// The UI's worker downloads the archive, checks it against the signed catalogue and
/// leaves it in temp; the request then names the manifest entry and its SHA-256. The
/// daemon is the last one to read the file, and it trusts nobody along the way: it
/// re-checks the bytes against the SHA-256 it was told before any of them land
/// anywhere, and the helper installer's own rules (a safe folder name, a checked entry
/// path, the folder locks, the extraction caps) run after that. Whether it worked or
/// not, the archive is removed.
fn install_helper_file(
    search: &[std::path::PathBuf],
    id: &str,
    name: &str,
    version: &str,
    entry: &str,
    source: &std::path::Path,
    sha256: &str,
    size: Option<u64>,
) -> Result<(String, bool), String> {
    let result = (|| {
        let said = "the helper request asks for";
        if !is_helper_temp(source) {
            return Err(format!(
                "{said} {} - that is not the temp archive the catalogue download \
                 writes, so nothing is extracted",
                source.display()
            ));
        }
        let archive = std::fs::read(source)
            .map_err(|e| format!("cannot read the downloaded helper {}: {e}", source.display()))?;
        catalog::verify(&archive, sha256, size)?;
        helpers::install_from_bytes(
            search,
            &catalog::CatalogPack {
                id: id.to_string(),
                name: name.to_string(),
                version: version.to_string(),
                description: String::new(),
                kind: "helper".to_string(),
                file: String::new(),
                sha256: sha256.to_string(),
                size,
                min_engine: None,
                entry: Some(entry.to_string()),
            },
            &archive,
        )
    })();
    remove_helper_temp(source);
    result
}

/// Remove the verified catalogue file the install has just read.
///
/// Only a file that is one of ours goes: it has to sit in the temp folder and to carry
/// the name the catalogue download gives it. A pack.json the user picked in the file
/// dialog is the user's own file and stays where it is.
fn remove_catalog_temp(source: &std::path::Path) {
    if !is_catalog_temp(source) {
        return;
    }
    match std::fs::remove_file(source) {
        Ok(()) => info!("removed the downloaded file {}", source.display()),
        Err(e) => warn!("could not remove {} ({e})", source.display()),
    }
}

/// Remove the verified helper archive the install has just read - or refused. The
/// download is not the user's to keep: whatever the request said, the archive that
/// bore it is gone when the answer is in.
fn remove_helper_temp(source: &std::path::Path) {
    if !is_helper_temp(source) {
        return;
    }
    match std::fs::remove_file(source) {
        Ok(()) => info!("removed the downloaded helper archive {}", source.display()),
        Err(e) => warn!("could not remove {} ({e})", source.display()),
    }
}

/// Is this the temp file the catalogue download writes with `suffix`?
///
/// The name rule and the folder rule are one test: the file carries the download's own
/// prefix and suffix, and it has to sit in the temp folder - a file with that name
/// somewhere else is not ours to read or to remove.
fn is_temp_of(path: &std::path::Path, suffix: &str) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !name.starts_with(catalog::TEMP_PREFIX) || !name.ends_with(suffix) {
        return false;
    }
    // It also has to sit in the temp folder: a file with that name somewhere else is not
    // ours to remove. The two folders are compared as paths and not as text: the temp
    // folder can be spelled in the short 8.3 form ("C:\Users\PIXELA~1\..."), and the
    // short and the long form of a name are the same folder.
    let Some(parent) = path.parent() else {
        return false;
    };
    same_folder(parent, &std::env::temp_dir())
}

/// Is this the temp file the catalogue download writes?
fn is_catalog_temp(path: &std::path::Path) -> bool {
    is_temp_of(path, catalog::TEMP_PACK_SUFFIX)
}

/// Is this the temp archive the catalogue download writes for a helper?
fn is_helper_temp(path: &std::path::Path) -> bool {
    is_temp_of(path, catalog::TEMP_HELPER_SUFFIX)
}

/// Are these the same folder? The two paths are canonicalised first, so the short and
/// the long form of a name are the same path. A path that cannot be read is not ours.
fn same_folder(a: &std::path::Path, b: &std::path::Path) -> bool {
    let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) else {
        return false;
    };
    a.to_string_lossy()
        .eq_ignore_ascii_case(b.to_string_lossy().as_ref())
}

/// Rainmeter: guarantee a truly empty layout exists and offer the machine's real layouts.
fn enrich_rainmeter(installed: &mut [packs::Installed]) {
    let Some(empty) = packs::ensure_empty_layout() else {
        return;
    };
    let mut values: Vec<String> = packs::rainmeter_layouts()
        .into_iter()
        .filter(|l| !l.eq_ignore_ascii_case("@Backup"))
        .collect();
    if !values.iter().any(|v| v.eq_ignore_ascii_case(&empty)) {
        values.push(empty.clone());
    }
    values.push("kill".to_string());
    for p in installed.iter_mut() {
        if p.pack.id == "rainmeter" {
            if let Some(setting) = p.pack.settings.first_mut() {
                setting.values = values.clone();
                if setting.default.is_none() {
                    setting.default = Some(empty.clone());
                }
            }
        }
    }
    info!(
        "Rainmeter: empty layout '{empty}' ready; dropdown offers {}",
        values.join(", ")
    );
}

/// What the dashboard needs to know about one installed pack.
pub struct PackMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Which worker owns this pack's jobs (A4): a mode runs on the mode worker.
    pub kind: PackKind,
    pub settings: Vec<armoury_core::pack::Setting>,
}

fn build_catalog(installed: &[packs::Installed]) -> Vec<PackMeta> {
    installed
        .iter()
        .map(|p| PackMeta {
            id: p.pack.id.clone(),
            name: p.pack.name.clone(),
            version: p.pack.version.clone(),
            kind: p.pack.kind,
            settings: p.pack.settings.clone(),
        })
        .collect()
}

/// Is this id a mode (a pack the mode worker owns)?
fn is_mode(catalog: &[PackMeta], id: &str) -> bool {
    catalog.iter().any(|m| m.id == id && m.kind == PackKind::Mode)
}

/// Split a whole-state job's enabled set by kind (A4): each worker only gets the packs of
/// its own kind, so a slow mode cannot stand in front of a pack job on the pack worker.
/// An id that is not in the catalogue keeps its old home (the pack worker).
fn split_enabled(
    enabled: HashMap<String, bool>,
    catalog: &[PackMeta],
) -> (HashMap<String, bool>, HashMap<String, bool>) {
    let mut pack = HashMap::new();
    let mut mode = HashMap::new();
    for (id, on) in enabled {
        if is_mode(catalog, &id) {
            mode.insert(id, on);
        } else {
            pack.insert(id, on);
        }
    }
    (pack, mode)
}

/// A4: merge one half's outcome into the applied state and the journal. Everything the
/// outcome was scoped to belongs to it — apply takes the ids it reports and its
/// captures, a revert hands the scope back. Two halves therefore merge in either arrival
/// order, and a whole-state outcome (the single-worker scope) lands exactly where the
/// old replace/clear put it.
fn merge_scoped(
    applied: &mut Vec<String>,
    captured: &mut HashMap<String, HashMap<String, String>>,
    o: &packs::PackOutcome,
) {
    applied.retain(|p| !o.scoped.contains(p));
    captured.retain(|k, _| !o.scoped.contains(k));
    if o.applied {
        // keep the journal so the matching revert restores the right state
        applied.extend(o.applied_ids.iter().cloned());
        captured.extend(o.captured.clone());
    }
}

struct App {
    cfg: AppConfig,
    /// Tray menu item whose label flips between Pause/Resume.
    pause_item: tray_icon::menu::MenuItem,
    /// Tray menu item that toggles idle mode (the escape hatch when the UI is unreachable).
    idle_item: tray_icon::menu::MenuItem,
    /// Tray menu item that toggles the killswitch.
    kill_item: tray_icon::menu::MenuItem,
    /// Owned here so the icon can be swapped while the killswitch is active.
    tray: Option<TrayIcon>,
    icon_normal: tray_icon::Icon,
    icon_active: tray_icon::Icon,
    /// What the killswitch hid/muted.
    killswitch: killswitch::Ledger,
    /// Tracked key -> live pids (needed to hide/mute mid-killswitch starts).
    running_pids: HashMap<String, Vec<u32>>,
    /// Idle overlay (black screens + muted output devices).
    overlay: overlay::Overlay,
    /// Tick count when idle mode started, so any later input dismisses it.
    idle_engage_tick: u32,
    idle_deadline: Option<Instant>,
    /// Last "still idle" heartbeat, so the log proves the loop is alive while idle.
    last_idle_beat: Option<Instant>,
    /// Pack search order (install folder first) — reused when packs are installed/removed.
    search: Vec<std::path::PathBuf>,
    /// Jobs to the pack worker (apply/revert). Packs and modes have separate workers (A4);
    /// the mode's channel carries the same job type, so a half of a transition is
    /// indistinguishable from a whole job.
    pack_tx: crossbeam_channel::Sender<packs::PackJob>,
    mode_tx: crossbeam_channel::Sender<packs::PackJob>,
    /// Both workers report through one channel: an outcome carries the ids it was
    /// scoped to, and the daemon merges the halves (A4).
    pack_out_rx: crossbeam_channel::Receiver<packs::PackOutcome>,
    pack_catalog: Vec<PackMeta>,
    /// Packs currently in the applied (game-mode) state.
    applied_packs: Vec<String>,
    /// Journal: per-pack values captured at apply time, replayed on revert.
    captured_by_pack: HashMap<String, HashMap<String, String>>,
    /// A pack removal that is waiting for its revert to finish: (pack id, requesting client).
    pending_removal: Option<(String, u64)>,
    /// Shared with the watcher thread (single source of truth for what to watch).
    tracked: Arc<Mutex<Vec<TrackedApp>>>,
    /// Tracked key (exe file name, lowercase) -> last resolved image path, for pack templates.
    running_paths: HashMap<String, String>,
    engine: Engine,
    started: Instant,
    clients: HashMap<u64, crossbeam_channel::Sender<Response>>,
    ui_conn: Option<u64>,
    ui_child: Option<Child>,
    spawning_ui: bool,
    /// The console-launcher process (spawned by a fresh pad connect or from the tray).
    console_child: Option<Child>,
    /// When the console launcher was spawned: its band needs a moment to come up, so a
    /// launcher without a window is only stale once it is this old.
    console_spawned: Option<Instant>,
    /// Pad hotplug events from the controller watch.
    pad_rx: crossbeam_channel::Receiver<console::PadEvent>,
    /// Cover-art passes: worker -> main loop (a ready key, or "" when a pass finished).
    art_tx: crossbeam_channel::Sender<String>,
    art_rx: crossbeam_channel::Receiver<String>,
    /// The picker's answers: the photos of a page, or the result of a saved pick.
    art_query_tx: crossbeam_channel::Sender<ArtAnswer>,
    art_query_rx: crossbeam_channel::Receiver<ArtAnswer>,
    art_busy: bool,
    /// Launch results: the worker thread -> the main loop (the client gets the answer).
    launch_tx: crossbeam_channel::Sender<LaunchEvent>,
    launch_rx: crossbeam_channel::Receiver<LaunchEvent>,
    /// A pass with the old (keyless) SteamGridDB key is still running: it leaves miss markers
    /// behind, so they have to be cleared again when that pass ends.
    art_retry_after_pass: bool,
    /// Another art pass is needed when the running one ends (a slot went back to automatic).
    art_pass_again: bool,
    /// The last failure the dashboard shows (`None` = nothing went wrong).
    last_error: Option<LastError>,
    /// Set by the tray menu's "Quit" and by a `quit` request over the pipe.
    quit_requested: QuitRequest,
    last_broadcast: Instant,
    /// Last text set on the tray tooltip (avoid a Win32 call every second).
    last_tooltip: String,
    /// The wake event every producer signals (handed to each pack worker).
    wake: usize,
}

/// The daemon's exit request. The tray menu's "Quit" and a `quit` request over the pipe both
/// set it; the main loop then runs the shutdown sequence (revert the packs, restore the
/// audio, exit).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct QuitRequest {
    asked: bool,
}

impl QuitRequest {
    /// Ask for the shutdown.
    fn ask(&mut self) {
        self.asked = true;
    }

    /// Has the shutdown been asked for?
    fn asked(&self) -> bool {
        self.asked
    }
}

/// The slot's name for the log.
fn slot_name(slot: ArtSlot) -> &'static str {
    match slot {
        ArtSlot::Icon => "icon",
        ArtSlot::Background => "background",
    }
}

/// What an artwork request's worker thread answered.
enum ArtAnswer {
    /// The photos of one page of one slot.
    Listing { client: u64, listing: ArtListing },
    /// The result of a pick: did the file land?
    Saved { client: u64, ok: bool, message: String },
}

/// The launcher entry for an exe, as the art fetcher wants it: the name and the path.
///
/// The name comes from the launcher's list — it is what the search is built from. An app
/// that is not in the list falls back to the file name.
fn art_want(cfg: &AppConfig, exe: &str) -> art::Want {
    let name = launcher_entries(cfg)
        .into_iter()
        .find(|(_, e)| e.eq_ignore_ascii_case(exe))
        .map(|(n, _)| n)
        .unwrap_or_else(|| {
            std::path::Path::new(exe)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| exe.to_string())
        });
    art::Want {
        key: art::key(exe),
        name,
        exe: exe.to_string(),
    }
}

/// The console launcher's entry list: tracked apps first, then console-only pins (deduped by
/// exe base name) — the same merge the console itself performs. Hidden tracked apps stay out.
fn launcher_entries(cfg: &AppConfig) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for a in &cfg.tracked_apps {
        if cfg.console_hidden.contains(&basename_lower(&a.exe)) {
            continue; // hidden from the band only: the daemon still tracks it
        }
        out.push((a.name.clone(), a.exe.clone()));
    }
    for a in &cfg.console_apps {
        let base = basename_lower(&a.exe);
        if !out.iter().any(|(_, exe)| basename_lower(exe) == base) {
            out.push((a.name.clone(), a.exe.clone()));
        }
    }
    out
}

/// What a finished launch means for the daemon.
enum LaunchAction {
    /// The game started: remember it as played.
    Played,
    /// The launch failed: show the reason, remember nothing.
    Failed(String),
}

/// The action a finished launch leads to. A launch that failed must not count as played:
/// the "last played" stamp re-sorts the band, and the user would keep landing on the game
/// that never even started.
fn launch_action(outcome: &LaunchOutcome) -> LaunchAction {
    if outcome.ok {
        LaunchAction::Played
    } else {
        LaunchAction::Failed(outcome.message.clone())
    }
}

/// What the daemon does with the console launcher it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsoleAction {
    /// The launcher is alive and its band is on screen: keep it.
    Keep,
    /// The launcher is gone: forget the handle.
    Clear,
    /// The launcher is alive but its band is gone: kill it and start a new one.
    Replace,
}

/// What to do with the console launcher the daemon holds.
///
/// `pid` is the launcher's process id (0 when there is none). `exited` says the process is
/// known to have finished; `window_gone` says its band is gone — closed, hidden or
/// minimised (the OS parks a minimised band at (-32000, -32000)). A launcher that is
/// alive but has no band can never be used again, so it is replaced instead of kept:
/// keeping it would turn the tray's "Console mode" into a no-op.
fn console_action(pid: u32, exited: bool, window_gone: bool) -> ConsoleAction {
    if pid == 0 || exited {
        ConsoleAction::Clear
    } else if window_gone {
        ConsoleAction::Replace
    } else {
        ConsoleAction::Keep
    }
}

/// Does the process still show a window? False when the window is gone, hidden or
/// minimised: a launcher in that state is alive but useless.
fn console_window_up(pid: u32) -> bool {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    };

    /// What the enumeration is looking for.
    struct Search {
        pid: u32,
        found: bool,
    }

    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        // SAFETY: the parameter is the search the caller set up, and it outlives the
        // enumeration.
        let search = unsafe { &mut *(param as *mut Search) };
        let mut owner = 0u32;
        // SAFETY: the window handle comes from EnumWindows and the pid is a plain u32.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut owner);
        }
        if owner == search.pid {
            // SAFETY: the window handle comes from EnumWindows.
            if unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 } {
                search.found = true;
                return 0; // the band is up: stop looking
            }
        }
        1 // keep going
    }

    let mut search = Search { pid, found: false };
    // SAFETY: the callback only writes into the search, which lives until the call
    // returns.
    unsafe {
        EnumWindows(Some(visit), &mut search as *mut Search as LPARAM);
    }
    search.found
}

/// ---------------------------------------------------------------------------
/// Starting a game
/// ---------------------------------------------------------------------------

/// How an app is started.
enum LaunchPlan {
    /// A Steam app: the Steam client starts it (`steam.exe -applaunch <appid>`).
    Steam {
        appid: u32,
        steam_exe: std::path::PathBuf,
        steam_dir: std::path::PathBuf,
        /// The install root — its name is the game's name in Steam.
        install_dir: std::path::PathBuf,
    },
    /// A standalone exe: it is started directly.
    Direct { exe: std::path::PathBuf },
}

/// The launch plan for an exe: through Steam when it is an installed Steam app, directly
/// otherwise.
///
/// A Steam game must be started by the Steam client. The exe in the install folder is the
/// innermost payload of a launcher chain (the BattlEye wrapper, the uplay loader, the game
/// itself); started on its own it dies within a second, because the data it needs lives in
/// the install root and the chain never ran. Steam runs the chain with the install root as
/// the working directory, so that is what the daemon does too.
fn launch_plan(exe: &std::path::Path, app: Option<art::SteamApp>) -> LaunchPlan {
    match app {
        Some(app) => LaunchPlan::Steam {
            appid: app.appid,
            steam_exe: app.steam_exe(),
            steam_dir: app.steam_dir,
            install_dir: app.install_dir,
        },
        None => LaunchPlan::Direct {
            exe: exe.to_path_buf(),
        },
    }
}

/// The result of a launch attempt.
struct LaunchOutcome {
    /// Did the app start?
    ok: bool,
    /// What the log says and what the band shows.
    message: String,
}

/// A finished launch on its way back to the client that asked for it.
struct LaunchEvent {
    /// The pipe client that asked for the launch.
    client: u64,
    /// The exe that was launched.
    exe: String,
    /// How it went.
    outcome: LaunchOutcome,
}

/// Start an app and find out whether it really started.
fn run_launch(exe: &str) -> LaunchOutcome {
    let path = std::path::Path::new(exe);
    match launch_plan(path, art::steam_app(path)) {
        LaunchPlan::Steam {
            appid,
            steam_exe,
            steam_dir,
            install_dir,
        } => {
            if !steam_exe.is_file() {
                warn!(
                    "console: no Steam client at {} — starting the exe directly",
                    steam_exe.display()
                );
                return launch_direct(path);
            }
            let game = install_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| exe.to_string());
            launch_via_steam(appid, &steam_exe, &steam_dir, &game)
        }
        LaunchPlan::Direct { exe } => launch_direct(&exe),
    }
}

/// Start the exe itself and check that it stays up.
///
/// The working directory is the exe's own folder, like Explorer does it: plenty of games
/// (and repack launchers) resolve their data files relative to it, and inheriting the
/// daemon's folder breaks them.
fn launch_direct(exe: &std::path::Path) -> LaunchOutcome {
    let mut cmd = Command::new(exe);
    if let Some(dir) = exe.parent().filter(|d| !d.as_os_str().is_empty()) {
        cmd.current_dir(dir);
    }
    cmd.creation_flags(GAME_FLAGS);
    match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            thread::sleep(LAUNCH_GRACE);
            match child.try_wait() {
                Ok(None) => LaunchOutcome {
                    ok: true,
                    message: format!("launched {} (pid {pid})", exe.display()),
                },
                Ok(Some(status)) => LaunchOutcome {
                    ok: false,
                    message: format!(
                        "{} exited right away ({}); the game did not start",
                        exe.display(),
                        exit_code(&status)
                    ),
                },
                Err(e) => LaunchOutcome {
                    ok: true,
                    message: format!("launched {} (pid {pid}, state unknown: {e})", exe.display()),
                },
            }
        }
        Err(e) => LaunchOutcome {
            ok: false,
            message: format!("cannot start {}: {e}", exe.display()),
        },
    }
}

/// Start a Steam game the way Steam starts it: `steam.exe -applaunch <appid>`, with the
/// Steam directory as the working directory.
///
/// The client hands the request to the running instance and leaves, so the game is never a
/// child of the daemon. The game itself is not waited for: Steam shows its own screens
/// (updates, the uplay login) long before the process appears.
fn launch_via_steam(
    appid: u32,
    steam_exe: &std::path::Path,
    steam_dir: &std::path::Path,
    game: &str,
) -> LaunchOutcome {
    let mut cmd = Command::new(steam_exe);
    cmd.arg("-applaunch").arg(appid.to_string());
    cmd.current_dir(steam_dir);
    cmd.creation_flags(GAME_FLAGS);
    match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            thread::sleep(LAUNCH_GRACE);
            match child.try_wait() {
                // the client is still up: it is starting the game
                Ok(None) => LaunchOutcome {
                    ok: true,
                    message: format!("Steam is starting {game} (appid {appid}, client pid {pid})"),
                },
                // the client took the request and left: the game starts on its own
                Ok(Some(status)) if status.success() => LaunchOutcome {
                    ok: true,
                    message: format!("Steam took the launch of {game} (appid {appid})"),
                },
                Ok(Some(status)) => LaunchOutcome {
                    ok: false,
                    message: format!(
                        "Steam refused the launch of {game} (appid {appid}, {})",
                        exit_code(&status)
                    ),
                },
                Err(e) => LaunchOutcome {
                    ok: true,
                    message: format!("Steam is starting {game} (appid {appid}, state unknown: {e})"),
                },
            }
        }
        Err(e) => LaunchOutcome {
            ok: false,
            message: format!("cannot start Steam ({}): {e}", steam_exe.display()),
        },
    }
}

/// The exit code of a finished child, for the failure message.
fn exit_code(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "no exit code".to_string(),
    }
}

/// Wake the main loop: the raw Win32 event handle every producer signals.
fn signal_wake(wake: usize) {
    if wake != 0 {
        unsafe { SetEvent(wake as HANDLE) };
    }
}

impl App {
    fn status(&self) -> StatusSnapshot {
        let now = Instant::now();
        let (state, trigger_source) = match self.engine.state(now) {
            EngineState::Idle => (RunState::Idle, None),
            EngineState::Triggered => (RunState::Triggered, self.engine.source()),
            EngineState::Cooldown => (RunState::Cooldown, self.engine.source()),
        };
        StatusSnapshot {
            daemon_version: armoury_core::VERSION.to_string(),
            uptime_secs: self.started.elapsed().as_secs(),
            state,
            trigger_source,
            cooldown_remaining_ms: self
                .engine
                .cooldown_remaining(now)
                .map(|d| d.as_millis() as u64),
            // What the window is holding back, if anything: the engine knows the change
            // that is waiting for the lockout to end, and the dashboard says so.
            cooldown_action: self.engine.pending_change().map(|t| match t {
                Transition::Applied => "apply".to_string(),
                Transition::Reverted => "revert".to_string(),
            }),
            cooldown_seconds: self.cfg.cooldown_seconds,
            paused: self.cfg.paused,
            killswitch_active: self.killswitch.is_active(),
            // The apps the killswitch is hiding/muting right now — from its own ledger, not
            // from the list of running processes: an app that is merely running is not
            // hidden, and a released killswitch hides nothing at all.
            killswitch_count: self.killswitch.affected_apps() as u32,
            killswitch_enabled: self.cfg.killswitch_enabled,
            idle_active: self.overlay.is_active(),
            idle_monitors: self.overlay.monitor_count() as u32,
            idle_devices: self.overlay.muted_devices() as u32,
            idle_failsafe_enabled: self.cfg.idle_failsafe_enabled,
            idle_failsafe_minutes: self.cfg.idle_failsafe_minutes,
            tracked_apps: self.cfg.tracked_apps.clone(),
            tracked_apps_running: self.engine.running_names(),
            console_open: self.cfg.console_open.clone(),
            console_apps: self.cfg.console_apps.clone(),
            console_hidden: self.cfg.console_hidden.clone(),
            last_played: self.cfg.last_played.clone(),
            sgdb_api_key_set: self.cfg.sgdb_api_key.is_some(),
            // The key never travels over the pipe: the snapshot says whether a key is set and
            // nothing more. The field stays in the wire format, so an older UI still loads.
            sgdb_api_key: String::new(),
            packs: self
                .pack_catalog
                .iter()
                .map(|m| {
                    let user = self.cfg.packs.get(&m.id);
                    let enabled = user.map(|u| u.enabled).unwrap_or(false);
                    let settings = m
                        .settings
                        .iter()
                        .map(|s| PackSettingStatus {
                            key: s.key.clone(),
                            label: s.label.clone(),
                            values: s.values.clone(),
                            value: Some(
                                user.and_then(|u| u.values.get(&s.key).cloned())
                                    .or_else(|| s.default.clone())
                                    .unwrap_or_default(),
                            ),
                        })
                        .collect();
                    PackStatus {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        version: m.version.clone(),
                        enabled,
                        settings,
                        applied: self.applied_packs.contains(&m.id),
                    }
                })
                .collect(),
            last_error: self.last_error.clone(),
        }
    }

    /// Start an app and report the result to the client that asked for it.
    ///
    /// The work happens on a worker thread: the check waits for the process to stay up,
    /// and the daemon's main loop must not block on that. For a tracked app the watcher
    /// then triggers the packs exactly as if the user had opened the game by hand.
    fn launch_app(&mut self, client: u64, exe: String) {
        let tx = self.launch_tx.clone();
        let wake = self.wake;
        thread::spawn(move || {
            let outcome = run_launch(&exe);
            let _ = tx.send(LaunchEvent {
                client,
                exe,
                outcome,
            });
            signal_wake(wake);
        });
    }

    /// The launch finished: the client that asked gets the answer, and only a game that
    /// really started counts as played.
    fn on_launch(&mut self, ev: LaunchEvent) {
        match launch_action(&ev.outcome) {
            LaunchAction::Played => {
                info!("console: {}", ev.outcome.message);
                self.record_played(&ev.exe);
            }
            LaunchAction::Failed(message) => {
                warn!("console: {message}");
                remember_error(&mut self.last_error, "launch", &message);
            }
        }
        if let Some(push) = self.clients.get(&ev.client) {
            let _ = push.send(armoury_core::protocol::launch_ack(
                ev.outcome.ok,
                &ev.outcome.message,
            ));
        }
        self.broadcast_status();
    }

    fn on_watch(&mut self, ev: watch::WatchEvent) {
        let now = Instant::now();
        let transition = match ev {
            watch::WatchEvent::Started {
                key,
                display,
                path,
                pid,
            } => {
                self.running_paths.insert(key.clone(), path);
                self.running_pids.entry(key.clone()).or_default().push(pid);
                self.record_played(&key); // recency order for the console band
                if self.killswitch.is_active() {
                    // a tracked app started while the killswitch is on: hide + mute it too
                    info!("killswitch: {display} started while active — hiding it");
                    killswitch::engage(&[pid], &mut self.killswitch);
                    self.broadcast_status();
                }
                self.engine.on_started(&key, &display, pid, now)
            }
            watch::WatchEvent::Stopped { key, pid } => {
                if let Some(list) = self.running_pids.get_mut(&key) {
                    list.retain(|p| *p != pid);
                }
                if !self.engine.is_running(&key) {
                    self.running_paths.remove(&key);
                    self.running_pids.remove(&key);
                }
                self.engine.on_stopped(&key, pid, now)
            }
        };
        if let Some(t) = transition {
            self.on_transition(t);
        }
    }

    fn on_transition(&mut self, t: Transition) {
        self.send_transition(t, false);
    }

    /// `force` = a manual action (entering or leaving idle, removing the last tracked app)
    /// that must take effect even while the daemon is paused (user decision 2026-10-18: pause
    /// stops *automatic* switching, not the user's own actions).
    fn send_transition(&mut self, t: Transition, force: bool) {
        if !transition_allowed(self.cfg.paused, force) {
            info!("paused — skipping the {:?} transition", t);
            // The packs were not changed: the engine must not believe that they were, or the
            // next trigger would find "already applied" and leave the machine wrong.
            self.engine.transition_not_performed(t);
            self.broadcast_status();
            return;
        }
        // Hand the transition to the workers. A4: split the enabled set by kind — the pack
        // half on the pack worker, the mode half on the mode worker — and pass `values` and
        // `captured` whole to both (`run_job` looks them up per pack id).
        let job = packs::PackJob {
            apply: t == Transition::Applied,
            tag: None,
            enabled: self
                .cfg
                .packs
                .iter()
                .map(|(k, v)| (k.clone(), v.enabled))
                .collect(),
            values: self
                .cfg
                .packs
                .iter()
                .map(|(k, v)| (k.clone(), v.values.clone()))
                .collect(),
            captured: self.captured_by_pack.clone(),
        };
        self.send_by_kind(job);
        match t {
            Transition::Applied => info!(
                "packs applied (triggered by {})",
                self.engine.source().unwrap_or_default()
            ),
            Transition::Reverted => info!("packs reverted"),
        }
        self.broadcast_status();
    }

    /// A4: a whole-state job goes out as two halves, one per worker — the pack half on the
    /// pack worker, the mode half on the mode worker. `values` and `captured` stay whole
    /// (`run_job` looks them up per pack id), and each outcome echoes the ids its half was
    /// scoped to, so the daemon merges both halves order-independently.
    fn send_by_kind(&self, job: packs::PackJob) {
        let packs::PackJob { apply, tag, enabled, values, captured } = job;
        let (pack_enabled, mode_enabled) = split_enabled(enabled, &self.pack_catalog);
        let _ = self.pack_tx.send(packs::PackJob {
            apply,
            tag: tag.clone(),
            enabled: pack_enabled,
            values: values.clone(),
            captured: captured.clone(),
        });
        // The mode half is skipped when no mode is part of the job: waking a worker to
        // report that everything is disabled would only add an empty outcome to merge.
        if mode_enabled.values().any(|v| *v) {
            let _ = self
                .mode_tx
                .send(packs::PackJob { apply, tag, enabled: mode_enabled, values, captured });
        }
    }

    /// The worker that owns this pack's jobs (A4): a tagged, single-pack operation (a
    /// disable/remove of something currently applied) goes to that pack's own worker.
    fn worker_for(&self, id: &str) -> &crossbeam_channel::Sender<packs::PackJob> {
        if is_mode(&self.pack_catalog, id) { &self.mode_tx } else { &self.pack_tx }
    }

    fn set_paused(&mut self, paused: bool) {
        if self.cfg.paused == paused {
            return;
        }
        self.cfg.paused = paused;
        self.pause_item
            .set_text(if paused { "Resume all packs" } else { "Pause all packs" });
        if let Err(e) = armoury_core::config::save(&self.cfg) {
            warn!("failed to save config: {e}");
        }
        info!(
            "packs {} (all switching {})",
            if paused { "paused" } else { "resumed" },
            if paused { "suppressed" } else { "active" }
        );
        // Freeze/thaw the engine itself, so a pause never leaves a phantom lockout behind and
        // resuming catches up with whatever is running (2026-10-18).
        if let Some(t) = self.engine.set_paused(paused, Instant::now()) {
            self.on_transition(t);
        }
        self.broadcast_status();
    }

    fn on_pack_outcome(&mut self, o: packs::PackOutcome) {
        // The dashboard shows the most recent failure; a clean run clears it again.
        for (id, err) in &o.failed {
            warn!("pack {id} failed: {err}");
            remember_error(&mut self.last_error, &format!("pack:{id}"), err);
        }
        if o.failed.is_empty() {
            forget_error(&mut self.last_error, "pack:");
        }

        if let Some(tag) = o.tag.clone() {
            // A single-pack operation (removing an applied pack): merge instead of replacing.
            self.applied_packs.retain(|p| p != &tag);
            self.captured_by_pack.remove(&tag);
            for line in &o.succeeded {
                info!("pack {line}");
            }
            let failed = o.failed.first().map(|(_, e)| e.clone());
            if self.pending_removal.as_ref().map(|(id, _)| id == &tag) == Some(true) {
                let (_, client) = self.pending_removal.take().unwrap();
                self.finish_removal(&tag, client, failed);
            } else {
                self.write_journal(&o, false);
                self.broadcast_status();
            }
            return;
        }

        // A4: merges the two halves of a whole-state job order-independently. Each half
        // echoes the ids its job was scoped to, so one half never touches the other's part
        // of the applied state or the journal — and for the common single-worker case the
        // scope is everything, which lands exactly where the old replace/clear put it.
        merge_scoped(&mut self.applied_packs, &mut self.captured_by_pack, &o);
        for line in &o.succeeded {
            info!("pack {line}");
        }
        self.write_journal(&o, true);
        self.broadcast_status();
    }

    /// The journal is also the restart record: it must carry everything a revert needs
    /// (which packs, their dropdown values and the values captured at apply time).
    /// `whole_state` = this outcome describes the full applied set (a transition or a global
    /// revert); a single-pack outcome only updates that pack's row.
    fn write_journal(&self, o: &packs::PackOutcome, whole_state: bool) {
        if !whole_state {
            // a single-pack operation is not the whole applied state — leave the journal alone
            return;
        }
        let path = armoury_core::config::config_dir().join("journal.json");
        let entry = serde_json::json!({
            "at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "applied": o.applied,
            "trigger": self.engine.source(),
            "packs": self.applied_packs,
            "values": o.values,
            "captured": self.captured_by_pack,
            "failed": o
                .failed
                .iter()
                .map(|(id, e)| serde_json::json!({ "id": id, "error": e }))
                .collect::<Vec<_>>(),
        });
        let mut log: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        log.push(entry);
        if log.len() > 200 {
            let drop = log.len() - 200;
            log.drain(0..drop);
        }
        if let Ok(text) = serde_json::to_string_pretty(&log) {
            // atomic-ish write: a truncated journal would silently skip the restart revert
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Remove a pack folder now that its (optional) revert has finished.
    fn finish_removal(&mut self, pack_id: &str, client: u64, revert_error: Option<String>) {
        match packs::remove_id(&self.search, pack_id) {
            Ok(removed) => {
                self.reload_packs();
                let ok = revert_error.is_none();
                let message = match &revert_error {
                    Some(e) => format!("{removed} removed, but reverting it failed: {e}"),
                    None => format!("{removed} removed"),
                };
                info!("pack op: {message}");
                self.reply_pack_op(client, ok, message);
            }
            Err(e) => {
                warn!("pack remove failed: {e}");
                self.reply_pack_op(client, false, e);
            }
        }
    }

    /// Re-read the packs folders (after an install/remove) and give the workers the new
    /// list. Dropping the old senders makes the old worker threads exit on their own.
    fn reload_packs(&mut self) {
        let mut installed = packs::load(&self.search);
        enrich_rainmeter(&mut installed);
        self.pack_catalog = build_catalog(&installed);
        let (tx, rx) = crossbeam_channel::unbounded::<packs::PackJob>();
        let (modex, modex_rx) = crossbeam_channel::unbounded::<packs::PackJob>();
        let (out_tx, out_rx) = crossbeam_channel::unbounded::<packs::PackOutcome>();
        packs::start(installed, rx, modex_rx, out_tx, self.wake);
        self.pack_tx = tx;
        self.mode_tx = modex;
        self.pack_out_rx = out_rx;
        info!("pack list reloaded: {} installed", self.pack_catalog.len());
        self.broadcast_status();
    }

    fn reply_pack_op(&self, client: u64, ok: bool, message: String) {
        if let Some(push) = self.clients.get(&client) {
            let _ = push.send(Response::PackOp { ok, message });
        }
    }

    fn set_pack_enabled(&mut self, id: String, enabled: bool) {
        if !self.pack_catalog.iter().any(|m| m.id == id) {
            warn!("unknown pack '{id}'");
            return;
        }
        let entry = self.cfg.packs.entry(id.clone()).or_default();
        if entry.enabled == enabled {
            return;
        }
        entry.enabled = enabled;
        if let Err(e) = armoury_core::config::save(&self.cfg) {
            warn!("failed to save config: {e}");
        }
        info!("pack {id} {}", if enabled { "enabled" } else { "disabled" });

        if enabled {
            // Flipping a switch mid-session should act now, not at some future trigger.
            self.apply_now_if_triggered(&format!("pack {id} enabled"));
        } else if self.applied_packs.iter().any(|p| p == &id) {
            // Disabling something that is currently applied puts that app back straight away.
            if let Some(captured) = self.captured_by_pack.get(&id).cloned() {
                let job = packs::PackJob {
                    apply: false,
                    tag: Some(id.clone()),
                    enabled: HashMap::from([(id.clone(), true)]),
                    values: self
                        .cfg
                        .packs
                        .get(&id)
                        .map(|v| HashMap::from([(id.clone(), v.values.clone())]))
                        .unwrap_or_default(),
                    captured: HashMap::from([(id.clone(), captured)]),
                };
                info!("pack {id} is applied — reverting it now that it is disabled");
                let sender = self.worker_for(&id);
                let _ = sender.send(job);
            }
        }
        self.broadcast_status();
    }

    fn set_pack_value(&mut self, id: String, key: String, value: String) {
        if !self.pack_catalog.iter().any(|m| m.id == id) {
            warn!("unknown pack '{id}'");
            return;
        }
        let entry = self.cfg.packs.entry(id.clone()).or_default();
        entry.values.insert(key.clone(), value.clone());
        if let Err(e) = armoury_core::config::save(&self.cfg) {
            warn!("failed to save config: {e}");
        }
        info!("pack {id}: {key} = {value}");
        self.apply_now_if_triggered(&format!("pack {id} changed to {value}"));
        self.broadcast_status();
    }

    /// A pack was enabled or re-configured while a tracked app is already running: apply it now
    /// instead of waiting for some future trigger (what a user expects when flipping a switch
    /// mid-session — the 2026-10-17 test round). The captures from the original trigger are kept,
    /// so a later revert still restores the true pre-trigger state.
    fn apply_now_if_triggered(&mut self, why: &str) {
        if self.cfg.paused || self.overlay.is_active() || !self.engine.is_applied() {
            return; // paused / idle / nothing running: the next trigger picks the change up
        }
        let job = packs::PackJob {
            apply: true,
            tag: None,
            enabled: self
                .cfg
                .packs
                .iter()
                .map(|(k, v)| (k.clone(), v.enabled))
                .collect(),
            values: self
                .cfg
                .packs
                .iter()
                .map(|(k, v)| (k.clone(), v.values.clone()))
                .collect(),
            captured: self.captured_by_pack.clone(),
        };
        self.send_by_kind(job);
        info!("packs applied now ({why}, a tracked app is already running)");
    }

    fn add_tracked_app(&mut self, name: String, exe: String) {
        // Matching is by file name (the user's choice), so dedupe on that too.
        let base = basename_lower(&exe);
        if self
            .cfg
            .tracked_apps
            .iter()
            .any(|a| basename_lower(&a.exe) == base)
        {
            info!("tracked app already in the list (by name): {base}");
            return;
        }
        self.cfg.tracked_apps.push(TrackedApp {
            name,
            exe: exe.clone(),
        });
        info!("tracked app added: {exe}");
        self.save_and_share_tracked();
    }

    fn remove_tracked_app(&mut self, exe: &str) {
        // Matching is by file name, like the add path and the watcher: a client that sends
        // back a rebuilt path removes the app just the same.
        let base = basename_lower(exe);
        let removed = remove_tracked_apps(&mut self.cfg.tracked_apps, exe);
        if removed.is_empty() {
            info!("tracked app not in the list (by name): {base}");
            return;
        }
        info!("tracked app removed: {base}");
        self.save_and_share_tracked();
        // The entry is gone, so nothing holds the packs applied any more: removing the last
        // tracked app reverts the packs at once. It is a manual action, so it bypasses the
        // lockout (and the pause, G12) and leaves no lockout behind — the next tracked app
        // may apply right away.
        if let Some(t) = self.engine.on_untracked(&base, Instant::now()) {
            info!("tracked app removed — reverting the packs now");
            self.send_transition(t, true);
        }
    }

    fn save_and_share_tracked(&mut self) {
        if let Err(e) = armoury_core::config::save(&self.cfg) {
            warn!("failed to save config: {e}");
        }
        if let Ok(mut list) = self.tracked.lock() {
            *list = self.cfg.tracked_apps.clone();
        }
        self.broadcast_status();
    }

    /// The low-level keyboard hook saw a hotkey. The hook only forwards the key — the work
    /// happens here, on the event-loop thread.
    fn on_hotkey(&mut self, vk: u32) {
        match hotkey::action_for(vk) {
            Some(hotkey::Action::Killswitch) => {
                info!("killswitch hotkey (End)");
                self.toggle_killswitch();
            }
            Some(hotkey::Action::Idle) => {
                if self.overlay.is_active() {
                    info!("idle mode off (Home hotkey)");
                    self.exit_idle();
                } else {
                    info!("idle mode on (Home hotkey)");
                    self.enter_idle();
                }
            }
            None => {}
        }
    }

    fn enter_idle(&mut self) {
        if self.overlay.is_active() {
            return;
        }
        // marker first: if we crash while idle, the next start can restore the audio
        let _ = std::fs::write(overlay::marker_path(), b"idle");
        overlay::show(&mut self.overlay);
        if !self.overlay.is_active() {
            // The black windows could not be created. Do not cancel pack switching in that case:
            // the user would have no way out of a mode that is not actually up.
            warn!("idle overlay could not be shown — leaving pack switching alone");
            let _ = std::fs::remove_file(overlay::marker_path());
            return;
        }
        self.idle_engage_tick = tick_count();
        self.last_idle_beat = Some(Instant::now());
        // Drain the "pressed since last poll" flags so only keys pressed from now on wake us.
        let _ = keyboard_pressed_since_last_poll();
        self.idle_item.set_text("Wake up");
        self.idle_deadline = if self.cfg.idle_failsafe_enabled {
            Some(Instant::now() + Duration::from_secs(self.cfg.idle_failsafe_minutes.max(1) * 60))
        } else {
            None
        };
        // Idle cancels switching: whatever is applied reverts now (decision B8). This is a manual
        // action, so it happens even while paused.
        if let Some(t) = self.engine.set_idle(true) {
            info!("idle mode: tracked apps are normal apps again");
            self.send_transition(t, IDLE_IS_FORCED);
        }
        self.broadcast_status();
    }

    fn exit_idle(&mut self) {
        if !self.overlay.is_active() {
            info!("idle mode is not up — nothing to leave");
            return;
        }
        overlay::hide(&mut self.overlay);
        let _ = std::fs::remove_file(overlay::marker_path());
        self.idle_deadline = None;
        self.last_idle_beat = None;
        self.idle_item.set_text("Idle mode");
        // Idle over: re-apply if a tracked app is running at this moment (decision B8). Like
        // entering idle, this is a manual action: it goes through even while the daemon is
        // paused, so the packs are not left reverted after the user leaves idle mode.
        if let Some(t) = self.engine.set_idle(false) {
            info!("idle mode over: a tracked app is running, applying the packs again");
            self.send_transition(t, IDLE_IS_FORCED);
        }
        self.broadcast_status();
    }

    /// Leaving idle mode on the way out. The black windows vanish with the process, but the
    /// mutes live on the device: they have to be undone here, or the machine stays silent
    /// until the daemon starts again (and the marker file would send the next start down the
    /// crash-recovery path). The packs are *not* re-applied — the daemon is going away.
    fn leave_idle_for_exit(&mut self) {
        if !self.overlay.is_active() {
            return;
        }
        overlay::hide(&mut self.overlay);
        let _ = std::fs::remove_file(overlay::marker_path());
        self.idle_deadline = None;
        self.last_idle_beat = None;
        info!("idle mode left before the exit: the audio is back");
    }

    fn toggle_killswitch(&mut self) {
        if !self.cfg.killswitch_enabled {
            info!("killswitch: disabled in the config — the toggle does nothing");
            return;
        }
        if self.killswitch.is_active() {
            killswitch::disengage(&mut self.killswitch);
            self.set_tray_icon(false);
            self.kill_item.set_text("Toggle killswitch");
        } else {
            let pids: Vec<u32> = self.running_pids.values().flatten().copied().collect();
            if pids.is_empty() {
                info!("killswitch: no tracked app is running — nothing to hide or mute");
                return;
            }
            killswitch::engage(&pids, &mut self.killswitch);
            self.set_tray_icon(true);
            self.kill_item.set_text("Release killswitch");
        }
        self.broadcast_status();
    }

    /// The config-level killswitch switch (`set_killswitch` over the pipe). Turning it off
    /// releases an engaged killswitch first: a switch that reads "off" everywhere must not
    /// leave windows hidden and audio muted behind it.
    fn set_killswitch_config(&mut self, enabled: bool) {
        self.cfg.killswitch_enabled = enabled;
        if !enabled && self.killswitch.is_active() {
            info!("killswitch: the config switch turns off — releasing an engaged killswitch");
            killswitch::disengage(&mut self.killswitch);
            self.set_tray_icon(false);
        }
        self.kill_item.set_text(if enabled {
            "Toggle killswitch"
        } else {
            "Killswitch off in config"
        });
        if let Err(e) = armoury_core::config::save(&self.cfg) {
            warn!("failed to save config: {e}");
        }
        info!("killswitch {} in the config", if enabled { "enabled" } else { "disabled" });
        self.broadcast_status();
    }

    /// The killswitch on the way out. Its hidden windows vanish with the process, but the
    /// mutes live on the device: release here, or the machine stays hidden and silent until
    /// the daemon starts again.
    fn release_killswitch_for_exit(&mut self) {
        if !self.killswitch.is_active() {
            return;
        }
        killswitch::disengage(&mut self.killswitch);
        self.set_tray_icon(false);
        info!("killswitch released before the exit: the windows and audio are back");
    }

    fn set_tray_icon(&self, active: bool) {
        if let Some(tray) = &self.tray {
            let icon = if active {
                self.icon_active.clone()
            } else {
                self.icon_normal.clone()
            };
            if let Err(e) = tray.set_icon(Some(icon)) {
                warn!("tray icon swap failed: {e}");
            }
        }
    }

    /// Called once when the user quits: run the revert and wait (bounded) for it, so the apps are
    /// really back before the daemon exits. Without this the packs stayed applied until the next
    /// start noticed the journal ("Quit means close everything", 2026-10-18).
    fn revert_before_quit(&mut self) {
        if !self.engine.is_applied() {
            return;
        }
        let job = packs::PackJob {
            apply: false,
            tag: None,
            enabled: self
                .cfg
                .packs
                .iter()
                .map(|(k, v)| (k.clone(), v.enabled))
                .collect(),
            values: self
                .cfg
                .packs
                .iter()
                .map(|(k, v)| (k.clone(), v.values.clone()))
                .collect(),
            captured: self.captured_by_pack.clone(),
        };
        info!("quit: reverting the packs before exiting");
        self.send_by_kind(job);

        // Shutdown is not time-critical; the kill/relaunch paths can take a few seconds.
        // A4: the two halves of the revert report separately (a slow mode may take seconds
        // more), so quit waits until the applied set is empty, not just for one outcome.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.pack_out_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(outcome) => {
                    self.on_pack_outcome(outcome);
                    if self.applied_packs.is_empty() {
                        info!("quit: packs reverted, exiting");
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        warn!("quit: the revert did not finish within 15 s — exiting anyway");
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn on_tick(&mut self) {
        if self.overlay.is_active() {
            overlay::pump();
            // A keypress ends idle (keyboard only — a mouse twitch must not wake an AFK session).
            if keyboard_pressed_since_last_poll() {
                info!("idle overlay dismissed by a keypress");
                self.exit_idle();
            } else {
                if self
                    .last_idle_beat
                    .map_or(true, |t| t.elapsed() >= Duration::from_secs(30))
                {
                    self.last_idle_beat = Some(Instant::now());
                    info!(
                        "idle overlay still on (engaged at tick {}) — waiting for a keypress or the failsafe",
                        self.idle_engage_tick
                    );
                }
                if let Some(deadline) = self.idle_deadline {
                    if Instant::now() >= deadline {
                        warn!(
                            "idle failsafe fired after {} min — leaving idle mode",
                            self.cfg.idle_failsafe_minutes
                        );
                        self.exit_idle();
                    }
                }
            }
        }
        if let Some(t) = self.engine.tick(Instant::now()) {
            self.on_transition(t);
        }
        if self.last_broadcast.elapsed() >= Duration::from_secs(1) {
            self.last_broadcast = Instant::now();
            self.update_tooltip();
            self.broadcast_status();
            self.reap_ui_child();
            self.reap_console_child();
        }
    }

    fn broadcast_status(&self) {
        if self.clients.is_empty() {
            return;
        }
        let status = self.status();
        for push in self.clients.values() {
            let _ = push.send(Response::Status {
                status: status.clone(),
            });
        }
    }

    /// Tray tooltip: the product name plus what is happening right now (user request 2026-10-18).
    /// Called from the 1 Hz housekeeping tick; only touches the tray when the text changes.
    fn update_tooltip(&mut self) {
        let now = Instant::now();
        let text = if self.cfg.paused {
            "ArmouryDaemon — paused".to_string()
        } else {
            match self.engine.state(now) {
                EngineState::Idle => "ArmouryDaemon — idle".to_string(),
                EngineState::Triggered => match self.engine.source() {
                    Some(src) => format!("ArmouryDaemon — {src}"),
                    None => "ArmouryDaemon — triggered".to_string(),
                },
                EngineState::Cooldown => match self.engine.cooldown_remaining(now) {
                    Some(left) => format!("ArmouryDaemon — locked for {}s", left.as_secs()),
                    None => "ArmouryDaemon — cooldown".to_string(),
                },
            }
        };
        if text == self.last_tooltip {
            return;
        }
        if let Some(tray) = &self.tray {
            if let Err(e) = tray.set_tooltip(Some(&text)) {
                warn!("tray tooltip update failed: {e}");
                return;
            }
        }
        self.last_tooltip = text;
    }

    fn reap_ui_child(&mut self) {
        if let Some(child) = &mut self.ui_child {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    info!("UI process exited ({exit})");
                    self.ui_child = None;
                    self.spawning_ui = false;
                }
                Ok(None) => {}
                Err(e) => warn!("UI process wait failed: {e}"),
            }
        }
    }

    /// Pad hotplug: a fresh connect opens the console launcher when auto-open is not `never`
    /// and no tracked app is running (user decision 2026-10-19).
    fn on_pad(&mut self, ev: console::PadEvent) {
        match ev {
            console::PadEvent::Connected { slot } => {
                info!("controller connected (slot {slot})");
                self.open_console_if_allowed();
            }
            console::PadEvent::Disconnected { slot } => {
                info!("controller disconnected (slot {slot})");
            }
        }
    }

    /// Open the launcher unless auto-open is `never` or a tracked app is running. Shared by a
    /// fresh connect and by choosing a mode while a pad already sits in a slot — that moment
    /// never produces a connect event (the watcher seeds the current slots at startup).
    fn open_console_if_allowed(&mut self) {
        if self.cfg.console_open == CONSOLE_OPEN_NEVER {
            return;
        }
        self.open_console(ConsoleOpen::Automatic);
    }

    /// Open the band, if the reason allows it.
    ///
    /// `reason` says who asked for it: an automatic open (the daemon started, the machine
    /// resumed from sleep, the open mode changed) is refused while a tracked app is running
    /// — the band would pop up over the game. An explicit open (the tray's "Console mode",
    /// the dashboard's console button) always opens the band.
    fn open_console(&mut self, reason: ConsoleOpen) {
        // Only an automatic open has to look at what is running.
        let tracked_running =
            matches!(reason, ConsoleOpen::Automatic) && self.tracked_app_running();
        if !console_open_allowed(reason, tracked_running) {
            info!("console: kept closed — a tracked app is running");
            return;
        }
        self.show_console();
    }

    /// Is a tracked app running right now?
    ///
    /// The engine knows the apps the watcher has seen. Right after the start it knows none
    /// of them yet — the watcher's first samples reach the daemon through the main loop,
    /// which has not started yet — so the process list is read directly as well.
    fn tracked_app_running(&self) -> bool {
        !self.engine.running_names().is_empty() || tracked_app_running_now(&self.cfg)
    }

    /// Open the console launcher now (tray menu / settings preview / a fresh pad connect).
    fn show_console(&mut self) {
        self.reap_console_child();
        match self.console_child_state() {
            Some((_, ConsoleAction::Keep)) => {
                info!("console launcher is already running");
                return;
            }
            Some((_, ConsoleAction::Clear)) => self.forget_console_child(),
            Some((pid, ConsoleAction::Replace)) => {
                warn!("stale console launcher (pid {pid}): its band is gone — replacing it");
                self.kill_console_child();
            }
            None => {}
        }

        // The console is a **pack**: its exe (`ArmouryConsole.exe`) comes from a catalogue
        // helper installed under `modules/<id>/`, and this binary carries no code that could
        // draw its band (2026-10-27). Without the pack there is no console to show, so say so
        // and leave the machine alone — launching something that is not installed was the old
        // `--console` fallback, and it is gone.
        let module = match console_helper(&pack_search_dirs()) {
            Some(module) => module,
            None => {
                info!(
                    "console launcher: the console pack is not installed — install it from the Packs window"
                );
                return;
            }
        };
        info!(
            "console launcher: using module {} v{} ({})",
            module.id,
            module.version,
            module.path.display()
        );
        let mut command = Command::new(module.path);
        match command.creation_flags(CREATE_NO_WINDOW).spawn() {
            Ok(child) => {
                info!("spawned console launcher (pid {})", child.id());
                self.console_spawned = Some(Instant::now());
                self.console_child = Some(child);
            }
            Err(e) => warn!("failed to spawn the console launcher: {e}"),
        }
    }

    /// What to do with the console launcher the daemon holds, if it holds one. A launcher
    /// is only useful while its band is on screen: one that is alive but has no band was
    /// closed (or minimised) without the process going with it.
    fn console_child_state(&mut self) -> Option<(u32, ConsoleAction)> {
        let started = self.console_spawned;
        let child = self.console_child.as_mut()?;
        let pid = child.id();
        let exited = match child.try_wait() {
            Ok(Some(exit)) => {
                info!("console launcher exited ({exit})");
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!("console launcher wait failed: {e}");
                false
            }
        };
        // A launcher that has just started has no band yet: that is not a stale one.
        let starting = started.map_or(false, |at| at.elapsed() < CONSOLE_GRACE);
        let window_gone = !starting && !console_window_up(pid);
        Some((pid, console_action(pid, exited, window_gone)))
    }

    /// Forget the console launcher: its process is gone and the handle is useless.
    fn forget_console_child(&mut self) {
        self.console_child = None;
        self.console_spawned = None;
    }

    /// Kill the console launcher the daemon holds. Only ever the launcher: it is the one
    /// process the daemon owns here, and a stale one must not keep the band away.
    fn kill_console_child(&mut self) {
        if let Some(mut child) = self.console_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.console_spawned = None;
    }

    fn reap_console_child(&mut self) {
        if let Some(child) = &mut self.console_child {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    info!("console launcher exited ({exit})");
                    // The launcher is gone: the handle goes with it, so the next request
                    // spawns a new one instead of refusing.
                    self.console_child = None;
                    self.console_spawned = None;
                }
                Ok(None) => {}
                Err(e) => warn!("console launcher wait failed: {e}"),
            }
        }
    }

    /// Cover-art pass for the launcher's apps. Cheap when the cache is warm: the worker stats
    /// files and only hits the network for keys with nothing cached.
    fn ensure_art(&mut self) {
        if self.art_busy {
            return;
        }
        let wants: Vec<art::Want> = launcher_entries(&self.cfg)
            .into_iter()
            .map(|(name, exe)| art::Want {
                key: art::key(&exe),
                name,
                exe,
            })
            .collect();
        if wants.is_empty() {
            return;
        }
        self.art_busy = true;
        let tx = self.art_tx.clone();
        let api_key = self.cfg.sgdb_api_key.clone();
        thread::spawn(move || {
            art::ensure_all(wants, api_key, |key| {
                let _ = tx.send(key.to_string());
            });
            let _ = tx.send(String::new()); // the pass finished
        });
    }

    fn on_art(&mut self, key: String) {
        if key.is_empty() {
            self.art_busy = false;
            if self.art_retry_after_pass {
                // The pass that just ended ran with the old key and left its miss markers
                // behind: clear them and go again with the new one.
                self.art_retry_after_pass = false;
                art::clear_misses();
                self.ensure_art();
            }
            if self.art_pass_again {
                // A slot went back to automatic while the pass was running: it has to be
                // fetched again.
                self.art_pass_again = false;
                self.ensure_art();
            }
            return;
        }
        info!("cover art: {key} is ready");
        self.broadcast_status(); // the console re-reads the art folder on status updates
    }

    /// The picker's worker answered: the client that asked gets the answer.
    fn on_art_answer(&mut self, answer: ArtAnswer) {
        match answer {
            ArtAnswer::Listing { client, listing } => {
                if listing.ok {
                    info!(
                        "artwork: {} photo(s) of the {} slot ({} in total)",
                        listing.items.len(),
                        slot_name(listing.slot),
                        listing.total
                    );
                } else {
                    warn!("artwork: {}", listing.error);
                }
                if let Some(push) = self.clients.get(&client) {
                    let _ = push.send(armoury_core::protocol::art_listing(&listing));
                }
            }
            ArtAnswer::Saved { client, ok, message } => {
                if ok {
                    info!("artwork: {message}");
                } else {
                    warn!("artwork: {message}");
                    remember_error(&mut self.last_error, "artwork", &message);
                }
                if let Some(push) = self.clients.get(&client) {
                    let _ = push.send(armoury_core::protocol::art_set(ok, &message));
                }
                self.broadcast_status();
            }
        }
    }

    /// Remember when an app was last played, for the console's recency order. `exe` may be a
    /// full path or a bare exe base name; keys are exe base names, lowercased.
    fn record_played(&mut self, exe: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let key = basename_lower(exe);
        if self
            .cfg
            .last_played
            .get(&key)
            .map(|t| now.saturating_sub(*t) < 5)
            .unwrap_or(false)
        {
            return; // the same launch seen twice (console request + watcher): keep the file quiet
        }
        self.cfg.last_played.insert(key, now);
        if let Err(e) = armoury_core::config::save(&self.cfg) {
            warn!("failed to save config: {e}");
        }
        self.broadcast_status();
    }

    /// Pin an app to the console launcher. False when it is already in the list (by file name).
    /// A tracked app that was hidden from the band counts as a pin: adding it back un-hides it.
    fn add_console_app(&mut self, name: String, exe: String) -> bool {
        // Matching in the console is by file name, so dedupe on that, like tracked apps.
        let base = basename_lower(&exe);
        if self
            .cfg
            .tracked_apps
            .iter()
            .any(|a| basename_lower(&a.exe) == base)
        {
            let before = self.cfg.console_hidden.len();
            self.cfg.console_hidden.retain(|h| h != &base);
            if self.cfg.console_hidden.len() == before {
                info!("console app already in the list (by name): {base}");
                return false;
            }
            if let Err(e) = armoury_core::config::save(&self.cfg) {
                warn!("failed to save config: {e}");
            }
            info!("console app shown again: {exe}");
            return true;
        }
        if self
            .cfg
            .console_apps
            .iter()
            .any(|a| basename_lower(&a.exe) == base)
        {
            info!("console app already in the list (by name): {base}");
            return false;
        }
        self.cfg.console_apps.push(ConsoleApp {
            name,
            exe: exe.clone(),
        });
        if let Err(e) = armoury_core::config::save(&self.cfg) {
            warn!("failed to save config: {e}");
        }
        info!("console app added: {exe}");
        true
    }

    /// Tray click / menu "Open dashboard": focus the UI if it is connected, else spawn it.
    fn on_open(&mut self) {
        if let Some(ui_id) = self.ui_conn {
            if let Some(push) = self.clients.get(&ui_id) {
                if push.send(Response::Focus).is_ok() {
                    info!("UI already running — focus requested");
                    return;
                }
            }
            self.ui_conn = None;
        }
        if self.spawning_ui {
            info!("UI is already starting");
            return;
        }

        // The UI is this same executable, started with `--ui`.
        // CREATE_NO_WINDOW: never flash a console, even in debug builds.
        let exe = std::env::current_exe();
        let Some(exe) = exe.ok() else {
            warn!("cannot resolve own executable path");
            return;
        };
        match Command::new(&exe)
            .arg("--ui")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn() {
            Ok(child) => {
                info!("spawned UI (pid {}) from {}", child.id(), exe.display());
                self.ui_child = Some(child);
                self.spawning_ui = true;
            }
            Err(e) => warn!("failed to spawn UI: {e}"),
        }
    }

    fn on_pipe(&mut self, ev: ipc::PipeEvent) {
        match ev {
            ipc::PipeEvent::Connected { id, push } => {
                info!("pipe client {id} connected");
                self.clients.insert(id, push);
            }
            ipc::PipeEvent::Disconnected { id } => {
                self.clients.remove(&id);
                if self.ui_conn == Some(id) {
                    self.ui_conn = None;
                    info!("UI disconnected");
                }
            }
            ipc::PipeEvent::Request { id, req } => match req {
                Request::UiHello { pid } => {
                    info!("UI hello (ui pid {pid}) on connection {id}");
                    self.ui_conn = Some(id);
                    self.spawning_ui = false;
                    let status = self.status();
                    if let Some(push) = self.clients.get(&id) {
                        let _ = push.send(Response::HelloAck {
                            daemon_version: armoury_core::VERSION.to_string(),
                            status,
                        });
                    }
                }
                Request::GetStatus => {
                    let status = self.status();
                    if let Some(push) = self.clients.get(&id) {
                        let _ = push.send(Response::Status { status });
                    }
                }
                Request::ShowUi => self.on_open(),
                Request::Quit => {
                    // The same shutdown the tray menu's "Quit" triggers: the flag is read by
                    // the main loop, which then reverts the packs and restores the audio.
                    info!("quit requested over the pipe");
                    self.quit_requested.ask();
                }
                Request::ConsoleHello { pid } => {
                    info!("console hello (pid {pid}) on connection {id}");
                    let status = self.status();
                    if let Some(push) = self.clients.get(&id) {
                        let _ = push.send(Response::HelloAck {
                            daemon_version: armoury_core::VERSION.to_string(),
                            status,
                        });
                    }
                    self.ensure_art(); // the band is up: make sure its covers exist
                }
                // An explicit request over the pipe (the settings preview asks for the
                // band): the user asked for it, so it opens even while a game is running.
                Request::ShowConsole => self.open_console(ConsoleOpen::Explicit),
                Request::SetConsoleOpen { mode } => {
                    let mode = normalize_console_open(&mode).to_string();
                    let changed = self.cfg.console_open != mode;
                    self.cfg.console_open = mode.clone();
                    if changed {
                        if let Err(e) = armoury_core::config::save(&self.cfg) {
                            warn!("failed to save config: {e}");
                        }
                        info!("console: open mode '{mode}'");
                    }
                    // Selecting a mode is a trigger in its own right: a pad that is already
                    // connected never sends a connect event, and "always" should show the band
                    // now rather than at the next daemon start. It is an automatic open all
                    // the same: a tracked app that is running keeps the band away.
                    let pad_present = pad::first_connected().is_some();
                    if mode == CONSOLE_OPEN_ALWAYS
                        || (mode != CONSOLE_OPEN_NEVER && pad_present)
                    {
                        self.open_console_if_allowed();
                    }
                    self.broadcast_status();
                }
                Request::SetSgdbKey { key } => {
                    let key = key.trim();
                    let new = if key.is_empty() {
                        None
                    } else {
                        Some(key.to_string())
                    };
                    let changed = self.cfg.sgdb_api_key != new;
                    self.cfg.sgdb_api_key = new;
                    if changed {
                        if let Err(e) = armoury_core::config::save(&self.cfg) {
                            warn!("failed to save config: {e}");
                        }
                        // Never the value itself.
                        info!("steamgriddb api key changed");
                        // A keyless pass left a miss marker per app, and the fetcher skips a
                        // fresh marker for seven days: without this the new key would never be
                        // used and no cover would ever appear.
                        art::clear_misses();
                        // A pass that is still running uses the old key and writes its markers
                        // again: it has to be followed by another pass with the new key.
                        self.art_retry_after_pass = self.art_busy;
                        self.ensure_art();
                    }
                    self.broadcast_status();
                }
                Request::AddConsoleApp { name, exe } => {
                    let ok = self.add_console_app(name, exe);
                    if ok {
                        self.ensure_art();
                    }
                    if let Some(push) = self.clients.get(&id) {
                        let _ = push.send(Response::ConsoleOp {
                            ok,
                            message: if ok { "added" } else { "already in the list" }.into(),
                        });
                    }
                    self.broadcast_status();
                }
                Request::RemoveConsoleApp { exe } => {
                    let before = self.cfg.console_apps.len();
                    self.cfg
                        .console_apps
                        .retain(|a| !a.exe.eq_ignore_ascii_case(&exe));
                    let ok = self.cfg.console_apps.len() != before;
                    if ok {
                        if let Err(e) = armoury_core::config::save(&self.cfg) {
                            warn!("failed to save config: {e}");
                        }
                        info!("console app removed: {exe}");
                    }
                    if let Some(push) = self.clients.get(&id) {
                        let _ = push.send(Response::ConsoleOp {
                            ok,
                            message: if ok { "removed" } else { "not in the list" }.into(),
                        });
                    }
                    self.broadcast_status();
                }
                Request::HideFromConsole { exe } => {
                    // The band hides a tracked app without untracking it; a pin for the same
                    // exe would show it right back, so that goes too.
                    let base = basename_lower(&exe);
                    if !self.cfg.console_hidden.contains(&base) {
                        self.cfg.console_hidden.push(base.clone());
                    }
                    self.cfg
                        .console_apps
                        .retain(|a| basename_lower(&a.exe) != base);
                    if let Err(e) = armoury_core::config::save(&self.cfg) {
                        warn!("failed to save config: {e}");
                    }
                    info!("console app hidden: {exe}");
                    if let Some(push) = self.clients.get(&id) {
                        let _ = push.send(Response::ConsoleOp {
                            ok: true,
                            message: "hidden".into(),
                        });
                    }
                    self.broadcast_status();
                }
                Request::LaunchApp { exe } => {
                    // The daemon spawns and checks the result: a Steam game is started
                    // through the Steam client, and a process that dies right away is a
                    // failed launch, not a started game. The client is told either way.
                    self.launch_app(id, exe);
                }
                Request::ListArt { exe, slot, page } => {
                    // The console cannot call the API itself: the daemon holds the key and
                    // does the work. The network call happens on a worker thread, so the
                    // main loop keeps running.
                    let want = art_want(&self.cfg, &exe);
                    let api = self.cfg.sgdb_api_key.clone();
                    let tx = self.art_query_tx.clone();
                    let wake = self.wake;
                    thread::spawn(move || {
                        let listing = match art::list_photos(api.as_deref(), &want, slot, page) {
                            Ok(listing) => listing,
                            Err(e) => ArtListing {
                                ok: false,
                                slot,
                                page,
                                total: 0,
                                items: Vec::new(),
                                error: e,
                            },
                        };
                        let _ = tx.send(ArtAnswer::Listing { client: id, listing });
                        signal_wake(wake);
                    });
                }
                Request::SetArt { exe, slot, url } => {
                    let key = art::key(&exe);
                    if url.trim().is_empty() {
                        // Automatic again: the user's own art goes, and the next art pass
                        // fetches the slot the way it always did.
                        let ok = art::use_automatic(&key, slot).is_ok();
                        if ok {
                            info!(
                                "artwork: the {} slot of {exe} is automatic again",
                                slot_name(slot)
                            );
                            // The automatic picker has to run again for this slot. A pass that
                            // is running right now has already passed this app, so another
                            // one is needed when it ends.
                            self.art_pass_again = self.art_busy;
                            self.ensure_art();
                        } else {
                            warn!(
                                "artwork: cannot clear the {} slot of {exe}",
                                slot_name(slot)
                            );
                        }
                        let message = if ok {
                            "automatic again".to_string()
                        } else {
                            "cannot clear the artwork".to_string()
                        };
                        if let Some(push) = self.clients.get(&id) {
                            let _ = push.send(armoury_core::protocol::art_set(ok, &message));
                        }
                        self.broadcast_status();
                    } else {
                        // A chosen photo: download it and store it as the user's own art.
                        let tx = self.art_query_tx.clone();
                        let wake = self.wake;
                        thread::spawn(move || {
                            let (ok, message) = match art::set_photo(&key, slot, &url) {
                                Ok(()) => (true, "saved".to_string()),
                                Err(e) => (false, e),
                            };
                            let _ = tx.send(ArtAnswer::Saved { client: id, ok, message });
                            signal_wake(wake);
                        });
                    }
                }
                Request::AddTrackedApp { name, exe } => {
                    self.add_tracked_app(name, exe);
                    self.ensure_art();
                }
                Request::RemoveTrackedApp { exe } => self.remove_tracked_app(&exe),
                Request::SetPaused { paused } => self.set_paused(paused),
                Request::SetPackEnabled { id, enabled } => self.set_pack_enabled(id, enabled),
                Request::SetPackValue { id, key, value } => self.set_pack_value(id, key, value),
                Request::SetIdle { active } => {
                    info!("idle request from the UI: active={active}");
                    if active {
                        self.enter_idle();
                    } else {
                        self.exit_idle();
                    }
                }
                Request::SetCooldown { seconds } => {
                    self.cfg.cooldown_seconds = seconds.clamp(5, 3600);
                    self.engine
                        .set_cooldown(Duration::from_secs(self.cfg.cooldown_seconds), Instant::now());
                    if let Err(e) = armoury_core::config::save(&self.cfg) {
                        warn!("failed to save config: {e}");
                    }
                    info!("cooldown set to {}s", self.cfg.cooldown_seconds);
                    self.broadcast_status();
                }
                Request::SetIdleFailsafe { enabled, minutes } => {
                    self.cfg.idle_failsafe_enabled = enabled;
                    self.cfg.idle_failsafe_minutes = minutes.max(1);
                    if let Err(e) = armoury_core::config::save(&self.cfg) {
                        warn!("failed to save config: {e}");
                    }
                    info!(
                        "idle failsafe {} ({} min)",
                        if enabled { "on" } else { "off" },
                        self.cfg.idle_failsafe_minutes
                    );
                    self.broadcast_status();
                }
                Request::SetKillswitch { enabled } => self.set_killswitch_config(enabled),
                Request::InstallPack { source } => {
                    let result = install_pack_file(&self.search, std::path::Path::new(&source));
                    match result {
                        Ok((pack_id, updated)) => {
                            self.reload_packs();
                            let name = self
                                .pack_catalog
                                .iter()
                                .find(|m| m.id == pack_id)
                                .map(|m| m.name.clone())
                                .unwrap_or_else(|| pack_id.clone());
                            let message = format!(
                                "{name} {}",
                                if updated { "updated" } else { "installed" }
                            );
                            info!("pack op: {message}");
                            forget_error(&mut self.last_error, "catalog");
                            self.reply_pack_op(id, true, message);
                        }
                        Err(e) => {
                            warn!("pack install failed: {e}");
                            remember_error(&mut self.last_error, "catalog", &e);
                            self.reply_pack_op(id, false, e);
                        }
                    }
                }
                Request::InstallHelper {
                    id: helper_id,
                    name,
                    version,
                    entry,
                    file,
                    sha256,
                    size,
                } => {
                    let result = install_helper_file(
                        &self.search,
                        &helper_id,
                        &name,
                        &version,
                        &entry,
                        std::path::Path::new(&file),
                        &sha256,
                        size,
                    );
                    match result {
                        Ok((_, updated)) => {
                            let message = format!(
                                "{name} {}",
                                if updated { "updated" } else { "installed" }
                            );
                            info!("pack op: {message}");
                            forget_error(&mut self.last_error, "catalog");
                            self.reply_pack_op(id, true, message);
                        }
                        Err(e) => {
                            warn!("helper install failed: {e}");
                            remember_error(&mut self.last_error, "catalog", &e);
                            self.reply_pack_op(id, false, e);
                        }
                    }
                }
                Request::RemovePack { id: pack_id } => {
                    if let Some((pending, _)) = &self.pending_removal {
                        let message = format!(
                            "a removal of {pending} is still running — try again in a moment"
                        );
                        warn!("pack remove refused: {message}");
                        self.reply_pack_op(id, false, message);
                        return;
                    }
                    // If the pack is applied right now, put the app back *first*: the worker still
                    // has the pack, so queue the revert with its exact captures and remove the
                    // folder when that outcome comes back (`pending_removal`).
                    let applied = self.applied_packs.iter().any(|p| p == &pack_id);
                    let job = if applied {
                        self.captured_by_pack.get(&pack_id).cloned().map(|captured| {
                            packs::PackJob {
                                apply: false,
                                tag: Some(pack_id.clone()),
                                enabled: HashMap::from([(pack_id.clone(), true)]),
                                values: self
                                    .cfg
                                    .packs
                                    .get(&pack_id)
                                    .map(|v| HashMap::from([(pack_id.clone(), v.values.clone())]))
                                    .unwrap_or_default(),
                                captured: HashMap::from([(pack_id.clone(), captured)]),
                            }
                        })
                    } else {
                        None
                    };

                    match job {
                        Some(job) => {
                            info!("pack {pack_id} is applied — reverting it before removal");
                            self.pending_removal = Some((pack_id.clone(), id));
                            let sender = self.worker_for(&pack_id);
                            let _ = sender.send(job);
                        }
                        None => {
                            if applied {
                                warn!("pack {pack_id} is applied but has no captures — removing anyway");
                                self.applied_packs.retain(|p| p != &pack_id);
                            }
                            self.finish_removal(&pack_id, id, None);
                        }
                    }
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 plumbing

/// The pipe's security descriptor. Without one the pipe inherits the DACL of the elevated
/// token: a non-elevated process of the same user is locked out, an elevated process of
/// *any* user is not — and once connected, nothing is authenticated at all.
/// `None` = the descriptor could not be built; the pipe then keeps the default rights.
fn pipe_security_descriptor() -> Option<SecurityDescriptor> {
    let sddl = pipe_sddl();
    let wide = match widestring::U16CString::from_str(&sddl) {
        Ok(w) => w,
        Err(e) => {
            warn!("pipe security descriptor is malformed ({e}) — the pipe keeps the default rights");
            return None;
        }
    };
    match SecurityDescriptor::deserialize(wide.as_ucstr()) {
        Ok(sd) => Some(sd),
        Err(e) => {
            warn!(
                "pipe security descriptor refused by Windows ({e}) — the pipe keeps the default \
                 rights"
            );
            None
        }
    }
}

/// The SDDL of the pipe: the interactive user, Administrators and SYSTEM — nobody else.
fn pipe_sddl() -> String {
    const BASE: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
    match current_user_sid() {
        Some(sid) => {
            let who = current_user_name().unwrap_or_else(|| sid.clone());
            info!("pipe security: SYSTEM, Administrators and {who}");
            format!("{BASE}(A;;GA;;;{sid})")
        }
        None => {
            warn!(
                "the current user could not be determined — the pipe is open to SYSTEM and \
                 Administrators only"
            );
            BASE.to_string()
        }
    }
}

/// The SID of the user this daemon runs as (the interactive user), as an SDDL trustee
/// (`S-1-5-21-...`). The SDDL of a security descriptor only knows SIDs, not account names.
pub(crate) fn current_user_sid() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle of the current process is always valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return None;
    }
    let mut len: u32 = 0;
    // The first call only asks for the size of the TOKEN_USER (it fails on purpose).
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
    }
    if len == 0 {
        unsafe {
            let _ = CloseHandle(token);
        }
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: the buffer is exactly as big as the call above asked for.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .is_ok();
    unsafe {
        let _ = CloseHandle(token);
    }
    if !ok {
        return None;
    }
    // SAFETY: the buffer holds a TOKEN_USER written by GetTokenInformation.
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut sid: PWSTR = PWSTR::null();
    // SAFETY: the SID belongs to the token and lives as long as it.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) }.is_err() {
        return None;
    }
    // SAFETY: the string was just written by Windows and is null terminated.
    let text = unsafe { sid.to_string() }.ok();
    // SAFETY: the string was allocated by Windows with LocalAlloc.
    unsafe { LocalFree(Some(HLOCAL(sid.as_ptr().cast()))) };
    text
}

/// The name of the user this daemon runs as: the interactive user, whose process this is.
pub(crate) fn current_user_name() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::System::WindowsProgramming::GetUserNameW;

    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer and its length are handed over as the API expects.
    if unsafe { GetUserNameW(Some(PWSTR(buf.as_mut_ptr())), &mut len) }.is_ok() {
        // The length counts the terminating null.
        let name = String::from_utf16_lossy(&buf[..len as usize]);
        let name = name.trim_end_matches('\0').trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    // A service-style launch has no user name in the API but one in the environment.
    std::env::var("USERNAME")
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
}

/// Drain and dispatch pending window messages (tray icon, keyboard hook, taskbar restarts).
/// Bounded: a window that keeps re-posting messages (a missing ValidateRect, a menu loop) must
/// never stop the main loop from draining pipe requests — that is exactly how the "Wake up
/// button does nothing" bug happened on 2026-10-17.
fn pump_win32_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    for _ in 0..512 {
        let got = unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
        if got == 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

enum Singleton {
    Acquired(Option<windows::Win32::Foundation::HANDLE>),
    AlreadyRunning,
}

fn acquire_singleton() -> Singleton {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    // `ARMOURY_TEST_NS` also namespaces the mutex, so the integration harness can run a real
    // daemon next to the installed one.
    let suffix = match std::env::var("ARMOURY_TEST_NS") {
        Ok(ns) if !ns.trim().is_empty() => format!("-{}", ns.trim()),
        _ => String::new(),
    };
    let name: Vec<u16> = format!("Local\\ArmouryDaemon.Singleton{suffix}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Singleton::AlreadyRunning;
    }
    match handle {
        Ok(h) => Singleton::Acquired(Some(h)),
        Err(e) => {
            warn!("single-instance mutex failed ({e}); continuing without guard");
            Singleton::Acquired(None)
        }
    }
}

/// Second instance: ask the running daemon to show its UI, then exit.
fn nudge_existing_instance() {
    let pipe = armoury_core::pipe_name();
    for attempt in 0..5 {
        let Ok(name) = pipe.as_str().to_ns_name::<GenericNamespaced>() else {
            return;
        };
        match ConnectOptions::new().name(name).connect_sync() {
            Ok(mut stream) => {
                let _ = armoury_core::protocol::write_msg(&mut stream, &Request::ShowUi);
                info!("asked running instance to show the UI");
                return;
            }
            Err(e) => {
                if attempt == 4 {
                    warn!("could not reach running instance: {e}");
                } else {
                    thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::io::Write as _;

    /// The same shape of helper archive the helpers tests build: one executable at the
    /// archive root, zipped in-process, deflated. The bytes, their SHA-256 and their
    /// size are one truth for every expectation of the request path.
    fn install_zip() -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("Run.exe", options)
            .expect("a plain name always starts");
        zip.write_all(b"helper placeholder bytes")
            .expect("entry bytes");
        zip.finish().expect("the archive always finishes");
        buffer.into_inner()
    }

    #[test]
    fn a_helper_installs_through_the_daemon_request_path() {
        // The Modules window downloads the archive and sends the daemon the request with
        // the manifest entry and its SHA-256. The path here mirrors that request: the
        // bytes are verified, the helper lands under modules/, its record is beside the
        // executable, and the temp archive does not survive the answer.
        let scratch = Scratch::dir("daemon-helper");
        let dirs = vec![scratch.root().to_path_buf()];
        let archive = install_zip();
        let sha = catalog::hex_sha256(&archive);
        let temp = std::env::temp_dir().join(format!("{}daemon-b5a.zip", catalog::TEMP_PREFIX));
        std::fs::write(&temp, &archive).expect("the temp archive writes");

        let (id, updated) = install_helper_file(
            &dirs,
            "daemon-b5a",
            "Requested helper",
            "1.0.0",
            "Run.exe",
            &temp,
            &sha,
            Some(archive.len() as u64),
        )
        .expect("the verified archive installs");
        assert_eq!(id, "daemon-b5a");
        assert!(!updated, "the first install is not an update");
        assert!(
            !temp.exists(),
            "the archive is removed once the answer is in"
        );
        let record = scratch.root().join("modules").join("daemon-b5a");
        assert!(
            record.join("module.json").is_file() && record.join("Run.exe").is_file(),
            "the folder holds the executable and its record: {}",
            record.display()
        );
    }

    #[test]
    fn a_helper_request_is_refused_outside_the_temp_folder_and_refused_bad_bytes() {
        // The daemon reads one file and installs what it names; the rules around that
        // read are what keep the request from moving bytes anywhere else: the file has
        // to sit in temp with the download's name shape, and its bytes have to hash to
        // the SHA-256 the request came with.
        let scratch = Scratch::dir("daemon-helper-refused");
        let dirs = vec![scratch.root().to_path_buf()];
        // right name shape, wrong folder
        let foreign = scratch
            .root()
            .join(format!("{}wrong-place.zip", catalog::TEMP_PREFIX));
        std::fs::write(&foreign, b"junk").expect("the foreign file writes");
        let refused = install_helper_file(
            &dirs,
            "any",
            "Any helper",
            "1.0.0",
            "Run.exe",
            &foreign,
            "00",
            None,
        );
        let why = refused.expect_err("a file outside temp is not ours to extract");
        assert!(
            why.contains("not the temp archive"),
            "the refusal names the rule: {why}"
        );
        // right name shape, right folder, bytes that do not hash to the given SHA-256
        let temp = std::env::temp_dir().join(format!("{}daemon-b5b.zip", catalog::TEMP_PREFIX));
        std::fs::write(&temp, b"junk that does not hash to 00").expect("the temp file writes");
        let refused = install_helper_file(
            &dirs,
            "any",
            "Any helper",
            "1.0.0",
            "Run.exe",
            &temp,
            &"0".repeat(64),
            None,
        );
        let why = refused.expect_err("the hash check runs before anything lands");
        assert!(why.contains("sha256"), "the refusal names the hash: {why}");
        assert!(
            !temp.exists(),
            "the archive is removed even when the request is refused"
        );
    }

    /// The console helper the daemon spawns (C2): the tests install real helper modules
    /// into scratch `modules/` folders with the same machinery the request path uses, and
    /// read the selection back through the same scan.
    fn console_zip(entry: &str) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(entry, options)
            .expect("a plain name always starts");
        zip.write_all(b"MZ helper placeholder - nothing here runs in the tests")
            .expect("entry bytes");
        zip.finish().expect("the archive always finishes");
        buffer.into_inner()
    }

    fn install_console_helper(search: &[std::path::PathBuf], id: &str, entry: &str) {
        let pack = catalog::CatalogPack {
            id: id.to_string(),
            name: "Console helper".to_string(),
            version: "1.0.0".to_string(),
            description: String::new(),
            kind: "helper".to_string(),
            file: String::new(),
            sha256: "not-checked-here".to_string(),
            size: None,
            min_engine: None,
            entry: Some(entry.to_string()),
        };
        helpers::install_from_bytes(search, &pack, &console_zip(entry))
            .expect("the helper installs");
    }

    #[test]
    fn the_installed_console_helper_is_preferred() {
        let scratch = Scratch::dir("console-helper");
        let dirs = vec![scratch.root().to_path_buf()];
        install_console_helper(&dirs, "console-test", "ArmouryConsole.exe");
        let found = console_helper(&dirs).expect("the installed helper is the console");
        assert_eq!(found.id, "console-test");
        assert_eq!(
            found.path,
            scratch
                .root()
                .join("modules")
                .join("console-test")
                .join("ArmouryConsole.exe"),
            "the helper's own recorded entry is what would be spawned"
        );
    }

    #[test]
    fn the_console_entry_matches_by_file_name_only() {
        // The record's entry may name a subfolder; the file name alone has to decide.
        for entry in [
            "ArmouryConsole.exe",
            "console/ArmouryConsole.exe",
            "armouryconsole.EXE",
        ] {
            assert!(
                matches_console_entry(entry, "ArmouryConsole.exe"),
                "the entry {entry} is the console"
            );
        }
        for entry in ["Run.exe", "ArmouryConsole.exe.old", ""] {
            assert!(
                !matches_console_entry(entry, "ArmouryConsole.exe"),
                "the entry {entry:?} is not the console"
            );
        }
    }

    #[test]
    fn a_helper_with_another_entry_is_not_the_console() {
        let scratch = Scratch::dir("console-helper-other");
        let dirs = vec![scratch.root().to_path_buf()];
        install_console_helper(&dirs, "run-check", "Run.exe");
        assert!(
            console_helper(&dirs).is_none(),
            "a helper that does not carry the console exe is not the console"
        );
    }

    #[test]
    fn a_console_helper_whose_exe_is_gone_is_skipped() {
        let scratch = Scratch::dir("console-helper-gone");
        let dirs = vec![scratch.root().to_path_buf()];
        install_console_helper(&dirs, "console-gone", "ArmouryConsole.exe");
        let exe = scratch
            .root()
            .join("modules")
            .join("console-gone")
            .join("ArmouryConsole.exe");
        std::fs::remove_file(&exe).expect("the entry exe is removed");
        assert!(
            console_helper(&dirs).is_none(),
            "a recorded entry without a file on disk is not a launcher"
        );
    }

    #[test]
    fn two_console_helpers_pick_the_lower_id() {
        let scratch = Scratch::dir("console-helper-order");
        let dirs = vec![scratch.root().to_path_buf()];
        install_console_helper(&dirs, "b-check", "ArmouryConsole.exe");
        install_console_helper(&dirs, "a-check", "ArmouryConsole.exe");
        let found = console_helper(&dirs).expect("one of the two is chosen");
        assert_eq!(found.id, "a-check", "the choice must be deterministic: the lower id");
    }

    #[test]
    fn no_modules_folder_leaves_no_console_at_all() {
        // The console is a pack and nothing else: with no helper installed there is no exe to
        // launch, and the daemon must not fall back to a mode of its own (2026-10-27).
        let scratch = Scratch::dir("console-helper-none");
        let dirs = vec![scratch.root().to_path_buf()];
        assert!(console_helper(&dirs).is_none());
    }

    #[test]
    fn launcher_entries_drop_hidden_tracked_apps_and_dedupe_pins() {
        let mut cfg = AppConfig::default();
        cfg.tracked_apps.push(TrackedApp {
            name: "Demo".into(),
            exe: "C:\\games\\demo.exe".into(),
        });
        cfg.tracked_apps.push(TrackedApp {
            name: "Hidden".into(),
            exe: "C:\\games\\Hidden.exe".into(),
        });
        cfg.console_hidden.push("hidden.exe".into());
        cfg.console_apps.push(ConsoleApp {
            name: "Pin".into(),
            exe: "D:\\tools\\pin.exe".into(),
        });
        cfg.console_apps.push(ConsoleApp {
            name: "Tracked again".into(),
            exe: "D:\\copy\\DEMO.exe".into(),
        });
        let entries = launcher_entries(&cfg);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, "C:\\games\\demo.exe"); // the tracked copy wins
        assert_eq!(entries[1].1, "D:\\tools\\pin.exe");
    }

    #[test]
    fn the_two_halves_of_a_job_merge_order_independently() {
        // A4: a whole-state job leaves as two halves. Whichever half arrives first, the
        // merge lands on the same total: both applied after the applies, nothing left and
        // an emptied journal after the reverts.
        let half = |applied: bool, scoped: &[&str], ids: &[&str], captured: &[&str]| {
            let mut journal = HashMap::new();
            for id in captured {
                journal.insert(
                    id.to_string(),
                    HashMap::from([("mode".to_string(), "2".to_string())]),
                );
            }
            packs::PackOutcome {
                applied,
                tag: None,
                scoped: scoped.iter().map(|s| s.to_string()).collect(),
                applied_ids: ids.iter().map(|s| s.to_string()).collect(),
                values: HashMap::new(),
                captured: journal,
                succeeded: vec![],
                failed: vec![],
            }
        };
        let pack_half = half(true, &["g-pack"], &["g-pack"], &["g-pack"]);
        let mode_half = half(true, &["m-mode"], &["m-mode"], &["m-mode"]);
        let pack_back = half(false, &["g-pack"], &[], &[]);
        let mode_back = half(false, &["m-mode"], &[], &[]);

        for order in [[&pack_half, &mode_half], [&mode_half, &pack_half]] {
            let mut applied: Vec<String> = Vec::new();
            let mut captured: HashMap<String, HashMap<String, String>> = HashMap::new();
            for o in order {
                merge_scoped(&mut applied, &mut captured, o);
            }
            assert!(applied.contains(&"g-pack".to_string()), "{applied:?}");
            assert!(applied.contains(&"m-mode".to_string()), "{applied:?}");
            assert_eq!(captured.len(), 2, "both halves journalled their captures");
            merge_scoped(&mut applied, &mut captured, &pack_back);
            merge_scoped(&mut applied, &mut captured, &mode_back);
            assert!(applied.is_empty(), "the reverts took everything back: {applied:?}");
            assert!(captured.is_empty(), "and emptied the journal: {captured:?}");
        }
    }

    #[test]
    fn a_mode_half_never_touches_the_packs_half_of_the_state() {
        // The scope is per kind: a mode outcome must leave a pack's applied id alone, and
        // a pack outcome must leave a mode's alone. That is what makes the merge safe two
        // workers feeding one channel.
        let mut applied = vec!["g-pack".to_string(), "m-mode".to_string()];
        let mut captured: HashMap<String, HashMap<String, String>> = HashMap::from([(
            "g-pack".to_string(),
            HashMap::from([("was_running".to_string(), "1".to_string())]),
        )]);
        merge_scoped(
            &mut applied,
            &mut captured,
            &packs::PackOutcome {
                applied: false,
                tag: None,
                scoped: vec!["m-mode".to_string()],
                applied_ids: vec![],
                values: HashMap::new(),
                captured: HashMap::new(),
                succeeded: vec![],
                failed: vec![],
            },
        );
        assert_eq!(applied, vec!["g-pack"]);
        assert!(captured.contains_key("g-pack"));
    }

    #[test]
    fn split_enabled_routes_by_the_catalogue() {
        // The same job on the two kind sides; an unknown id stays on the pack side (the
        // old behaviour — the worker simply ignores ids it does not carry).
        let catalog = vec![
            PackMeta {
                id: "g-pack".into(),
                name: String::new(),
                version: String::new(),
                kind: PackKind::Pack,
                settings: vec![],
            },
            PackMeta {
                id: "m-mode".into(),
                name: String::new(),
                version: String::new(),
                kind: PackKind::Mode,
                settings: vec![],
            },
        ];
        let enabled: HashMap<String, bool> = HashMap::from([
            ("g-pack".to_string(), true),
            ("m-mode".to_string(), true),
            ("ghost".to_string(), true),
        ]);
        let (pack, mode) = split_enabled(enabled, &catalog);
        assert!(pack.contains_key("g-pack") && pack.contains_key("ghost"));
        assert_eq!(mode.keys().cloned().collect::<Vec<_>>(), vec!["m-mode"]);
        assert!(is_mode(&catalog, "m-mode"));
        assert!(!is_mode(&catalog, "g-pack"));
        assert!(!is_mode(&catalog, "m-mode-not-there"));
    }

    #[test]
    fn a_steam_game_is_started_through_steam_and_a_standalone_game_directly() {
        let root = Scratch::dir("launch-plan");
        let game = root.join("Steam/steamapps/common/Some Game/sub");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(
            root.join("Steam/steamapps/appmanifest_12345.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"12345\"\n\t\"installdir\"\t\t\"Some Game\"\n}\n",
        )
        .unwrap();

        // the folder right after `common` is the installdir the manifest names
        let exe = game.join("game.exe");
        match launch_plan(&exe, art::steam_app(&exe)) {
            LaunchPlan::Steam {
                appid,
                steam_exe,
                steam_dir,
                install_dir,
            } => {
                assert_eq!(appid, 12345);
                assert_eq!(steam_exe, root.join("Steam/steam.exe"));
                assert_eq!(steam_dir, root.join("Steam"));
                assert_eq!(install_dir, root.join("Steam/steamapps/common/Some Game"));
            }
            LaunchPlan::Direct { .. } => panic!("a Steam game must be started through Steam"),
        }

        // a game that is not in a Steam library is started directly
        let plain = std::path::Path::new("C:\\Games\\AlanWake2.exe");
        match launch_plan(plain, art::steam_app(plain)) {
            LaunchPlan::Direct { exe } => assert_eq!(exe, plain),
            LaunchPlan::Steam { .. } => panic!("a standalone game must be started directly"),
        }
    }

    #[test]
    fn a_failed_launch_is_not_played_but_a_started_one_is() {
        let started = LaunchOutcome {
            ok: true,
            message: "launched game.exe (pid 42)".into(),
        };
        assert!(matches!(launch_action(&started), LaunchAction::Played));

        let failed = LaunchOutcome {
            ok: false,
            message: "game.exe exited right away (exit code 0); the game did not start".into(),
        };
        match launch_action(&failed) {
            LaunchAction::Failed(message) => {
                assert!(message.contains("did not start"), "{message}");
            }
            LaunchAction::Played => panic!("a failed launch must not count as played"),
        }
    }

    #[test]
    fn the_quit_request_asks_for_the_shutdown() {
        let mut quit = QuitRequest::default();
        assert!(!quit.asked());
        quit.ask();
        assert!(quit.asked());
    }

    #[test]
    fn a_live_console_launcher_with_a_window_is_kept() {
        assert_eq!(console_action(4242, false, false), ConsoleAction::Keep);
    }

    #[test]
    fn the_first_run_opens_the_dashboard_even_with_packs_installed() {
        // The packs are not what makes a first run a first run: a user who unpacked the
        // zip has three of them and still lands on the dashboard.
        assert!(
            first_run_opens_dashboard(Start::Manual, true),
            "the first run opens the dashboard"
        );
        // A scheduled start (logon, resume) must not disturb the user.
        assert!(
            !first_run_opens_dashboard(Start::Autostart, true),
            "a scheduled start opens nothing"
        );
    }

    #[test]
    fn the_marker_ends_the_first_run() {
        // The marker is what stops the dashboard from coming back: it is written as the
        // dashboard opens, so the second start leaves it alone.
        let dir = Scratch::dir("first-run");
        let marker = dir.join(install::FIRST_RUN_MARKER);
        assert!(
            install::is_first_run(&marker),
            "a daemon that never ran is a first run"
        );
        assert!(first_run_opens_dashboard(
            Start::Manual,
            install::is_first_run(&marker)
        ));
        install::mark_first_run_done(&marker).expect("the marker is written");
        assert!(
            !first_run_opens_dashboard(Start::Manual, install::is_first_run(&marker)),
            "the marker ends the first run: the dashboard must not open again"
        );
    }

    #[test]
    fn the_first_run_does_not_ask_how_many_packs_are_installed() {
        // The packs are not what makes a first run a first run: a user who unpacked the
        // zip has three of them and still lands on the dashboard. The gate is read from
        // the source, so a pack count cannot creep back into it.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("daemon.rs");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let gate: Vec<&str> = source
            .lines()
            .map(|line| line.trim())
            .filter(|line| line.starts_with("if ") && line.contains("first_run_opens_dashboard"))
            .collect();
        assert_eq!(gate.len(), 1, "the gate is one line: {gate:?}");
        assert!(
            !gate[0].contains("pack"),
            "the gate must not ask about the packs: {}",
            gate[0]
        );
    }

    /// A pack that is valid: enough for the installer to take it.
    const DEMO_PACK: &str = r#"{
        "spec": 1,
        "id": "demo",
        "name": "Demo",
        "version": "1.0.0",
        "targets": { "process": ["demo.exe"] },
        "apply": { "steps": [ { "mechanism": "process_kill" } ] },
        "revert": { "steps": [ { "mechanism": "process_kill" } ] }
    }"#;

    #[test]
    fn the_downloaded_pack_file_is_removed_after_the_install() {
        let root = Scratch::dir("install-ok");
        // The file the catalogue download leaves in the temp folder.
        let download_guard = Scratch::file(&format!(
            "{}{}.json",
            catalog::TEMP_PREFIX,
            std::process::id()
        ));
        let download = download_guard.root().to_path_buf();
        std::fs::write(&download, DEMO_PACK).unwrap();

        let installed = install_pack_file(&[root.root().to_path_buf()], &download);
        assert!(installed.is_ok(), "the pack installs: {installed:?}");
        assert!(
            !download.exists(),
            "the downloaded file is gone: it must not pile up in the temp folder"
        );
        assert!(
            root.join("packs").join("demo").join("pack.json").is_file(),
            "the pack landed in the packs folder"
        );
    }

    #[test]
    fn the_downloaded_pack_file_is_removed_after_a_failed_install() {
        let root = Scratch::dir("install-bad");
        let download_guard = Scratch::file(&format!(
            "{}{}-broken.json",
            catalog::TEMP_PREFIX,
            std::process::id()
        ));
        let download = download_guard.root().to_path_buf();
        std::fs::write(&download, "{ this is not a pack").unwrap();

        let installed = install_pack_file(&[root.root().to_path_buf()], &download);
        assert!(installed.is_err(), "a broken pack is refused");
        assert!(
            !download.exists(),
            "the downloaded file is gone even when the install failed"
        );
    }

    #[test]
    fn a_pack_the_user_picked_is_never_removed() {
        // The file dialog hands the daemon a path of the user's own: it stays where it
        // is, and so does every other file that is not a catalogue download.
        let dir = Scratch::dir("user-pack");
        let picked = dir.join("pack.json");
        std::fs::write(&picked, DEMO_PACK).unwrap();
        // A file with the catalogue's name, but somewhere else: not ours either.
        let elsewhere = dir.join(format!("{}.json", catalog::TEMP_PREFIX));
        std::fs::write(&elsewhere, DEMO_PACK).unwrap();

        let _ = install_pack_file(&[dir.root().to_path_buf()], &picked);
        let _ = install_pack_file(&[dir.root().to_path_buf()], &elsewhere);
        assert!(picked.exists(), "the user's own file stays where it is");
        assert!(elsewhere.exists(), "a file outside the temp folder is not ours");
    }

    /// The short (8.3) form of a path, when the system has one for it.
    fn short_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::GetShortPathNameW;
        use windows::core::PCWSTR;

        // A folder has to be asked for with a trailing separator.
        let mut text = path.as_os_str().encode_wide().collect::<Vec<u16>>();
        if !text.ends_with(&[b'\\' as u16]) {
            text.push(b'\\' as u16);
        }
        text.push(0);
        let mut buffer = vec![0u16; 32768];
        let len = unsafe { GetShortPathNameW(PCWSTR(text.as_ptr()), Some(buffer.as_mut_slice())) };
        if len == 0 {
            return None;
        }
        let short = String::from_utf16_lossy(&buffer[..(len as usize).min(buffer.len())]);
        Some(std::path::PathBuf::from(
            short.trim_end_matches(['\\', '/']).to_string(),
        ))
    }

    #[test]
    fn the_temp_file_guard_knows_the_short_and_the_long_form_of_the_folder() {
        // The temp folder can be spelled in the short 8.3 form ("C:\Users\PIXELA~1\...")
        // and in the long one: they are the same folder, and the file the catalogue
        // download leaves there is ours in both spellings.
        let name = format!("{}{}.json", catalog::TEMP_PREFIX, std::process::id());
        let long = std::env::temp_dir().join(&name);
        assert!(
            is_catalog_temp(&long),
            "the downloaded file is ours: {}",
            long.display()
        );
        let short = short_path(&std::env::temp_dir()).unwrap_or_else(|| std::env::temp_dir());
        let spelled = short.join(&name);
        assert!(
            is_catalog_temp(&spelled),
            "the same file, spelled the short way, is ours too: {}",
            spelled.display()
        );

        // A file the user picked is never ours: it does not sit in the temp folder.
        let dir = Scratch::dir("picked");
        let picked = dir.join("pack.json");
        assert!(!is_catalog_temp(&picked), "the user's own file is not ours");
        let elsewhere = dir.join(&name);
        assert!(
            !is_catalog_temp(&elsewhere),
            "a file outside the temp folder is not ours"
        );
        // And a file in the temp folder that does not carry the catalogue's name is
        // not ours either.
        let named = std::env::temp_dir().join("pack.json");
        assert!(!is_catalog_temp(&named), "another file in the temp folder is not ours");
    }

    #[test]
    fn an_automatic_open_is_refused_while_a_tracked_app_runs() {
        // The band must not pop up over a game: an automatic open (the daemon starts, the
        // machine resumes from sleep, the open mode changes) waits for the game to be gone.
        assert!(
            !console_open_allowed(ConsoleOpen::Automatic, true),
            "an automatic open must not interrupt a running game"
        );
        assert!(console_open_allowed(ConsoleOpen::Automatic, false));
    }

    #[test]
    fn an_explicit_open_always_opens_the_band() {
        // The user asked for it (the tray's "Console mode", the dashboard's console
        // button): the band opens even while a game is running.
        assert!(
            console_open_allowed(ConsoleOpen::Explicit, true),
            "an explicit request must open the band"
        );
        assert!(console_open_allowed(ConsoleOpen::Explicit, false));
    }

    #[test]
    fn a_daemon_without_tracked_apps_has_no_game_to_interrupt() {
        let cfg = AppConfig::default();
        assert!(cfg.tracked_apps.is_empty());
        assert!(!tracked_app_running_now(&cfg));
    }

    #[test]
    fn the_process_walk_finds_the_running_processes() {
        // The walk is what the startup open of the band relies on: it has to see the
        // process list at least once.
        assert!(!running_process_names().is_empty());
    }

    #[test]
    fn a_console_launcher_that_exited_is_cleared() {
        assert_eq!(console_action(4242, true, false), ConsoleAction::Clear);
        // an exited launcher is cleared even when its band is gone as well
        assert_eq!(console_action(4242, true, true), ConsoleAction::Clear);
        // and there is nothing to keep when there is no launcher at all
        assert_eq!(console_action(0, false, false), ConsoleAction::Clear);
    }

    #[test]
    fn a_console_launcher_without_a_window_is_replaced() {
        // the band was closed, or the OS minimised it: the launcher is alive but can
        // never be used again, so it is replaced instead of kept
        assert_eq!(console_action(4242, false, true), ConsoleAction::Replace);
    }

    #[test]
    fn a_process_without_a_window_has_none() {
        // pid 0 is the idle process: it never owns a window, so the enumeration finds
        // nothing. The one probe that reads the same on every machine.
        assert!(!console_window_up(0));
    }

    #[test]
    fn a_pack_failure_is_remembered_and_a_clean_run_forgets_it() {
        let mut last_error: Option<LastError> = None;
        remember_error(&mut last_error, "pack:g-helper", "the hotkey did not land");
        let err = last_error.clone().expect("the failure is remembered");
        assert_eq!(err.source, "pack:g-helper");
        assert_eq!(err.message, "the hotkey did not land");
        assert!(err.at > 0);

        // the most recent failure wins
        remember_error(&mut last_error, "pack:rainmeter", "the layout is gone");
        assert_eq!(last_error.as_ref().unwrap().source, "pack:rainmeter");

        // a clean run clears it again
        forget_error(&mut last_error, "pack:");
        assert!(last_error.is_none());
    }

    #[test]
    fn a_failure_of_another_kind_survives_a_clean_pack_run() {
        let mut last_error: Option<LastError> = None;
        remember_error(&mut last_error, "catalog", "the manifest could not be read");
        forget_error(&mut last_error, "pack:");
        assert_eq!(last_error.as_ref().unwrap().source, "catalog");
    }

    #[test]
    fn the_pipe_is_opened_to_the_user_the_administrators_and_system() {
        let sddl = pipe_sddl();
        assert!(sddl.starts_with("D:P"));
        assert!(sddl.contains("(A;;GA;;;SY)")); // SYSTEM
        assert!(sddl.contains("(A;;GA;;;BA)")); // Administrators
        assert!(!sddl.contains("WD)")); // not "everyone"
        assert!(!sddl.contains("AU)")); // not "authenticated users"
        match current_user_sid() {
            Some(sid) => {
                assert!(sid.starts_with("S-1-"));
                assert!(sddl.contains(&format!("(A;;GA;;;{sid})")));
            }
            None => assert_eq!(sddl, "D:P(A;;GA;;;SY)(A;;GA;;;BA)"),
        }
    }

    #[test]
    fn the_pipe_security_descriptor_is_accepted_by_windows() {
        use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt;
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        };
        let sddl = pipe_sddl();
        let wide = widestring::U16CString::from_str(&sddl).expect("the sddl is well formed");
        let sd = SecurityDescriptor::deserialize(wide.as_ucstr())
            .unwrap_or_else(|e| panic!("the sddl '{sddl}' was refused: {e}"));
        // A valid descriptor can be turned back into SDDL.
        let text = sd
            .serialize(
                DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION,
                |s| s.to_string_lossy().to_string(),
            )
            .expect("the descriptor is serialized");
        assert!(text.contains("SY"));
        assert!(text.contains("BA"));
    }

    #[test]
    fn a_client_of_the_same_user_still_reaches_the_pipe() {
        // The pipe as the daemon opens it: the same descriptor, a name of its own so the
        // running daemon is not touched.
        let raw = format!("armoury-daemon-test-{}.pipe", std::process::id());
        let name = raw.as_str().to_ns_name::<GenericNamespaced>().expect("the name");
        let mut options = ListenerOptions::new().name(name);
        let sd = pipe_security_descriptor().expect("the descriptor");
        options = options.security_descriptor(sd);
        let listener = options.create_sync().expect("the listener");

        let server = thread::spawn(move || {
            let mut conn = listener
                .incoming()
                .next()
                .expect("a client connects")
                .expect("no error");
            let mut reader = std::io::BufReader::new(&mut conn);
            let mut buf = String::new();
            armoury_core::protocol::read_msg(&mut reader, &mut buf).expect("a request")
        });

        let name = raw.as_str().to_ns_name::<GenericNamespaced>().expect("the name");
        let mut client = ConnectOptions::new()
            .name(name)
            .connect_sync()
            .expect("the client connects");
        armoury_core::protocol::write_msg(&mut client, &Request::GetStatus).expect("the request");
        let request = server.join().expect("the server thread");
        assert!(matches!(request, Some(Request::GetStatus)));
    }

    /// The daemon's side of the engine: what the packs really do, and the daemon's own rule
    /// about when a transition is performed.
    struct Machine {
        engine: Engine,
        /// What the packs actually do: true = applied.
        applied: bool,
        /// The daemon's pause switch.
        paused: bool,
    }

    impl Machine {
        /// The daemon's `send_transition`.
        fn send(&mut self, t: Transition, force: bool) {
            if !transition_allowed(self.paused, force) {
                // The packs were not changed: the engine must not believe that they were.
                self.engine.transition_not_performed(t);
                return;
            }
            self.applied = match t {
                Transition::Applied => true,
                Transition::Reverted => false,
            };
        }
    }

    #[test]
    fn leaving_idle_while_paused_applies_the_packs_again() {
        // The dashboard showed "Triggered" while the packs were still reverted: leaving idle
        // mode took the paused path, so the apply was dropped — and the engine believed it
        // had happened, so nothing ever applied the packs again.
        let now = Instant::now();
        let mut m = Machine {
            engine: Engine::new(Duration::from_secs(60)),
            applied: false,
            paused: false,
        };

        // a tracked app starts: the packs are applied
        let t = m
            .engine
            .on_started("game.exe", "game.exe", 1, now)
            .expect("the packs are applied");
        m.send(t, false);
        assert!(m.applied);

        // the user pauses the switching
        m.paused = true;
        assert!(m.engine.set_paused(true, now).is_none());

        // idle mode: the packs are reverted (a manual action, so it happens while paused)
        let t = m.engine.set_idle(true).expect("idle reverts the packs");
        m.send(t, IDLE_IS_FORCED);
        assert!(!m.applied);
        assert!(!m.engine.is_applied());

        // idle mode ends: the packs are applied again — that is a manual action too
        let t = m
            .engine
            .set_idle(false)
            .expect("leaving idle applies the packs again");
        m.send(t, IDLE_IS_FORCED);
        assert!(m.applied, "leaving idle must apply the packs again");
        assert_eq!(m.engine.is_applied(), m.applied, "the engine and the machine agree");

        // and unpausing changes nothing
        m.paused = false;
        if let Some(t) = m.engine.set_paused(false, now) {
            m.send(t, false);
        }
        assert!(m.applied, "the packs must be applied after the unpause");
        assert_eq!(m.engine.is_applied(), m.applied, "the engine and the machine agree");
    }

    #[test]
    fn a_transition_the_daemon_drops_leaves_the_engine_and_the_machine_alike() {
        // The pause drops an automatic transition: the engine must not believe that the
        // packs changed, or the next trigger would find "already applied" and do nothing.
        let now = Instant::now();
        let mut m = Machine {
            engine: Engine::new(Duration::from_secs(60)),
            applied: false,
            paused: true,
        };
        m.engine.on_started("game.exe", "game.exe", 1, now);
        // the transition the engine asks for, dropped by the pause
        let t = Transition::Applied;
        m.send(t, false);
        assert!(!m.applied, "the machine did not change");
        assert!(!m.engine.is_applied(), "and the engine knows it");
    }

    #[test]
    fn a_tracked_app_is_removed_by_its_file_name() {
        let mut list = vec![
            TrackedApp {
                name: "Demo".into(),
                exe: "C:\\games\\demo.exe".into(),
            },
            TrackedApp {
                name: "Other".into(),
                exe: "C:\\games\\other.exe".into(),
            },
        ];
        // a client that rebuilds the path removes the app just the same
        let removed = remove_tracked_apps(&mut list, "D:\\copy\\DEMO.exe");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "Demo");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Other");

        // the plain file name works too
        let removed = remove_tracked_apps(&mut list, "other.exe");
        assert_eq!(removed.len(), 1);
        assert!(list.is_empty());

        // a name that is not in the list removes nothing — and says so
        assert!(remove_tracked_apps(&mut list, "nothing.exe").is_empty());
    }
}
