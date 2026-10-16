//! Daemon mode: single instance, tray icon + menu, Scroll Lock global hotkey,
//! named-pipe server, UI process lifecycle (spawn/focus/kill), status broadcasts.

use crate::ipc;
use crate::watch;
use armoury_core::config::{AppConfig, TrackedApp};
use armoury_core::engine::{Engine, EngineState, Transition};
use armoury_core::protocol::{Request, Response};
use armoury_core::status::{RunState, StatusSnapshot};
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced, ListenerOptions};
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

pub fn run() {
    if let Err(e) = armoury_core::logging::init("daemon") {
        eprintln!("logging init failed: {e}");
    }
    info!("daemon starting (v{})", armoury_core::VERSION);

    // ---- single instance ----
    let _mutex_guard = match acquire_singleton() {
        Singleton::AlreadyRunning => {
            info!("another instance is already running — asking it to show the UI");
            nudge_existing_instance();
            return;
        }
        Singleton::Acquired(h) => h,
    };

    // ---- config ----
    let cfg = armoury_core::config::load_or_create().unwrap_or_else(|e| {
        error!("config load failed ({e}); using in-memory defaults");
        AppConfig::default()
    });
    info!(
        "config: cooldown {}s, {} tracked app(s), voice {}",
        cfg.cooldown_seconds,
        cfg.tracked_apps.len(),
        if cfg.voice_enabled { "on" } else { "off" }
    );

    // ---- named-pipe server ----
    let name = match armoury_core::PIPE_NAME.to_ns_name::<GenericNamespaced>() {
        Ok(n) => n,
        Err(e) => {
            error!("invalid pipe name: {e}");
            return;
        }
    };
    let listener = match ListenerOptions::new().name(name).create_sync() {
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
    info!("pipe server ready ({})", armoury_core::PIPE_NAME);

    // ---- tracked-app watcher + trigger engine ----
    let tracked = Arc::new(Mutex::new(cfg.tracked_apps.clone()));
    let (watch_tx, watch_rx) = crossbeam_channel::unbounded::<watch::WatchEvent>();
    watch::start(tracked.clone(), watch_tx, wake_handle);
    let engine = Engine::new(Duration::from_secs(cfg.cooldown_seconds.max(1)));
    info!(
        "watching {} tracked app(s); cooldown {}s",
        cfg.tracked_apps.len(),
        cfg.cooldown_seconds
    );

    // ---- tray icon + menu (created on this thread: it owns the message pump) ----
    let menu = Menu::new();
    let open_item = MenuItem::with_id("open", "Open dashboard", true, None);
    let pause_item = MenuItem::with_id("pause_all", "Pause all packs", true, None);
    let kill_item = MenuItem::with_id("killswitch", "Killswitch (coming soon)", false, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);
    if let Err(e) = menu.append_items(&[&open_item, &pause_item, &kill_item, &quit_item]) {
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

    // ---- Scroll Lock global hotkey ----
    let hotkey_manager: Option<GlobalHotKeyManager> = match GlobalHotKeyManager::new() {
        Ok(m) => Some(m),
        Err(e) => {
            warn!("hotkey manager unavailable: {e}");
            None
        }
    };
    let hotkey = HotKey::new(None, Code::ScrollLock);
    let mut hotkey_registered = false;
    if let Some(m) = &hotkey_manager {
        match m.register(hotkey) {
            Ok(()) => {
                hotkey_registered = true;
                info!("Scroll Lock hotkey registered");
            }
            Err(e) => warn!("Scroll Lock hotkey registration failed: {e}"),
        }
    }

    // ---- main loop ----
    // A single waitable event lets every producer (watcher, pipe threads) wake the loop
    // immediately; Win32 messages (tray, hotkey) wake it through the message queue.
    let tray_rx = TrayIconEvent::receiver();
    let menu_rx = MenuEvent::receiver();
    let hotkey_rx = GlobalHotKeyEvent::receiver();

    let mut app = App {
        cfg,
        pause_item: pause_item.clone(),
        tracked,
        running_paths: HashMap::new(),
        engine,
        started: Instant::now(),
        clients: HashMap::new(),
        ui_conn: None,
        ui_child: None,
        spawning_ui: false,
        last_broadcast: Instant::now(),
    };
    let mut last_housekeeping = Instant::now();
    if app.cfg.paused {
        app.pause_item.set_text("Resume all packs");
        info!("starting paused (packs will not switch until resumed)");
    }
    info!("daemon ready");

    let quit = loop {
        pump_win32_messages();

        // drain every source without blocking
        let mut quit = false;
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
                "quit" => {
                    info!("quit requested from tray menu");
                    quit = true;
                }
                other => info!("tray menu '{other}' — not implemented yet"),
            }
        }
        while let Ok(e) = hotkey_rx.try_recv() {
            if e.state == HotKeyState::Pressed {
                app.on_hotkey();
            }
        }
        while let Ok(p) = pipe_rx.try_recv() {
            app.on_pipe(p);
        }
        while let Ok(w) = watch_rx.try_recv() {
            app.on_watch(w);
        }
        if quit {
            break true;
        }

        if last_housekeeping.elapsed() >= Duration::from_secs(1) {
            last_housekeeping = Instant::now();
            app.on_tick();
        }

        // Block until a message arrives, a producer signals the event, or 1 s passes.
        // Idle cost is therefore ~zero (no polling tick).
        unsafe {
            MsgWaitForMultipleObjectsEx(1, &wake, 1000, QS_ALLINPUT, 0);
        }
    };

    if !quit {
        info!("main loop ended unexpectedly");
    }

    // ---- shutdown ----
    info!("shutting down");
    if let Some(m) = &hotkey_manager {
        if hotkey_registered {
            let _ = m.unregister(hotkey);
        }
    }
    if let Some(mut child) = app.ui_child.take() {
        let _ = child.kill();
    }
    drop(tray);
    log::logger().flush();
}

