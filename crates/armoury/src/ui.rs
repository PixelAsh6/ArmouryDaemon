//! UI mode (`--ui`): the dashboard window plus a settings viewport.
//! Undecorated (custom title bar: title, settings gear, close), auto-fitting,
//! placed at the bottom exterior corner of the primary monitor, above the taskbar.
//! A second launch of the same exe carries the daemon's pipe traffic.

use armoury_core::config::TrackedApp;
use armoury_core::protocol::{read_msg, write_msg, Request, Response};
use armoury_core::status::{PackStatus, RunState, StatusSnapshot};
use crossbeam_channel::Receiver;
use eframe::egui::{self, Color32, FontFamily, FontId, Rect, RichText, Stroke, TextStyle};
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SETTINGS_VIEWPORT: &str = "armoury-settings";
const MIN_WINDOW_WIDTH: f32 = 380.0;
const MIN_SETTINGS_WIDTH: f32 = 330.0;
/// Inner padding of every window (text must never hug the border).
const PAD: i8 = 10;
const PAD_F: f32 = PAD as f32;

enum UiEvent {
    Response(Response),
    Eof,
}

/// What a native file dialog came back with.
#[derive(Debug, Clone)]
enum Picked {
    /// An executable chosen to be a tracked app.
    TrackedApp(PathBuf),
    /// A `pack.json` chosen for installation.
    PackJson(PathBuf),
}

#[derive(Default)]
struct Shared {
    status: Option<StatusSnapshot>,
    connected: bool,
    daemon_version: String,
    monitors: usize,
    /// Outer rect (logical points) of the core window, for stacking the child windows.
    core_rect: Option<Rect>,
    settings_open: bool,
    /// 0 = settling, 1 = positioned (waiting for confirmation), 2 = shown
    settings_stage: u8,
    /// Safety valve: reveal anyway if placement cannot be confirmed in time.
    settings_deadline: Option<Instant>,
    settings_target: Option<egui::Pos2>,
    /// (dashboard rect, own size) the current position was computed for —
    /// lets the settings window follow the dashboard when it grows or moves.
    settings_anchor: Option<(Rect, egui::Vec2)>,
    /// The size we last asked each child window to take, so content that appears later
    /// (a new pack row, a longer message) can never be clipped.
    settings_size: Option<egui::Vec2>,
    pack_sizes: HashMap<String, egui::Vec2>,
    /// When each pack window was asked for, so a viewport that never reports its size
    /// still gets placed and shown instead of hanging invisibly.
    pack_first_seen: HashMap<String, Instant>,
    /// The settings window's number boxes, kept as text so they can be emptied while typing.
    cooldown_text: String,
    failsafe_text: String,
    /// Requests raised from child viewports, drained by the root App.
    outbox: Option<crossbeam_channel::Sender<Request>>,
    /// File dialogs started from a child viewport report back through this.
    pick_tx: Option<crossbeam_channel::Sender<Picked>>,
    /// Result of the last pack install/remove, shown in the settings window.
    last_pack_op: Option<(bool, String)>,
    /// Per-pack windows: ids in open order.
    open_packs: Vec<String>,
    /// Per-pack window placement state: (stage 0 settle / 1 positioned / 2 shown, target).
    pack_windows: HashMap<String, (u8, Option<egui::Pos2>)>,
}

