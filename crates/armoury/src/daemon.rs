//! Daemon mode: single instance, tray icon + menu, End/Home keyboard hook,
//! named-pipe server, UI process lifecycle (spawn/focus/kill), status broadcasts.

use crate::art;
use crate::console;
use crate::hotkey;
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
use armoury_core::protocol::{Request, Response};
use armoury_core::status::{LastError, PackSettingStatus, PackStatus, RunState, StatusSnapshot};
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced, ListenerOptions};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use log::{error, info, warn};
use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::UI::WindowsAndMessaging::{MsgWaitForMultipleObjectsEx, QS_ALLINPUT};

pub fn run(show_ui_when_running: bool) {
    if let Err(e) = armoury_core::logging::init("daemon") {
        eprintln!("logging init failed: {e}");
    }
    info!("daemon starting (v{})", armoury_core::VERSION);
    if let Some(code) = crate::dpi_error() {
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

    // ---- config ----
    let cfg = armoury_core::config::load_or_create().unwrap_or_else(|e| {
        error!("config load failed ({e}); using in-memory defaults");
        AppConfig::default()
    });
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
    console::start(pad_tx, wake_handle);
    let engine = {
        let mut e = Engine::new(Duration::from_secs(cfg.cooldown_seconds.max(1)));
        if cfg.paused {
            // start frozen, exactly as the user left it
            e.set_paused(true, Instant::now());
        }
        e
    };

    // ---- packs ----
    let search = pack_search_dirs();
    let mut installed = packs::load(&search);
    enrich_rainmeter(&mut installed);

    let pack_catalog: Vec<PackMeta> = build_catalog(&installed);
    let (pack_tx, pack_rx) = crossbeam_channel::unbounded::<packs::PackJob>();
    let (pack_out_tx, pack_out_rx) = crossbeam_channel::unbounded::<packs::PackOutcome>();
    packs::start(installed, pack_rx, pack_out_tx, wake_handle);
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
        let _ = pack_tx.send(job);
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
        pad_rx,
        art_tx,
        art_rx,
        art_busy: false,
        // A pass with the old (keyless) key may still be running: it leaves miss markers.
        art_retry_after_pass: false,
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
    info!("daemon ready");
    app.ensure_art(); // tracked apps get their covers even before the console opens
    if app.cfg.console_open == CONSOLE_OPEN_ALWAYS {
        info!("console: open mode 'always' — starting the launcher");
        app.show_console();
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
                "console" => app.show_console(),
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
        loop {
            match app.pack_out_rx.try_recv() {
                Ok(o) => app.on_pack_outcome(o),
                Err(_) => break,
            }
        }
        if app.quit_requested.asked() {
            // "Quit means close everything": the packs go back and the audio comes back before
            // the daemon goes away.
            app.revert_before_quit();
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
    search
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
    pub settings: Vec<armoury_core::pack::Setting>,
}

fn build_catalog(installed: &[packs::Installed]) -> Vec<PackMeta> {
    installed
        .iter()
        .map(|p| PackMeta {
            id: p.pack.id.clone(),
            name: p.pack.name.clone(),
            version: p.pack.version.clone(),
            settings: p.pack.settings.clone(),
        })
        .collect()
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
    /// Jobs to the pack worker (apply/revert).
    pack_tx: crossbeam_channel::Sender<packs::PackJob>,
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
    /// Pad hotplug events from the controller watch.
    pad_rx: crossbeam_channel::Receiver<console::PadEvent>,
    /// Cover-art passes: worker -> main loop (a ready key, or "" when a pass finished).
    art_tx: crossbeam_channel::Sender<String>,
    art_rx: crossbeam_channel::Receiver<String>,
    art_busy: bool,
    /// A pass with the old (keyless) SteamGridDB key is still running: it leaves miss markers
    /// behind, so they have to be cleared again when that pass ends.
    art_retry_after_pass: bool,
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
            // The wire format keeps the field, but the engine no longer plans a switch: it
            // only knows the lockout, which is reported as the remaining time.
            cooldown_action: None,
            cooldown_seconds: self.cfg.cooldown_seconds,
            paused: self.cfg.paused,
            killswitch_active: self.killswitch.is_active(),
            killswitch_count: (self.running_pids.values().flatten().count()) as u32,
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

    /// `force` = a manual action (entering idle, removing the last tracked app) that must take
    /// effect even while the daemon is paused (user decision 2026-10-18: pause stops *automatic*
    /// switching, not the user's own actions).
    fn send_transition(&mut self, t: Transition, force: bool) {
        if self.cfg.paused && !force {
            info!("paused — skipping the {:?} transition", t);
            self.broadcast_status();
            return;
        }
        // Hand the transition to the pack worker (it owns the mechanisms).
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
        let _ = self.pack_tx.send(job);
        match t {
            Transition::Applied => info!(
                "packs applied (triggered by {})",
                self.engine.source().unwrap_or_default()
            ),
            Transition::Reverted => info!("packs reverted"),
        }
        self.broadcast_status();
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

        if o.applied {
            self.applied_packs = o.applied_ids.clone();
            // keep the journal so the matching revert restores the right state
            self.captured_by_pack = o.captured.clone();
        } else {
            self.applied_packs.clear();
            self.captured_by_pack.clear();
        }
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

    /// Re-read the packs folders (after an install/remove) and give the worker the new list.
    /// Dropping the old sender makes the old worker thread exit on its own.
    fn reload_packs(&mut self) {
        let mut installed = packs::load(&self.search);
        enrich_rainmeter(&mut installed);
        self.pack_catalog = build_catalog(&installed);
        let (tx, rx) = crossbeam_channel::unbounded::<packs::PackJob>();
        let (out_tx, out_rx) = crossbeam_channel::unbounded::<packs::PackOutcome>();
        packs::start(installed, rx, out_tx, self.wake);
        self.pack_tx = tx;
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
                let _ = self.pack_tx.send(job);
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
        let _ = self.pack_tx.send(job);
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
        let before = self.cfg.tracked_apps.len();
        self.cfg
            .tracked_apps
            .retain(|a| !a.exe.eq_ignore_ascii_case(exe));
        if self.cfg.tracked_apps.len() != before {
            info!("tracked app removed: {exe}");
            self.save_and_share_tracked();
            // The entry is gone, so nothing holds the packs applied any more: removing the last
            // tracked app reverts the packs at once. It is a manual action, so it bypasses the
            // lockout (and the pause, G12) and leaves no lockout behind — the next tracked app
            // may apply right away.
            let key = basename_lower(exe);
            if let Some(t) = self.engine.on_untracked(&key, Instant::now()) {
                info!("tracked app removed — reverting the packs now");
                self.send_transition(t, true);
            }
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
            self.send_transition(t, true);
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
        // Idle over: re-apply if a tracked app is running at this moment (decision B8).
        if let Some(t) = self.engine.set_idle(false) {
            info!("idle mode over: a tracked app is running, applying the packs again");
            self.on_transition(t);
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
        let _ = self.pack_tx.send(job);

        // Shutdown is not time-critical; the kill/relaunch paths can take a few seconds.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.pack_out_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(outcome) => {
                    let reverted = !outcome.applied;
                    self.on_pack_outcome(outcome);
                    if reverted {
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
        if !self.engine.running_names().is_empty() {
            info!("console mode: kept closed — a tracked app is running");
            return;
        }
        self.show_console();
    }

    /// Open the console launcher now (tray menu / settings preview / a fresh pad connect).
    fn show_console(&mut self) {
        self.reap_console_child();
        if self.console_child.is_some() {
            info!("console launcher is already running");
            return;
        }
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        use std::os::windows::process::CommandExt;

        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                warn!("cannot resolve own executable path: {e}");
                return;
            }
        };
        match Command::new(&exe)
            .arg("--console")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            Ok(child) => {
                info!("spawned console launcher (pid {})", child.id());
                self.console_child = Some(child);
            }
            Err(e) => warn!("failed to spawn the console launcher: {e}"),
        }
    }

    fn reap_console_child(&mut self) {
        if let Some(child) = &mut self.console_child {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    info!("console launcher exited ({exit})");
                    self.console_child = None;
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
            return;
        }
        info!("cover art: {key} is ready");
        self.broadcast_status(); // the console re-reads the art folder on status updates
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
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        use std::os::windows::process::CommandExt;

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
                Request::ShowConsole => self.show_console(),
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
                    // now rather than at the next daemon start.
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
                    // The daemon spawns; for a tracked app the watcher then triggers the packs
                    // exactly as if the user had opened it by hand.
                    // Launch like Explorer does: CWD = the exe's own folder. Plenty of games
                    // (and repack launchers) resolve data files relative to the working
                    // directory — inheriting the daemon's folder breaks them.
                    let mut cmd = Command::new(&exe);
                    if let Some(dir) = std::path::Path::new(&exe)
                        .parent()
                        .filter(|d| !d.as_os_str().is_empty())
                    {
                        cmd.current_dir(dir);
                    }
                    match cmd.spawn() {
                        Ok(child) => {
                            info!("console: launched {exe} (pid {})", child.id());
                            self.record_played(&exe);
                        }
                        Err(e) => warn!("console: cannot launch {exe}: {e}"),
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
                        .set_cooldown(Duration::from_secs(self.cfg.cooldown_seconds));
                    if let Err(e) = armoury_core::config::save(&self.cfg) {
                        warn!("failed to save config: {e}");
                    }
                    info!("cooldown set to {}s", self.cfg.cooldown_seconds);
                    self.broadcast_status();
                }
                Request::SetUiPosition { .. } => {
                    // The dashboard always opens in the corner: the position is not kept, so the
                    // request is accepted and dropped.
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
                Request::InstallPack { source } => {
                    let result =
                        packs::install_from_path(&self.search, std::path::Path::new(&source));
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
                            let _ = self.pack_tx.send(job);
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
fn current_user_sid() -> Option<String> {
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
fn current_user_name() -> Option<String> {
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
    fn the_quit_request_asks_for_the_shutdown() {
        let mut quit = QuitRequest::default();
        assert!(!quit.asked());
        quit.ask();
        assert!(quit.asked());
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
}