fn basename_lower(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| path.to_ascii_lowercase())
}

/// Tray icon built from the shared procedural icon generator.
fn make_tray_icon() -> tray_icon::Icon {    let (rgba, w, h) = armoury_core::icon::rgba_icon(32);
    tray_icon::Icon::from_rgba(rgba, w, h).expect("tray icon rgba is 32x32")
}

// ---------------------------------------------------------------------------

struct App {
    cfg: AppConfig,
    /// Tray menu item whose label flips between Pause/Resume.
    pause_item: tray_icon::menu::MenuItem,
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
    last_broadcast: Instant,
}

impl App {
    fn status(&self) -> StatusSnapshot {
        let now = Instant::now();
        let (state, trigger_source) = match self.engine.state() {
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
            cooldown_seconds: self.cfg.cooldown_seconds,
            paused: self.cfg.paused,
            killswitch_active: false,
            tracked_apps: self.cfg.tracked_apps.clone(),
            tracked_apps_running: self.engine.running_names(),
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
                self.engine.on_started(&key, &display, pid, now)
            }
            watch::WatchEvent::Stopped { key, pid } => {
                let t = self.engine.on_stopped(&key, pid, now);
                if !self.engine.is_running(&key) {
                    self.running_paths.remove(&key);
                }
                t
            }
        };
        if let Some(t) = transition {
            self.on_transition(t);
        }
    }

    fn on_transition(&mut self, t: Transition) {
        if self.cfg.paused {
            info!("paused — skipping the {:?} transition", t);
            self.broadcast_status();
            return;
        }
        match t {
            Transition::Applied => info!(
                "packs APPLIED (triggered by {}) [pack execution lands in Phase 3]",
                self.engine.source().unwrap_or_default()
            ),
            Transition::Reverted => {
                info!("packs REVERTED (cooldown expired with nothing running)")
            }
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
        self.broadcast_status();
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

    fn on_hotkey(&mut self) {
        // Phase 4 turns this into the killswitch; for now, prove the wiring.
        info!("Scroll Lock pressed (killswitch wiring check — Phase 4)");
    }

    fn on_tick(&mut self) {
        if let Some(t) = self.engine.tick(Instant::now()) {
            self.on_transition(t);
        }
        if self.last_broadcast.elapsed() >= Duration::from_secs(1) {
            self.last_broadcast = Instant::now();
            self.broadcast_status();
            self.reap_ui_child();
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
                Request::AddTrackedApp { name, exe } => self.add_tracked_app(name, exe),
                Request::RemoveTrackedApp { exe } => self.remove_tracked_app(&exe),
                Request::SetPaused { paused } => self.set_paused(paused),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 plumbing

/// Drain and dispatch pending window messages (tray icon, hotkey, taskbar restarts).
fn pump_win32_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
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
    use windows::core::w;

    let handle = unsafe { CreateMutexW(None, false, w!("Local\\ArmouryDaemon.Singleton")) };
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
    for attempt in 0..5 {
        let Ok(name) = armoury_core::PIPE_NAME.to_ns_name::<GenericNamespaced>() else {
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