pub fn run() {
    if let Err(e) = armoury_core::logging::init("ui") {
        eprintln!("logging init failed: {e}");
    }
    log::info!("ui starting (v{})", armoury_core::VERSION);

    // ---- connect to the daemon (retry briefly: the daemon just spawned us) ----
    let mut stream = None;
    for attempt in 0..50 {
        let Ok(name) = armoury_core::PIPE_NAME.to_ns_name::<GenericNamespaced>() else {
            break;
        };
        match ConnectOptions::new().name(name).connect_sync() {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                if attempt == 49 {
                    log::error!("could not connect to daemon: {e}");
                    std::process::exit(1);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let Some(stream) = stream else {
        log::error!("invalid pipe name");
        std::process::exit(1);
    };

    let (recv_half, mut send_half) = stream.split();
    if let Err(e) = write_msg(&mut send_half, &Request::UiHello { pid: std::process::id() }) {
        log::error!("hello failed: {e}");
        std::process::exit(1);
    }
    log::info!("connected to daemon");

    let (pick_tx, pick_rx) = crossbeam_channel::unbounded::<Picked>();
    // requests raised from child viewports (settings, pack windows) come back through here
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<Request>();

    let (tx, rx) = crossbeam_channel::unbounded::<UiEvent>();
    thread::spawn(move || {
        let mut reader = BufReader::new(recv_half);
        let mut line = String::new();
        loop {
            match read_msg::<_, Response>(&mut reader, &mut line) {
                Ok(Some(resp)) => {
                    if tx.send(UiEvent::Response(resp)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(UiEvent::Eof);
                    break;
                }
                Err(e) => {
                    log::warn!("pipe read error: {e}");
                    let _ = tx.send(UiEvent::Eof);
                    break;
                }
            }
        }
    });

    let shared = Arc::new(Mutex::new(Shared {
        monitors: monitor_count(),
        outbox: Some(req_tx),
        pick_tx: Some(pick_tx.clone()),
        ..Default::default()
    }));

    let (icon_rgba, icon_w, icon_h) = armoury_core::icon::rgba_icon(32);

    // Geometry is computed before the window exists: the very first frame is already in
    // the final spot, because eframe reveals the window right after its first frame.
    let (initial_size, initial_pos) = initial_geometry();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ArmouryDaemon")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_inner_size([initial_size.x, initial_size.y])
            .with_position([initial_pos.x, initial_pos.y])
            .with_icon(egui::IconData {
                rgba: icon_rgba,
                width: icon_w,
                height: icon_h,
            }),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "ArmouryDaemon",
        options,
        Box::new(move |_cc| {
            let app: Box<dyn eframe::App> = Box::new(App::new(
                rx,
                shared,
                Box::new(send_half) as Box<dyn Write>,
                pick_tx,
                pick_rx,
                req_rx,
            ));
            Ok(app)
        }),
    );
}

struct App {
    rx: Receiver<UiEvent>,
    shared: Arc<Mutex<Shared>>,
    /// Pipe writer — used to ask the daemon to change the tracked list.
    send: Box<dyn Write>,
    /// Paths coming back from the native file dialogs.
    pick_rx: Receiver<Picked>,
    /// Requests raised by child viewports (settings / pack windows).
    req_rx: Receiver<Request>,
    /// Running apps offered in the picker (title, exe path).
    apps: Vec<(String, String)>,
    apps_refreshed: Instant,
    /// Search text for the running-app picker.
    pick_query: String,
    /// Focus the picker search box once per opening.
    pick_focus_done: bool,
    pick_tx: crossbeam_channel::Sender<Picked>,
    styled: bool,
    last_desired: Option<egui::Vec2>,
}

impl App {
    fn new(
        rx: Receiver<UiEvent>,
        shared: Arc<Mutex<Shared>>,
        send: Box<dyn Write>,
        pick_tx: crossbeam_channel::Sender<Picked>,
        pick_rx: Receiver<Picked>,
        req_rx: Receiver<Request>,
    ) -> Self {
        Self {
            rx,
            shared,
            send,
            pick_tx,
            pick_rx,
            req_rx,
            apps: Vec::new(),
            apps_refreshed: Instant::now() - Duration::from_secs(60),
            pick_query: String::new(),
            pick_focus_done: false,
            styled: false,
            last_desired: None,
        }
    }

    fn request(&mut self, req: &Request) {
        if let Err(e) = write_msg(&mut self.send, req) {
            log::warn!("failed to send request: {e}");
        }
    }

    fn add_tracked(&mut self, name: String, exe: String) {
        self.request(&Request::AddTrackedApp { name, exe });
    }

    fn remove_tracked(&mut self, exe: &str) {
        self.request(&Request::RemoveTrackedApp {
            exe: exe.to_string(),
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            self.styled = true;
            install_fonts(ctx);
            ctx.all_styles_mut(|style| {
                style
                    .text_styles
                    .insert(TextStyle::Heading, FontId::new(23.0, FontFamily::Proportional));
                style
                    .text_styles
                    .insert(TextStyle::Body, FontId::new(15.5, FontFamily::Proportional));
                style
                    .text_styles
                    .insert(TextStyle::Button, FontId::new(15.5, FontFamily::Proportional));
                style
                    .text_styles
                    .insert(TextStyle::Small, FontId::new(13.0, FontFamily::Proportional));
                style.spacing.item_spacing = egui::vec2(10.0, 7.0);
            });
        }

        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::Response(Response::HelloAck {
                    daemon_version,
                    status,
                }) => {
                    let mut s = self.shared.lock().unwrap();
                    s.daemon_version = daemon_version;
                    s.status = Some(status);
                    s.connected = true;
                }
                UiEvent::Response(Response::Status { status }) => {
                    let mut s = self.shared.lock().unwrap();
                    s.status = Some(status);
                    s.connected = true;
                }
                UiEvent::Response(Response::Focus) => {
                    log::info!("focus requested by daemon");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiEvent::Response(Response::PackOp { ok, message }) => {
                    log::info!("pack op result: {message}");
                    self.shared.lock().unwrap().last_pack_op = Some((ok, message));
                }
                UiEvent::Eof => {
                    log::info!("daemon went away — exiting");
                    std::process::exit(0);
                }
            }
        }

        // Requests raised by child viewports.
        while let Ok(req) = self.req_rx.try_recv() {
            self.request(&req);
        }

        // Native file dialog results: a tracked app to add, or a pack to install.
        while let Ok(path) = self.pick_rx.try_recv() {
            match path {
                Picked::TrackedApp(path) => {
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let exe = path.display().to_string();
                    log::info!("picked {exe}");
                    // track by exe name (the user's rule), not the window title
                    self.add_tracked(name, exe);
                }
                Picked::PackJson(path) => {
                    log::info!("installing pack from {}", path.display());
                    self.request(&Request::InstallPack {
                        source: path.display().to_string(),
                    });
                }
            }
        }

        // Keep the running-app list fresh for the picker (~2 s is plenty).
        if self.apps_refreshed.elapsed() >= Duration::from_secs(2) {
            self.apps_refreshed = Instant::now();
            self.apps = list_window_apps();
        }

        // Keep the window live: pipe updates arrive at ~1 Hz. While a child window is open the
        // cadence is faster so typing in the settings boxes never feels laggy (feedback
        // 2026-10-17: a digit took ~2 s to vanish because the child only repainted on the 1 s tick).
        let child_open = {
            let s = self.shared.lock().unwrap();
            s.settings_open || !s.open_packs.is_empty()
        };
        ctx.request_repaint_after(if child_open {
            Duration::from_millis(150)
        } else {
            Duration::from_secs(1)
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

        // Whole-body drag (bottom layer; buttons added later sit on top).
        let bg = ui.interact(
            ui.max_rect(),
            ui.scope_id().with("bg-drag"),
            egui::Sense::click_and_drag(),
        );
        if bg.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        // ---- snapshot of shared state for this frame ----
        let (status_text, status_color, rows, settings_open, tracked, running, paused, packs, idle) = {
            let s = self.shared.lock().unwrap();
            let (text, color) = match &s.status {
                Some(st) => match st.state {
                    RunState::Idle => ("Idle".to_string(), Color32::from_rgb(52, 211, 153)),
                    RunState::Triggered => (
                        match &st.trigger_source {
                            Some(src) => format!("Triggered — {src}"),
                            None => "Triggered".to_string(),
                        },
                        Color32::from_rgb(251, 191, 36),
                    ),
                    RunState::Cooldown => (
                        match st.cooldown_remaining_ms {
                            Some(ms) => format!("Cooldown — {}s left", ms / 1000),
                            None => "Cooldown".to_string(),
                        },
                        Color32::from_rgb(96, 165, 250),
                    ),
                },
                None => ("Waiting for daemon…".to_string(), Color32::GRAY),
            };
            let version_text = if s.connected {
                format!("v{}", s.daemon_version)
            } else {
                "offline".to_string()
            };
            // Always the same four rows: the window keeps one stable height from its
            // first frame (values simply appear once the daemon answers).
            let rows: Vec<(String, String)> = vec![
                ("daemon".to_string(), version_text),
                (
                    "uptime".to_string(),
                    s.status
                        .as_ref()
                        .map(|st| fmt_uptime(st.uptime_secs))
                        .unwrap_or_else(|| "—".to_string()),
                ),
                (
                    "cooldown".to_string(),
                    match &s.status {
                        Some(st) => match st.cooldown_remaining_ms {
                            Some(ms) => format!("{}s left", ms / 1000),
                            None => format!("{}s", st.cooldown_seconds),
                        },
                        None => "—".to_string(),
                    },
                ),
                ("packs".to_string(), {
                    let list: Vec<PackStatus> = s
                        .status
                        .as_ref()
                        .map(|st| st.packs.clone())
                        .unwrap_or_default();
                    format!(
                        "{} installed · {} enabled",
                        list.len(),
                        list.iter().filter(|p| p.enabled).count()
                    )
                }),
            ];
            let tracked: Vec<TrackedApp> = s
                .status
                .as_ref()
                .map(|st| st.tracked_apps.clone())
                .unwrap_or_default();
            let running: Vec<String> = s
                .status
                .as_ref()
                .map(|st| st.tracked_apps_running.clone())
                .unwrap_or_default();
            let paused = s.status.as_ref().map(|st| st.paused).unwrap_or(false);
            let packs: Vec<PackStatus> = s
                .status
                .as_ref()
                .map(|st| st.packs.clone())
                .unwrap_or_default();
            // idle: (active, monitors covered, output devices muted)
            let idle = s
                .status
                .as_ref()
                .map(|st| (st.idle_active, st.idle_monitors, st.idle_devices))
                .unwrap_or((false, 0, 0));
            (
                text,
                color,
                rows,
                s.settings_open,
                tracked,
                running,
                paused,
                packs,
                idle,
            )
        };

        // ---- window body (padded so text never hugs the border) ----
        let frame = egui::Frame::NONE.inner_margin(egui::Margin::same(PAD));
        frame.show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

            // custom title bar
            ui.horizontal(|ui| {
                ui.label(RichText::new("ArmouryDaemon").strong().size(17.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, 22.0, draw_close).clicked() {
                        // Core X: close every window and unload the GUI.
                        self.shared.lock().unwrap().settings_open = false;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.add_space(6.0);
                    let label = if paused { "Start" } else { "Pause" };
                    if ui
                        .button(label)
                        .on_hover_text("Pause / resume all pack switching")
                        .clicked()
                    {
                        self.request(&Request::SetPaused { paused: !paused });
                    }
                    ui.add_space(6.0);
                    if icon_button(ui, 22.0, draw_gear).clicked() {
                        let mut s = self.shared.lock().unwrap();
                        s.settings_open = !settings_open;
                        if s.settings_open {
                            // a fresh window: forget the previous install/remove message and
                            // seed the number boxes from what the daemon reports right now
                            s.last_pack_op = None;
                            if let Some(st) = &s.status {
                                s.cooldown_text = st.cooldown_seconds.to_string();
                                s.failsafe_text = st.idle_failsafe_minutes.to_string();
                            }
                            s.settings_stage = 0;
                            s.settings_deadline = Some(Instant::now() + Duration::from_secs(6));
                            s.settings_anchor = None;
                        }
                    }
                });
            });
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(8.0);

            // content
            let (ks_active, ks_count) = {
                let s = self.shared.lock().unwrap();
                s.status
                    .as_ref()
                    .map(|st| (st.killswitch_active, st.killswitch_count))
                    .unwrap_or((false, 0))
            };
            ui.horizontal(|ui| {
                if paused {
                    status_dot(ui, Color32::from_gray(150));
                    ui.label(RichText::new(format!("Paused — {status_text}")).strong().size(17.0));
                } else if ks_active {
                    status_dot(ui, Color32::from_rgb(239, 68, 68));
                    ui.label(
                        RichText::new(format!("Killswitch — {ks_count} app(s) hidden + muted"))
                            .strong()
                            .size(17.0),
                    );
                } else if idle.0 {
                    status_dot(ui, Color32::from_rgb(147, 197, 253));
                    ui.label(RichText::new("Idle mode").strong().size(17.0));
                } else {
                    status_dot(ui, status_color);
                    ui.label(RichText::new(&status_text).strong().size(17.0));
                }
            });
            ui.add_space(8.0);
            for (key, value) in &rows {
                row(ui, key, value);
            }

            // ---- idle (in the body, per the user's choice) ----
            ui.add_space(8.0);
            let mut idle_clicked = false;
            ui.horizontal(|ui| {
                let label = if idle.0 { "Wake up" } else { "Idle mode" };
                idle_clicked = ui
                    .button(label)
                    .on_hover_text(
                        "Black out every screen and mute all audio. Any input brings it back.\n\
                         While idle, pack switching is cancelled (tracked apps are normal apps).",
                    )
                    .clicked();
                if idle.0 {
                    ui.small(format!(
                        "{} screen(s) black · {} device(s) muted",
                        idle.1, idle.2
                    ));
                }
            });
            if idle_clicked {
                self.request(&Request::SetIdle { active: !idle.0 });
            }

            // ---- tracked apps ----
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new("Tracked apps").strong());
            ui.add_space(4.0);

            let mut pick: Option<(String, String)> = None;
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("pick-app")
                    .selected_text("Pick a running app…")
                    .width(215.0)
                    .show_ui(ui, |ui| {
                        let search = ui.add(
                            egui::TextEdit::singleline(&mut self.pick_query)
                                .hint_text("type to search…")
                                .desired_width(200.0),
                        );
                        // Typing should work the moment the dropdown opens.
                        if !search.has_focus() && !self.pick_focus_done {
                            search.request_focus();
                            self.pick_focus_done = true;
                        }
                        let query = self.pick_query.trim().to_ascii_lowercase();
                        let mut matches: Vec<(String, String, String)> = Vec::new();
                        for (title, exe) in &self.apps {
                            if tracked.iter().any(|a| a.exe.eq_ignore_ascii_case(exe)) {
                                continue;
                            }
                            let name = file_name(exe);
                            if query.is_empty()
                                || name.to_ascii_lowercase().contains(&query)
                                || title.to_ascii_lowercase().contains(&query)
                            {
                                matches.push((name, exe.clone(), title.clone()));
                            }
                        }
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        for (name, exe, title) in &matches {
                            let label = format!("{name}  —  {title}");
                            if ui.selectable_label(false, label).clicked() {
                                pick = Some((name.clone(), exe.clone()));
                            }
                        }
                        if matches.is_empty() {
                            ui.label("(no match)");
                        } else if matches.len() == 1 && enter {
                            // half-typed name + a single candidate → Enter selects it
                            pick = Some((matches[0].0.clone(), matches[0].1.clone()));
                        }
                    });
                if pick.is_some() {
                    self.pick_query.clear();
                    self.pick_focus_done = false;
                }
                if ui.button("Browse…").clicked() {
                    spawn_picker(self.pick_tx.clone(), Picker::TrackedApp);
                }
            });
            if let Some((name, exe)) = pick {
                self.add_tracked(name, exe);
            }

            if tracked.is_empty() {
                ui.add_space(4.0);
                ui.small("No tracked apps yet — pick one above or browse for an .exe.");
            }

            let mut remove: Option<String> = None;
            for app in &tracked {
                ui.horizontal(|ui| {
                    let shown = file_name(&app.exe);
                    let is_running = running.iter().any(|n| n.eq_ignore_ascii_case(&shown));
                    status_dot(
                        ui,
                        if is_running {
                            Color32::from_rgb(52, 211, 153)
                        } else {
                            Color32::from_gray(110)
                        },
                    );
                    ui.label(&shown);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, 18.0, draw_close).clicked() {
                            remove = Some(app.exe.clone());
                        }
                    });
                });
            }
            if let Some(exe) = remove {
                self.remove_tracked(&exe);
            }

            // ---- packs ----
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new("Packs").strong());
            ui.add_space(4.0);
            if packs.is_empty() {
                ui.small("No packs found (looked in <app>/packs).");
            }
            let mut set_enabled: Option<(String, bool)> = None;
            let mut set_value: Option<(String, String, String)> = None;
            for pack in &packs {
                ui.horizontal(|ui| {
                    if ui
                        .small_button("‹")
                        .on_hover_text("Open this pack's own window")
                        .clicked()
                    {
                        let mut s = self.shared.lock().unwrap();
                        if !s.open_packs.contains(&pack.id) {
                            s.open_packs.push(pack.id.clone());
                            s.pack_windows.remove(&pack.id);
                            s.pack_first_seen.insert(pack.id.clone(), Instant::now());
                            log::info!("pack window {} requested", pack.id);
                        }
                    }
                    let mut enabled = pack.enabled;
                    if ui
                        .checkbox(&mut enabled, "")
                        .on_hover_text("Enable this pack")
                        .changed()
                    {
                        set_enabled = Some((pack.id.clone(), enabled));
                    }
                    ui.label(&pack.name);
                    if pack.applied {
                        ui.small("· applied");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(key) = &pack.setting_key {
                            let current = pack.value.clone().unwrap_or_default();
                            egui::ComboBox::from_id_salt(format!("pack-{}", pack.id))
                                .selected_text(current.clone())
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for value in &pack.values {
                                        if ui.selectable_label(value == &current, value).clicked() {
                                            set_value =
                                                Some((pack.id.clone(), key.clone(), value.clone()));
                                        }
                                    }
                                });
                        }
                    });
                });
            }
            if let Some((id, enabled)) = set_enabled {
                self.request(&Request::SetPackEnabled { id, enabled });
            }
            if let Some((id, key, value)) = set_value {
                self.request(&Request::SetPackValue { id, key, value });
            }
        });

        // ---- auto-fit + placement ----
        // eframe reveals the window right after its first frame, so the size AND the
        // position are both applied during that frame: it is never seen elsewhere.
        let title_font = FontId::new(17.0, FontFamily::Proportional);
        let status_font = FontId::new(17.0, FontFamily::Proportional);
        let body_font = TextStyle::Body.resolve(ui.style());
        let desired = {
            let p = ui.painter();
            let tw = |f: &FontId, t: &str| p.layout_no_wrap(t.to_owned(), f.clone(), Color32::WHITE).rect.width();
            let mut w = tw(&title_font, "ArmouryDaemon") + 72.0; // room for gear + close
            w = w.max(tw(&status_font, &status_text) + 26.0);
            for (k, v) in &rows {
                w = w.max(tw(&body_font, k) + 32.0 + tw(&body_font, v));
            }
            egui::vec2(
                (w + 2.0 * PAD_F + 8.0).max(MIN_WINDOW_WIDTH),
                (ui.min_rect().height() + 4.0).max(150.0),
            )
        };

        let ctx = ui.ctx();
        let changed = self.last_desired != Some(desired);
        if changed {
            // Self-resizing: whenever the content changes size, resize and stay pinned
            // to the bottom-right corner. The settings window follows automatically,
            // because it is positioned from this window's rect (see `settings_ui`).
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(bottom_corner(
                ctx, desired,
            )));
            self.last_desired = Some(desired);
        }

        if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
            self.shared.lock().unwrap().core_rect = Some(outer);
        }

        // ---- settings viewport (deferred window above the core) ----
        if settings_open {
            let shared = self.shared.clone();
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of(SETTINGS_VIEWPORT),
                egui::ViewportBuilder::default()
                    .with_title("Settings")
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_visible(false)
                    .with_inner_size([MIN_SETTINGS_WIDTH, 300.0]),
                move |ui, _class| settings_ui(ui, &shared),
            );
        }
        // ---- per-pack windows (opened from the dashboard, stacked to the left) ----
        let open_packs: Vec<String> = { self.shared.lock().unwrap().open_packs.clone() };
        for (index, pack_id) in open_packs.iter().enumerate() {
            let shared = self.shared.clone();
            let id = pack_id.clone();
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of(format!("armoury-pack-{pack_id}")),
                egui::ViewportBuilder::default()
                    .with_title(format!("Pack: {pack_id}"))
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_visible(false)
                    .with_inner_size([MIN_SETTINGS_WIDTH, 200.0]),
                move |ui, _class| pack_window(ui, &shared, &id, index),
            );
        }
    }
}

