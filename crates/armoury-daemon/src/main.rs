//! ArmouryDaemon — resident core.
//! Phase 1: single instance, tray icon + menu, Scroll Lock global hotkey (placeholder),
//! named-pipe server, spawn/focus/kill of the UI process, status broadcasting.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod icon;
mod ipc;

use armoury_core::protocol::{Request, Response};
use armoury_core::status::{RunState, StatusSnapshot};
use crossbeam_channel::select;
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced, ListenerOptions};
use log::{error, info, warn};
use std::collections::HashMap;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const TICK: Duration = Duration::from_millis(20);

enum AppEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Hotkey(GlobalHotKeyEvent),
    Pipe(ipc::PipeEvent),
    Tick,
}

fn main() {
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
        armoury_core::config::AppConfig::default()
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
    ipc::start(listener, pipe_tx);
    info!("pipe server ready ({})", armoury_core::PIPE_NAME);

    // ---- tray icon + menu (must be created on this thread: it hosts the message pump) ----
    let menu = Menu::new();
    let open_item = MenuItem::with_id("open", "Open dashboard", true, None);
    let pause_item = MenuItem::with_id("pause_all", "Pause all packs (coming soon)", false, None);
    let kill_item = MenuItem::with_id("killswitch", "Killswitch (coming soon)", false, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);
    if let Err(e) = menu.append_items(&[&open_item, &pause_item, &kill_item, &quit_item]) {
        warn!("failed to build tray menu: {e}");
    }
    let tray: Option<TrayIcon> = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ArmouryDaemon")
        .with_icon(icon::make_icon())
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
    let tray_rx = TrayIconEvent::receiver();
    let menu_rx = MenuEvent::receiver();
    let hotkey_rx = GlobalHotKeyEvent::receiver();

    let mut app = App {
        started: Instant::now(),
        clients: HashMap::new(),
        ui_conn: None,
        ui_child: None,
        spawning_ui: false,
        last_broadcast: Instant::now(),
    };
    info!("daemon ready");

    loop {
        pump_win32_messages();
        let ev = select! {
            recv(tray_rx) -> r => r.ok().map(AppEvent::Tray),
            recv(menu_rx) -> r => r.ok().map(AppEvent::Menu),
            recv(hotkey_rx) -> r => r.ok().map(AppEvent::Hotkey),
            recv(pipe_rx) -> r => r.ok().map(AppEvent::Pipe),
            default(TICK) => Some(AppEvent::Tick),
        };
        let Some(ev) = ev else { continue };
        match ev {
            AppEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => app.on_open(),
            AppEvent::Tray(_) => {}
            AppEvent::Menu(e) => match e.id.0.as_str() {
                "open" => app.on_open(),
                "quit" => {
                    info!("quit requested from tray menu");
                    break;
                }
                other => info!("tray menu '{other}' — not implemented yet"),
            },
            AppEvent::Hotkey(e) => {
                if e.state == HotKeyState::Pressed {
                    // Phase 4 turns this into the killswitch; for now, prove the wiring.
                    info!("Scroll Lock pressed (killswitch wiring check — Phase 4)");
                }
            }
            AppEvent::Pipe(p) => app.on_pipe(p),
            AppEvent::Tick => app.on_tick(),
        }
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

// ---------------------------------------------------------------------------

struct App {
    started: Instant,
    clients: HashMap<u64, crossbeam_channel::Sender<Response>>,
    ui_conn: Option<u64>,
    ui_child: Option<Child>,
    spawning_ui: bool,
    last_broadcast: Instant,
}

impl App {
    fn status(&self) -> StatusSnapshot {
        StatusSnapshot {
            daemon_version: armoury_core::VERSION.to_string(),
            uptime_secs: self.started.elapsed().as_secs(),
            state: RunState::Idle,
            trigger_source: None,
            cooldown_remaining_ms: None,
            paused: false,
            killswitch_active: false,
            tracked_apps_running: Vec::new(),
        }
    }

    fn on_tick(&mut self) {
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

        let ui_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("armoury-ui.exe")));
        let Some(ui_exe) = ui_exe else {
            warn!("cannot resolve daemon executable directory");
            return;
        };
        if !ui_exe.exists() {
            warn!("UI executable not found: {}", ui_exe.display());
            return;
        }
        match Command::new(&ui_exe).spawn() {
            Ok(child) => {
                info!("spawned UI (pid {})", child.id());
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
