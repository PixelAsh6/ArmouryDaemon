//! Daemon mode: single instance, tray icon + menu, Scroll Lock global hotkey,
//! named-pipe server, UI process lifecycle (spawn/focus/kill), status broadcasts.

use crate::ipc;
use crate::killswitch;
use crate::overlay;
use crate::packs;
use crate::watch;
use armoury_core::config::{AppConfig, TrackedApp};
use armoury_core::engine::{Engine, EngineState, Transition};
use armoury_core::protocol::{Request, Response};
use armoury_core::status::{PackStatus, RunState, StatusSnapshot};
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

pub fn run(show_ui_when_running: bool) {
    if let Err(e) = armoury_core::logging::init("daemon") {
        eprintln!("logging init failed: {e}");
    }
    info!("daemon starting (v{})", armoury_core::VERSION);
    killswitch::init_com();
    overlay::recover_if_needed();

    // ---- single instance ----
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
    let kill_item = MenuItem::with_id("killswitch", "Toggle killswitch", true, None);
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
        kill_item: kill_item.clone(),
        tray,
        icon_normal: make_tray_icon(),
        icon_active: make_tray_icon_active(),
        killswitch: killswitch::Ledger::default(),
        running_pids: HashMap::new(),
        echo_until: None,
        overlay: overlay::Overlay::default(),
        idle_engage_tick: 0,
        idle_deadline: None,
        tracked,
        running_paths: HashMap::new(),
        engine,
        search,
        pack_tx,
        pack_out_rx,
        pack_catalog,
        applied_packs: Vec::new(),
        captured_by_pack: HashMap::new(),
        started: Instant::now(),
        clients: HashMap::new(),
        ui_conn: None,
        ui_child: None,
        spawning_ui: false,
        last_broadcast: Instant::now(),
        wake: wake_handle,
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
                "killswitch" => app.toggle_killswitch(),
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
        loop {
            match app.pack_out_rx.try_recv() {
                Ok(o) => app.on_pack_outcome(o),
                Err(_) => break,
            }
        }
        if quit {
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
    if let Some(m) = &hotkey_manager {
        if hotkey_registered {
            let _ = m.unregister(hotkey);
        }
    }
    if let Some(mut child) = app.ui_child.take() {
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

/// Tick of the last keyboard/mouse input in this session.
fn last_input_tick() -> u32 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        GetLastInputInfo(&mut info);
    }
    info.dwTime
}

fn tick_count() -> u32 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() }
}