/// Raise a request from a child viewport (drained by the root App's logic()).
fn send_request(shared: &Arc<Mutex<Shared>>, req: &Request) {
    if let Ok(s) = shared.lock() {
        if let Some(tx) = &s.outbox {
            let _ = tx.send(req.clone());
        }
    }
}

/// Bottom-aligned to the left of an anchor edge; `anchor_left` is the left edge of the leftmost
/// window we already have on screen (the dashboard when nothing else is open).
fn place_left(ctx: &egui::Context, anchor_left: f32, core: Rect, size: egui::Vec2) -> egui::Pos2 {
    let gap = 8.0;
    let x = anchor_left - gap - size.x;
    let mut y = core.bottom() - size.y;
    if let Some((left, top, _right, bottom)) = work_area_pts(ctx) {
        let margin = 6.0;
        let x = x.max(left + margin);
        y = y.max(top + margin).min(bottom - size.y - margin);
        return egui::pos2(x, y);
    }
    egui::pos2(x, y)
}

/// Actual rect (logical points) of one of our own child windows, straight from the OS.
fn window_rect_pts(ctx: &egui::Context, title: &str) -> Option<Rect> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};
    use windows::core::PCWSTR;

    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) }.ok()?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    let ppp = ctx.pixels_per_point().max(0.5);
    Some(Rect::from_min_max(
        egui::pos2(rect.left as f32 / ppp, rect.top as f32 / ppp),
        egui::pos2(rect.right as f32 / ppp, rect.bottom as f32 / ppp),
    ))
}

/// Left edge of the leftmost window already on screen among the settings window and the pack
/// windows that have been placed (stage >= 2). A new pack window is placed against this edge, so
/// it always lands next to its neighbour — even when the user has dragged one somewhere else.
/// Called with the `Shared` lock already held by the caller.
fn leftmost_edge(ctx: &egui::Context, s: &Shared, skip: &str, core: Rect) -> f32 {
    let mut anchor = core.left();
    if s.settings_open && s.settings_stage >= 2 {
        if let Some(rect) = window_rect_pts(ctx, "Settings") {
            anchor = anchor.min(rect.left());
        }
    }
    for (other, (stage, _)) in s.pack_windows.iter() {
        if other == skip || *stage < 2 {
            continue;
        }
        if let Some(rect) = window_rect_pts(ctx, &format!("Pack: {other}")) {
            anchor = anchor.min(rect.left());
        }
    }
    anchor
}

/// One pack's own window: enable toggle + its single dropdown.
fn pack_window(ui: &mut egui::Ui, shared: &Arc<Mutex<Shared>>, id: &str, _index: usize) {
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
    let bg = ui.interact(
        ui.max_rect(),
        ui.scope_id().with(("pack-drag", id)),
        egui::Sense::click_and_drag(),
    );
    if bg.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    let pack = {
        let s = shared.lock().unwrap();
        s.status
            .as_ref()
            .and_then(|st| st.packs.iter().find(|p| p.id == id))
            .cloned()
    };
    let Some(pack) = pack else {
        // the pack disappeared (uninstalled): close the window
        let mut s = shared.lock().unwrap();
        s.open_packs.retain(|p| p != id);
        s.pack_windows.remove(id);
        return;
    };

    let frame = egui::Frame::NONE.inner_margin(egui::Margin::same(PAD));
    let mut close = false;
    frame.show(ui, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&pack.name).strong().size(16.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, 20.0, draw_close).clicked() {
                    close = true;
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let mut enabled = pack.enabled;
            if ui.checkbox(&mut enabled, "Enabled").changed() {
                send_request(
                    shared,
                    &Request::SetPackEnabled {
                        id: pack.id.clone(),
                        enabled,
                    },
                );
            }
            if pack.applied {
                ui.small("· applied");
            }
        });

        ui.add_space(6.0);
        if let Some(key) = &pack.setting_key {
            if let Some(label) = &pack.setting_label {
                ui.small(label);
            }
            let current = pack.value.clone().unwrap_or_default();
            egui::ComboBox::from_id_salt(format!("pack-win-{}", pack.id))
                .selected_text(current.clone())
                .width(190.0)
                .show_ui(ui, |ui| {
                    for value in &pack.values {
                        if ui.selectable_label(value == &current, value).clicked() {
                            send_request(
                                shared,
                                &Request::SetPackValue {
                                    id: pack.id.clone(),
                                    key: key.clone(),
                                    value: value.clone(),
                                },
                            );
                        }
                    }
                });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.small(if pack.applied {
                "Applied right now."
            } else {
                "Applies when a tracked app starts."
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !pack.version.is_empty() {
                    ui.small(RichText::new(format!("v{}", pack.version)).weak());
                }
            });
        });
    });

    if close {
        let mut s = shared.lock().unwrap();
        s.open_packs.retain(|p| p != id);
        s.pack_windows.remove(id);
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
        return;
    }

    // auto-fit from intrinsic text measurements (never from min_rect: a right-to-left row
    // stretches min_rect to the available width, which would grow the window every frame).
    let title_font = FontId::new(16.0, FontFamily::Proportional);
    let body_font = TextStyle::Body.resolve(ui.style());
    let desired = {
        let p = ui.painter();
        let tw = |f: &FontId, t: &str| p.layout_no_wrap(t.to_owned(), f.clone(), Color32::WHITE).rect.width();
        let mut w = tw(&title_font, &pack.name) + 60.0; // title + close button
        w = w.max(tw(&body_font, "Applies when a tracked app starts.") + 24.0);
        if let Some(label) = &pack.setting_label {
            w = w.max(tw(&body_font, label) + 24.0);
        }
        // the dropdown is 190 wide, plus its current text and the row padding
        w = w.max(
            tw(&body_font, pack.value.as_deref().unwrap_or("")) + 190.0 + 60.0,
        );
        egui::vec2(
            (w + 2.0 * PAD_F + 8.0).max(MIN_SETTINGS_WIDTH),
            (ui.min_rect().height() + 4.0).max(160.0),
        )
    };
    let ctx = ui.ctx();
    let mut s = shared.lock().unwrap();
    let stored = s.pack_windows.get(id).map(|(st, _)| *st).unwrap_or(0);
    let size_changed = s
        .pack_sizes
        .get(id)
        .map_or(true, |last| (*last - desired).length() > 0.5);
    match stored {
        0 => {
            let current = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            let settled = current.map_or(false, |c| (c - desired).length() <= 0.5);
            // A viewport that never reports a size must not hang the window forever.
            let waited = s
                .pack_first_seen
                .get(id)
                .map(|t| t.elapsed())
                .unwrap_or_default();
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
            s.pack_sizes.insert(id.to_string(), desired);
            if settled || waited > Duration::from_secs(2) {
                log::info!(
                    "pack window {id}: size settled at {:.0}x{:.0} (reported={settled})",
                    desired.x,
                    desired.y
                );
                s.pack_windows.insert(id.to_string(), (1, None));
            }
        }
        1 => {
            if let Some(core) = s.core_rect {
                // Anchor on the leftmost window already on screen (Settings included), so a new
                // pack window always lands next to its neighbour, never over it and never with a
                // gap where a dragged window used to be.
                let anchor = leftmost_edge(ctx, &s, id, core);
                let pos = place_left(ctx, anchor, core, desired);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                s.pack_windows.insert(id.to_string(), (2, Some(pos)));
                log::info!("pack window {id}: placed at {:.0},{:.0}", pos.x, pos.y);
            }
        }
        2 => {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            s.pack_windows.insert(id.to_string(), (3, None));
            log::info!("pack window {id}: shown");
        }
        _ => {
            // Up and running: follow the content if it grows, and re-pin when that pushes the
            // window off the work area (the same rule the settings window uses).
            if size_changed {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
                s.pack_sizes.insert(id.to_string(), desired);
                log::info!("pack window {id}: resized to {:.0}x{:.0}", desired.x, desired.y);
                let target = s.pack_windows.get(id).and_then(|(_, p)| *p);
                let off_screen = match (target, work_area_pts(ctx)) {
                    (Some(p), Some((left, top, right, bottom))) => {
                        p.x < left
                            || p.y < top
                            || p.x + desired.x > right
                            || p.y + desired.y > bottom
                    }
                    _ => true,
                };
                if off_screen {
                    if let Some(core) = s.core_rect {
                        let anchor = leftmost_edge(ctx, &s, id, core);
                        let pos = place_left(ctx, anchor, core, desired);
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                        s.pack_windows.insert(id.to_string(), (3, Some(pos)));
                    }
                }
            }
        }
    }
}