/// The current Scroll Lock toggle state (GetAsyncKeyState reads the global toggle).
fn scroll_lock_state() -> bool {    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    unsafe { GetAsyncKeyState(0x91) & 1 != 0 }
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
fn pack_search_dirs() -> Vec<std::path::PathBuf> {
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
    pub setting: Option<armoury_core::pack::Setting>,
}

fn build_catalog(installed: &[packs::Installed]) -> Vec<PackMeta> {
    installed
        .iter()
        .map(|p| PackMeta {
            id: p.pack.id.clone(),
            name: p.pack.name.clone(),
            version: p.pack.version.clone(),
            setting: p.pack.settings.first().cloned(),
        })
        .collect()
}

struct App {
    cfg: AppConfig,
    /// Tray menu item whose label flips between Pause/Resume.
    pause_item: tray_icon::menu::MenuItem,
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
    /// Ignore our own Scroll Lock correction presses until this instant.
    echo_until: Option<Instant>,
    /// Idle overlay (black screens + muted output devices).
    overlay: overlay::Overlay,
    /// Tick count when idle mode started, so any later input dismisses it.
    idle_engage_tick: u32,
    idle_deadline: Option<Instant>,
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
    /// The wake event every producer signals (handed to each pack worker).
    wake: usize,
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
            killswitch_active: self.killswitch.is_active(),
            killswitch_count: (self.running_pids.values().flatten().count()) as u32,
            idle_active: self.overlay.is_active(),
            idle_monitors: self.overlay.monitor_count() as u32,
            idle_devices: self.overlay.muted_devices() as u32,
            idle_failsafe_enabled: self.cfg.idle_failsafe_enabled,
            idle_failsafe_minutes: self.cfg.idle_failsafe_minutes,
            tracked_apps: self.cfg.tracked_apps.clone(),
            tracked_apps_running: self.engine.running_names(),
            packs: self
                .pack_catalog
                .iter()
                .map(|m| {
                    let user = self.cfg.packs.get(&m.id);
                    let enabled = user.map(|u| u.enabled).unwrap_or(false);
                    let (key, label, values, value) = match &m.setting {
                        Some(s) => (
                            Some(s.key.clone()),
                            Some(s.label.clone()),
                            s.values.clone(),
                            Some(
                                user.and_then(|u| u.values.get(&s.key).cloned())
                                    .or_else(|| s.default.clone())
                                    .unwrap_or_default(),
                            ),
                        ),
                        None => (None, None, Vec::new(), None),
                    };
                    PackStatus {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        version: m.version.clone(),
                        enabled,
                        setting_key: key,
                        setting_label: label,
                        values,
                        value,
                        applied: self.applied_packs.contains(&m.id),
                    }
                })
                .collect(),
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
        if self.cfg.paused {
            info!("paused — skipping the {:?} transition", t);
            self.broadcast_status();
            return;
        }
        // Hand the transition to the pack worker (it owns the mechanisms).
        let job = packs::PackJob {
            apply: t == Transition::Applied,
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

    fn on_pack_outcome(&mut self, o: packs::PackOutcome) {
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
        for (id, err) in &o.failed {
            warn!("pack {id} failed: {err}");
        }
        self.write_journal(&o);
        self.broadcast_status();
    }

    /// The journal is also the restart record: it must carry everything a revert needs
    /// (which packs, their dropdown values and the values captured at apply time).
    fn write_journal(&self, o: &packs::PackOutcome) {
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
            let _ = std::fs::write(&path, text);
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
        info!("pack {id}: {key} = {value} (applies on the next trigger)");
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
        // Ignore the correction presses we send ourselves.
        if self
            .echo_until
            .map_or(false, |t| Instant::now() < t)
        {
            info!("Scroll Lock echo ignored");
            return;
        }

        // Windows toggles the Scroll Lock state at driver level; the press that just
        // arrived has already flipped it. Whatever it was before, we want it back.
        let want = !scroll_lock_state();
        self.toggle_killswitch();

        for _ in 0..3 {
            if scroll_lock_state() == want {
                break;
            }
            self.echo_until = Some(Instant::now() + Duration::from_millis(500));
            if let Err(e) = packs::send_chord("scrolllock") {
                warn!("could not send the Scroll Lock correction: {e}");
                break;
            }
            std::thread::sleep(Duration::from_millis(80));
        }

        if scroll_lock_state() == want {
            info!("Scroll Lock state kept (was {}, still {})", !want as u8, want as u8);
        } else {
            warn!("Scroll Lock state could not be corrected (left toggled)");
        }
    }

    fn enter_idle(&mut self) {
        if self.overlay.is_active() {
            return;
        }
        // marker first: if we crash while idle, the next start can restore the audio
        let _ = std::fs::write(overlay::marker_path(), b"idle");
        overlay::show(&mut self.overlay);
        self.idle_engage_tick = tick_count();
        self.idle_deadline = if self.cfg.idle_failsafe_enabled {
            Some(Instant::now() + Duration::from_secs(self.cfg.idle_failsafe_minutes.max(1) * 60))
        } else {
            None
        };
        // Idle cancels switching: whatever is applied reverts now (decision B8).
        if let Some(t) = self.engine.set_idle(true) {
            info!("idle mode: tracked apps are normal apps again");
            self.on_transition(t);
        }
        self.broadcast_status();
    }

    fn exit_idle(&mut self) {
        if !self.overlay.is_active() {
            return;
        }
        overlay::hide(&mut self.overlay);
        let _ = std::fs::remove_file(overlay::marker_path());
        self.idle_deadline = None;
        // Idle over: re-apply if a tracked app is running at this moment (decision B8).
        if let Some(t) = self.engine.set_idle(false) {
            info!("idle mode over: a tracked app is running, applying the packs again");
            self.on_transition(t);
        }
        self.broadcast_status();
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

    fn on_tick(&mut self) {
        if self.overlay.is_active() {
            overlay::pump();
            // any input after we went idle dismisses the overlay
            if last_input_tick().wrapping_sub(self.idle_engage_tick) > 0 {
                info!("idle overlay dismissed by user input");
                self.exit_idle();
            } else if let Some(deadline) = self.idle_deadline {
                if Instant::now() >= deadline {
                    warn!(
                        "idle failsafe fired after {} min — leaving idle mode",
                        self.cfg.idle_failsafe_minutes
                    );
                    self.exit_idle();
                }
            }
        }
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
                Request::SetPackEnabled { id, enabled } => self.set_pack_enabled(id, enabled),
                Request::SetPackValue { id, key, value } => self.set_pack_value(id, key, value),
                Request::SetIdle { active } => {
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
                            self.reply_pack_op(id, true, message);
                        }
                        Err(e) => {
                            warn!("pack install failed: {e}");
                            self.reply_pack_op(id, false, e);
                        }
                    }
                }
                Request::RemovePack { id: pack_id } => {
                    let result = packs::remove_id(&self.search, &pack_id);
                    match result {
                        Ok(removed) => {
                            self.reload_packs();
                            let message = format!("{removed} removed");
                            info!("pack op: {message}");
                            self.reply_pack_op(id, true, message);
                        }
                        Err(e) => {
                            warn!("pack remove failed: {e}");
                            self.reply_pack_op(id, false, e);
                        }
                    }
                }
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