fn settings_ui(ui: &mut egui::Ui, shared: &Arc<Mutex<Shared>>) {
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

    let bg = ui.interact(
        ui.max_rect(),
        ui.scope_id().with("settings-drag"),
        egui::Sense::click_and_drag(),
    );
    if bg.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    let (status, connected, version, monitors, cooldown) = {
        let s = shared.lock().unwrap();
        let st = s.status.as_ref().map(|st| {
            if st.idle_active {
                // idle cancels switching: the engine state would still say "Triggered" for a
                // running tracked app, which is misleading while the packs are deliberately off.
                return "Idle mode (packs cancelled)".to_string();
            }
            match st.state {
                RunState::Idle => "Idle".to_string(),
                RunState::Triggered => format!(
                    "Triggered ({})",
                    st.trigger_source.as_deref().unwrap_or("?")
                ),
                RunState::Cooldown => format!(
                    "Cooldown ({}s left)",
                    st.cooldown_remaining_ms.unwrap_or(0) / 1000
                ),
            }
        });
        (
            st,
            s.connected,
            s.daemon_version.clone(),
            s.monitors,
            s.status.as_ref().map(|st| st.cooldown_seconds),
        )
    };

    // ---- rows data ----
    let mut rows: Vec<(String, String)> = Vec::new();
    rows.push((
        "state".to_string(),
        status.unwrap_or_else(|| "—".to_string()),
    ));
    rows.push((
        "daemon".to_string(),
        if connected {
            format!("v{version}")
        } else {
            "offline".to_string()
        },
    ));
    rows.push((
        "cooldown".to_string(),
        cooldown.map(|c| format!("{c}s")).unwrap_or_else(|| "—".to_string()),
    ));
    rows.push(("monitors".to_string(), monitors.to_string()));
    let config_dir = armoury_core::config::config_dir();

    // ---- window body (padded so text never hugs the border) ----
    let frame = egui::Frame::NONE.inner_margin(egui::Margin::same(PAD));
    frame.show(ui, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

        // custom title bar
        ui.horizontal(|ui| {
            ui.label(RichText::new("Settings").strong().size(16.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, 20.0, draw_close).clicked() {
                    shared.lock().unwrap().settings_open = false;
                    // The root viewport owns the decision to keep this window alive.
                    ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(8.0);

        for (key, value) in &rows {
            row(ui, key, value);
        }

        // ---- timing: revert cooldown + idle failsafe ----
        let (cooldown, fs_enabled, fs_minutes, mut cd_text, mut fs_text) = {
            let s = shared.lock().unwrap();
            let st = s.status.as_ref();
            (
                st.map(|x| x.cooldown_seconds).unwrap_or(60),
                st.map(|x| x.idle_failsafe_enabled).unwrap_or(true),
                st.map(|x| x.idle_failsafe_minutes).unwrap_or(180),
                s.cooldown_text.clone(),
                s.failsafe_text.clone(),
            )
        };
        let (packs, last_op) = {
            let s = shared.lock().unwrap();
            let packs = s
                .status
                .as_ref()
                .map(|st| st.packs.clone())
                .unwrap_or_default();
            (packs, s.last_pack_op.clone())
        };
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        // Plain typable boxes (a DragValue refuses to be emptied, which made editing them
        // painful — 2026-10-17 feedback). They commit on Enter or when focus moves away.
        let mut cd_commit: Option<u64> = None;
        let mut fs_commit: Option<u64> = None;
        ui.horizontal(|ui| {
            ui.label("Revert after");
            let resp = ui.add_sized(
                [46.0, 20.0],
                egui::TextEdit::singleline(&mut cd_text).char_limit(5),
            );
            ui.label("s");
            if resp.lost_focus() {
                match cd_text.trim().parse::<u64>() {
                    Ok(v) => {
                        let v = v.clamp(5, 3600);
                        cd_text = v.to_string();
                        cd_commit = Some(v);
                    }
                    Err(_) => cd_text = cooldown.to_string(), // put the old value back
                }
            }
        });
        let (mut fs_on, fs_min) = (fs_enabled, fs_minutes);
        let mut fs_changed = false;
        if ui
            .checkbox(&mut fs_on, "Idle failsafe")
            .on_hover_text("Leave idle mode automatically after the time below")
            .changed()
        {
            fs_changed = true;
        }
        ui.horizontal(|ui| {
            ui.label("Leave idle after");
            let resp = ui.add_sized(
                [46.0, 20.0],
                egui::TextEdit::singleline(&mut fs_text).char_limit(4),
            );
            ui.label("min");
            if resp.lost_focus() {
                match fs_text.trim().parse::<u64>() {
                    Ok(v) => {
                        let v = v.clamp(1, 1440);
                        fs_text = v.to_string();
                        fs_commit = Some(v);
                    }
                    Err(_) => fs_text = fs_minutes.to_string(),
                }
            }
        });
        {
            // keep the in-progress text so a repaint (or another window taking focus) keeps it
            let mut s = shared.lock().unwrap();
            s.cooldown_text = cd_text.clone();
            s.failsafe_text = fs_text.clone();
        }
        if let Some(seconds) = cd_commit {
            send_request(shared, &Request::SetCooldown { seconds });
            ui.ctx().request_repaint();
        }
        let minutes_now = fs_commit
            .or_else(|| fs_text.trim().parse::<u64>().ok())
            .unwrap_or(fs_min);
        if fs_changed || fs_commit.is_some() {
            send_request(
                shared,
                &Request::SetIdleFailsafe {
                    enabled: fs_on,
                    minutes: minutes_now,
                },
            );
            ui.ctx().request_repaint();
        }

        // ---- packs: what is installed, install more, remove ----
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        // The count is deliberate: if a row ever goes missing, this says whether the daemon or
        // the window is at fault.
        ui.label(RichText::new(format!("Packs ({})", packs.len())).strong());
        ui.add_space(4.0);

        let mut remove_pack: Option<String> = None;
        egui::Grid::new("settings-packs")
            .num_columns(2)
            .spacing(egui::vec2(10.0, 4.0))
            .show(ui, |ui| {
                for pack in &packs {
                    ui.horizontal(|ui| {
                        ui.label(&pack.name);
                        if !pack.version.is_empty() {
                            ui.small(RichText::new(format!("v{}", pack.version)).weak());
                        }
                    });
                    if ui
                        .small_button("Remove")
                        .on_hover_text("Delete this pack's folder and reload")
                        .clicked()
                    {
                        remove_pack = Some(pack.id.clone());
                    }
                    ui.end_row();
                }
            });
        if let Some(id) = remove_pack {
            send_request(shared, &Request::RemovePack { id });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .button("Install pack…")
                .on_hover_text("Pick a pack.json from disk; it is validated before it is installed")
                .clicked()
            {
                let tx = shared.lock().unwrap().pick_tx.clone();
                if let Some(tx) = tx {
                    spawn_picker(tx, Picker::PackJson);
                }
            }
            if ui.button("Open packs folder").clicked() {
                open_folder(&packs_dir());
            }
        });
        if let Some((ok, message)) = last_op {
            ui.add_space(2.0);
            ui.small(
                RichText::new(message).color(if ok {
                    Color32::from_rgb(52, 211, 153)
                } else {
                    Color32::from_rgb(248, 113, 113)
                }),
            );
        }

        // ---- logs + config ----
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .button("Reveal logs")
                .on_hover_text("Open the folder holding daemon.log and ui.log")
                .clicked()
            {
                open_folder(&config_dir.join("logs"));
            }
            if ui.button("Open config folder").clicked() {
                open_folder(&config_dir);
            }
        });
        ui.add_space(4.0);
    });

    // ---- shrink-wrap, position above the core window, then show ----
    let title_font = FontId::new(16.0, FontFamily::Proportional);
    let body_font = TextStyle::Body.resolve(ui.style());
    let desired = {
        let p = ui.painter();
        let tw = |f: &FontId, t: &str| p.layout_no_wrap(t.to_owned(), f.clone(), Color32::WHITE).rect.width();
        let mut w = tw(&title_font, "Settings") + 60.0;
        // the widest fixed strings in the body decide the width
        for text in [
            "Open packs folder",
            "Reveal logs",
            "Open config folder",
            "Leave idle after",
            "Install pack…",
            "Applies when a tracked app starts.",
        ] {
            w = w.max(tw(&body_font, text) + 24.0);
        }
        // one pack row: name + version bubble + the Remove button
        w = w.max(tw(&body_font, "Wallpaper Engine") + 32.0 + tw(&body_font, "Remove") + 24.0);
        for (k, v) in &rows {
            w = w.max(tw(&body_font, k) + 32.0 + tw(&body_font, v));
        }
        egui::vec2(
            (w + 2.0 * PAD_F + 8.0).max(MIN_SETTINGS_WIDTH),
            (ui.min_rect().height() + 4.0).max(170.0),
        )
    };

    let ctx = ui.ctx();
    let mut s = shared.lock().unwrap();
    let expired = s.settings_deadline.map_or(true, |d| Instant::now() > d);
    let size_changed = s
        .settings_size
        .map_or(true, |last| (last - desired).length() > 0.5);
    match s.settings_stage {
        0 => {
            let cur = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            let settled = cur.map_or(false, |c| (c - desired).length() <= 0.5);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
            s.settings_size = Some(desired);
            if settled || expired {
                s.settings_stage = 1;
            }
        }
        1 => {
            if let Some(core) = s.core_rect {
                let pos = above_core(ctx, core, desired);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                s.settings_target = Some(pos);
                s.settings_anchor = Some((core, desired));
                s.settings_stage = 2;
            }
        }
        2 => {
            // Same rule as the dashboard: only show once the OS reports the position.
            let ppp = ctx.pixels_per_point().max(0.5);
            let placed = match (s.settings_target, window_topleft_px("Settings")) {
                (Some(t), Some((x, y))) => {
                    let want = ((t.x * ppp).round() as i32, (t.y * ppp).round() as i32);
                    (x - want.0).abs() <= 2 && (y - want.1).abs() <= 2
                }
                _ => false,
            };
            if placed || expired {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                s.settings_stage = 3;
                log::info!("settings window shown ({:.0}x{:.0})", desired.x, desired.y);
            } else if let Some(t) = s.settings_target {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(t));
            }
        }
        _ => {
            // The window is up. It stays where the user put it, EXCEPT when the content has
            // grown: then it has to grow too, or the new rows are clipped out of sight.
            if size_changed {
                log::info!("settings window resized ({:.0}x{:.0})", desired.x, desired.y);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
                s.settings_size = Some(desired);
                let off_screen = match (s.settings_target, work_area_pts(ctx)) {
                    (Some(p), Some((left, top, right, bottom))) => {
                        p.x < left
                            || p.y < top
                            || p.x + desired.x > right
                            || p.y + desired.y > bottom
                    }
                    _ => true,
                };
                if off_screen {
                    if let Some(core) = s.core_rect {
                        let pos = above_core(ctx, core, desired);
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                        s.settings_target = Some(pos);
                    }
                }
            }
        }
    }
    drop(s);

    // keep the settings values live while open
    ctx.request_repaint_after(Duration::from_secs(1));
}

// ---------------------------------------------------------------------------
// Widgets

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

fn row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(key);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).strong());
        });
    });
}

fn icon_button(
    ui: &mut egui::Ui,
    size: f32,
    draw: fn(&egui::Painter, Rect, Color32),
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let color = if resp.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    draw(ui.painter(), rect.shrink(size * 0.2), color);
    resp
}

fn draw_gear(p: &egui::Painter, r: Rect, c: Color32) {
    let center = r.center();
    let rad = r.width().min(r.height()) / 2.0;
    let stroke = Stroke::new(1.8, c);
    for k in 0..8 {
        let a = k as f32 * std::f32::consts::TAU / 8.0;
        let d = egui::vec2(a.cos(), a.sin());
        p.line_segment([center + d * (rad * 0.55), center + d * rad], stroke);
    }
    p.circle_stroke(center, rad * 0.6, stroke);
    p.circle_filled(center, rad * 0.22, c);
}

fn draw_close(p: &egui::Painter, r: Rect, c: Color32) {
    let stroke = Stroke::new(1.8, c);
    p.line_segment([r.left_top(), r.right_bottom()], stroke);
    p.line_segment([r.right_top(), r.left_bottom()], stroke);
}

// ---------------------------------------------------------------------------
// Helpers

fn fmt_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn open_folder(path: &PathBuf) {
    // "Reveal logs" may be the first thing a user clicks: make sure the folder exists.
    let _ = std::fs::create_dir_all(path);
    if let Err(e) = std::process::Command::new("explorer").arg(path).spawn() {
        log::warn!("failed to open {}: {e}", path.display());
    }
}

/// The packs folder the daemon reads and writes: beside the executable (the install
/// folder), otherwise the repository root of a dev build. Mirrors the daemon's search order.
fn packs_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        let beside = dir.join("packs");
        if beside.is_dir() {
            return beside;
        }
        if let Some(repo) = dir.parent().and_then(|d| d.parent()).map(|d| d.join("packs")) {
            if repo.is_dir() {
                return repo;
            }
        }
        return beside;
    }
    PathBuf::from("packs")
}

/// Actual top-left of one of our own windows, straight from the OS (physical pixels).
/// egui's cached `outer_rect` is optimistic right after a move, so placement is
/// confirmed against the OS before a window is revealed.
fn window_topleft_px(title: &str) -> Option<(i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};
    use windows::core::PCWSTR;

    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) }.ok()?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some((rect.left, rect.top))
}

/// System DPI scale (primary monitor) — usable before any window exists.
fn system_scale() -> f32 {
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

/// Physical-pixel work area of the primary monitor.
fn work_area_physical_px() -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};
    let mut work: RECT = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut work as *mut _ as *mut core::ffi::c_void,
            0,
        )
    };
    if ok == 0 {
        None
    } else {
        Some((work.left, work.top, work.right, work.bottom))
    }
}

/// Size and position for the dashboard computed without a window, so the first
/// visible frame is already the final one.
fn initial_geometry() -> (egui::Vec2, egui::Pos2) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    };

    const EST_HEIGHT: f32 = 218.0; // measured height with padding and the four status rows
    let size = egui::vec2(MIN_WINDOW_WIDTH, EST_HEIGHT);

    let scale = system_scale().max(0.5);
    let (l, t, r, b) = work_area_physical_px().unwrap_or((40, 40, 1920, 1040));
    let (l, t, r, b) = (
        l as f32 / scale,
        t as f32 / scale,
        r as f32 / scale,
        b as f32 / scale,
    );

    let (vl, vw) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
        )
    };
    let vl = vl as f32 / scale;
    let vr = vl + vw as f32 / scale;
    let exterior_left = (l - vl).abs() < 1.0 && (r - vr).abs() > 1.0;

    let margin_x = ((r - l) * 0.005).max(6.0);
    let margin_y = ((b - t) * 0.005).max(6.0);
    let x = if exterior_left {
        l + margin_x
    } else {
        r - size.x - margin_x
    };
    let y = (b - size.y - margin_y).max(t + margin_y);
    (size, egui::pos2(x, y))
}

/// Use the machine's own fonts instead of egui's embedded ones: Segoe UI Variable Text
/// (Windows 11) → Segoe UI → Arial. Exactly one file is loaded (the first that exists) and
/// it also backs the monospace family, so nothing extra is kept in memory.
fn install_fonts(ctx: &egui::Context) {
    const CANDIDATES: [(&str, &str); 3] = [
        ("C:\\Windows\\Fonts\\SegUIVar.ttf", "Segoe UI Variable Text"),
        ("C:\\Windows\\Fonts\\segoeui.ttf", "Segoe UI"),
        ("C:\\Windows\\Fonts\\arial.ttf", "Arial"),
    ];

    let mut fonts = egui::FontDefinitions::default();
    let mut chosen = None;
    for (path, label) in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "ui".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            chosen = Some(label);
            break;
        }
    }

    match chosen {
        Some(label) => {
            let family = vec!["ui".to_owned()];
            fonts
                .families
                .insert(egui::FontFamily::Proportional, family.clone());
            fonts.families.insert(egui::FontFamily::Monospace, family);
            log::info!("using the system font: {label}");
        }
        None => log::warn!("no system UI font found; text may not render"),
    }

    ctx.set_fonts(fonts);
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Which native dialog to open.
#[derive(Debug, Clone, Copy)]
enum Picker {
    /// An .exe to track.
    TrackedApp,
    /// A `pack.json` to install.
    PackJson,
}

/// Run the native picker on a worker thread: the dialog runs its own modal loop, and
/// keeping it off the egui event thread avoids re-entrancy surprises.
fn spawn_picker(tx: crossbeam_channel::Sender<Picked>, kind: Picker) {
    thread::spawn(move || match pick_dialog(kind) {
        Some(path) => {
            let _ = tx.send(match kind {
                Picker::TrackedApp => Picked::TrackedApp(path),
                Picker::PackJson => Picked::PackJson(path),
            });
        }
        None => log::info!("file picker cancelled"),
    });
}

/// Modern common item dialog (`IFileDialog`) — no extra dependencies.
fn pick_dialog(kind: Picker) -> Option<PathBuf> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FileOpenDialog, IFileOpenDialog,
        SIGDN_FILESYSPATH,
    };
    use windows::core::w;

    let (name, spec, title) = match kind {
        Picker::TrackedApp => (w!("Applications"), w!("*.exe"), w!("Pick an application to track")),
        Picker::PackJson => (
            w!("ArmouryDaemon packs"),
            w!("pack.json"),
            w!("Pick a pack.json to install"),
        ),
    };

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let result = (|| -> Option<PathBuf> {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            if let Ok(opts) = dialog.GetOptions() {
                let _ = dialog.SetOptions(
                    opts | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST | FOS_FORCEFILESYSTEM,
                );
            }
            let _ = dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
                pszName: name,
                pszSpec: spec,
            }]);
            let _ = dialog.SetTitle(title);
            if dialog.Show(None).is_err() {
                return None; // cancelled
            }
            let item = dialog.GetResult().ok()?;
            let display = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = display.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(display.0 as *const core::ffi::c_void));
            path
        })();
        if hr.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// Visible top-level windows: (title, exe path), deduplicated by exe.
fn list_window_apps() -> Vec<(String, String)> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GWL_EXSTYLE, GW_OWNER, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
        GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    static APPS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    static OWN_EXE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let own_exe = OWN_EXE.get_or_init(|| {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });

    let own_exe_ptr: *const String = own_exe;

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> windows_sys::core::BOOL {
        unsafe {
            let own_exe: &String = &*(lparam as *const String);
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            if !GetWindow(hwnd, GW_OWNER).is_null() {
                return 1; // owned windows are usually dialogs/popups
            }
            if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & (WS_EX_TOOLWINDOW as isize) != 0 {
                return 1; // tool windows: tray/overlays, not apps
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return 1;
            }
            let mut buf = vec![0u16; (len + 1) as usize];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if n <= 0 {
                return 1;
            }
            buf.truncate(n as usize);
            let title = String::from_utf16_lossy(&buf);

            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return 1;
            }
            if let Some(exe) = crate::watch::process_path(pid) {
                if !own_exe.is_empty() && exe.eq_ignore_ascii_case(own_exe) {
                    return 1; // never offer our own GUI as a tracked app
                }
                if let Ok(mut apps) = APPS.lock() {
                    if !apps.iter().any(|(_, e)| e.eq_ignore_ascii_case(&exe)) {
                        apps.push((title, exe));
                    }
                }
            }
        }
        1
    }

    if let Ok(mut apps) = APPS.lock() {
        apps.clear();
    }
    unsafe {
        EnumWindows(Some(collect), own_exe_ptr as LPARAM);
    }
    let mut out = APPS.lock().map(|g| g.clone()).unwrap_or_default();
    out.sort_by_key(|(title, _)| title.to_lowercase());
    out
}

fn monitor_count() -> usize {    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
    let n = unsafe { GetSystemMetrics(SM_CMONITORS) };
    if n > 0 { n as usize } else { 1 }
}

/// Primary monitor's work area in logical points.
fn work_area_pts(ctx: &egui::Context) -> Option<(f32, f32, f32, f32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};

    let mut work: RECT = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut work as *mut _ as *mut core::ffi::c_void,
            0,
        )
    };
    if ok == 0 {
        log::warn!("SPI_GETWORKAREA failed");
        return None;
    }
    let ppp = ctx.pixels_per_point().max(0.5) as f32;
    Some((
        work.left as f32 / ppp,
        work.top as f32 / ppp,
        work.right as f32 / ppp,
        work.bottom as f32 / ppp,
    ))
}

/// Bottom corner of the primary monitor.
/// Single monitor → bottom-right. Multiple monitors → the exterior corner:
/// if the primary is the leftmost monitor, its bottom-left, otherwise bottom-right.
fn bottom_corner(ctx: &egui::Context, size: egui::Vec2) -> egui::Pos2 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    };

    let Some((left, top, right, bottom)) = work_area_pts(ctx) else {
        return egui::pos2(100.0, 100.0);
    };
    let (vl, vr) = unsafe {
        let vl = GetSystemMetrics(SM_XVIRTUALSCREEN) as f32;
        (
            vl,
            vl + GetSystemMetrics(SM_CXVIRTUALSCREEN) as f32,
        )
    };
    let ppp = ctx.pixels_per_point().max(0.5) as f32;
    let (vl, vr) = (vl / ppp, vr / ppp);

    let exterior_left = (left - vl).abs() < 1.0 && (right - vr).abs() > 1.0;

    let margin_x = ((right - left) * 0.005).max(6.0);
    let margin_y = ((bottom - top) * 0.005).max(6.0);

    let x = if exterior_left {
        left + margin_x
    } else {
        right - size.x - margin_x
    };
    let y = (bottom - size.y - margin_y).max(top + margin_y);
    egui::pos2(x, y)
}

/// Position the settings window: above the core (right edges aligned) when it fits, otherwise
/// bottom-aligned to the LEFT of the core, and only overlapping the core as a last resort.
/// It must never sit on top of the dashboard: the dashboard's body holds the Idle/Wake button.
fn above_core(ctx: &egui::Context, core: Rect, size: egui::Vec2) -> egui::Pos2 {
    let gap = 8.0;
    let margin = 6.0;
    let Some((left, top, right, bottom)) = work_area_pts(ctx) else {
        return egui::pos2(core.right() - size.x, core.top() - gap - size.y);
    };
    let x = (core.right() - size.x).max(left + margin).min(right - size.x - margin);

    let y_above = core.top() - gap - size.y;
    if y_above >= top + margin {
        return egui::pos2(x, y_above); // fits above, right edges aligned
    }
    let x_left = core.left() - gap - size.x;
    if x_left >= left + margin {
        let y = (core.bottom() - size.y).max(top + margin).min(bottom - size.y - margin);
        return egui::pos2(x_left, y); // no room above: to the left, bottom-aligned
    }
    // nowhere better: keep it inside the work area (it may overlap the core)
    egui::pos2(
        x,
        (bottom - size.y - margin).max(top + margin),
    )
}
