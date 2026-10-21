//! UI mode (`--ui`): the dashboard window plus a settings viewport.
//! Undecorated (custom title bar: title, settings gear, close), auto-fitting,
//! placed at the bottom-right corner of the primary monitor, above the taskbar.
//! A second launch of the same exe carries the daemon's pipe traffic.

use crate::catalog::{self, CatalogPack, EngineGate};
use armoury_core::config::{
    CONSOLE_OPEN_ALWAYS, CONSOLE_OPEN_NEVER, CONSOLE_OPEN_PAD, TrackedApp,
};
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
const INSTALL_PACKS_VIEWPORT: &str = "armoury-install-packs";
const MIN_WINDOW_WIDTH: f32 = 380.0;
const MIN_SETTINGS_WIDTH: f32 = 330.0;
/// The install-packs window: wide enough for a wrapped description, tall enough for a
/// handful of packs. The list scrolls when the catalog is longer.
const INSTALL_PACKS_WIDTH: f32 = 430.0;
const INSTALL_PACKS_HEIGHT: f32 = 470.0;
/// The height of the foot of the install-packs window: the rule and the "Local pack"
/// button. The list above it leaves this much room, so the button is inside the
/// window whatever the catalog holds.
const INSTALL_PACKS_FOOTER_HEIGHT: f32 = 62.0;
/// Inner padding of every window (text must never hug the border).
const PAD: i8 = 10;
const PAD_F: f32 = PAD as f32;
/// Distance from the work area's right and bottom edges when a window opens above the tray.
/// Zero: the window sits flush in the corner, right on top of the system tray. The chain's
/// windows are clamped with the same margin, so the whole chain stands on the dashboard's
/// bottom edge.
const UI_MARGIN: f32 = 0.0;
/// The gap between the windows of the chain, in physical pixels: always 5, whatever the
/// display's scale is. The chain's placement is done in physical pixels, so the gap is
/// exactly this and never four or six.
const CHAIN_GAP_PX: f32 = 5.0;
/// The chain's margin from the work area's edges, in physical pixels.
const UI_MARGIN_PX: f32 = 0.0;

/// The console launcher is a daemon-owned feature, not an installed pack: it appears in the
/// packs lists as a built-in row with one dropdown.
const CONSOLE_FEATURE: &str = "Console launcher";
/// What the SteamGridDB field shows instead of the key: a row of dots.
const SGDB_MASK: &str = "••••••••••••";
/// Auto-open modes in dropdown order: (value stored in config.json, label).
const CONSOLE_OPEN_MODES: [(&str, &str); 3] = [
    (CONSOLE_OPEN_ALWAYS, "Always open"),
    (CONSOLE_OPEN_PAD, "On pad connect"),
    (CONSOLE_OPEN_NEVER, "Never"),
];

fn console_open_label(mode: &str) -> &'static str {
    CONSOLE_OPEN_MODES
        .iter()
        .find(|(value, _)| *value == mode)
        .map(|(_, label)| *label)
        .unwrap_or("On pad connect")
}

/// The name a pack row shows: the pack's own name, or its id when it has none.
fn pack_row_name<'a>(name: &'a str, id: &'a str) -> &'a str {
    if name.is_empty() { id } else { name }
}

/// What a pack row is sorted by: the name on the row in lower case, and the id when two
/// names are the same, so the order never wobbles.
///
/// This is the order the rows are *shown* in. The order the daemon applies the packs in
/// is the pack's own `order` field and is a different thing: nothing here touches it.
fn pack_row_order<'a>(name: &'a str, id: &'a str) -> (String, &'a str) {
    (pack_row_name(name, id).to_lowercase(), id)
}

/// The installed packs in the order the dashboard and the settings window show them.
/// The list is a copy, so the daemon's order - the one it applies the packs in - stays
/// as it is.
fn sorted_packs(packs: &[PackStatus]) -> Vec<&PackStatus> {
    let mut rows: Vec<&PackStatus> = packs.iter().collect();
    rows.sort_by(|a, b| pack_row_order(&a.name, &a.id).cmp(&pack_row_order(&b.name, &b.id)));
    rows
}

/// The catalogue entries in the order the install-packs window shows them.
fn sorted_catalog(entries: &[CatalogPack]) -> Vec<&CatalogPack> {
    let mut rows: Vec<&CatalogPack> = entries.iter().collect();
    rows.sort_by(|a, b| pack_row_order(&a.name, &a.id).cmp(&pack_row_order(&b.name, &b.id)));
    rows
}

/// True when the window already has the size we asked for.
///
/// The comparison is in physical pixels, because that is the only place the two sizes can
/// be equal: the OS rounds a window's size to whole pixels, so a size in points never
/// matches exactly. 430 pt at 125% is 538 px, which is 430.4 pt again - a difference of
/// 0.4 pt per axis, and the length of that vector is longer than the half a point the
/// check used to allow. The window then never settled and waited for its six-second
/// deadline before it came up.
fn size_settled(cur: egui::Vec2, desired: egui::Vec2, ppp: f32) -> bool {
    let ppp = ppp.max(0.5);
    let have = egui::vec2((cur.x * ppp).round(), (cur.y * ppp).round());
    let want = egui::vec2((desired.x * ppp).round(), (desired.y * ppp).round());
    (have - want).length() <= 1.0
}

enum UiEvent {
    /// Boxed: a response carries a whole status snapshot, which dwarfs the other variant.
    Response(Box<Response>),
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
    /// Outer rect (logical points) of the core window, for stacking the child windows.
    core_rect: Option<Rect>,
    settings_open: bool,
    /// 0 = settling, 1 = positioned (waiting for confirmation), 2 = shown
    settings_stage: u8,
    /// Safety valve: reveal anyway if placement cannot be confirmed in time.
    settings_deadline: Option<Instant>,
    settings_target: Option<egui::Pos2>,
    /// The size we last asked each child window to take, so content that appears later
    /// (a new pack row, a longer message) can never be clipped.
    settings_size: Option<egui::Vec2>,
    /// The settings window's viewport, so another window can ask for its repaints.
    settings_viewport: Option<egui::ViewportId>,
    /// The install-packs window (the catalog list) and its placement state: the same
    /// stage machine the settings window uses.
    install_packs_open: bool,
    install_packs_stage: u8,
    install_packs_deadline: Option<Instant>,
    /// When the window was asked for, so the log can say how long it took to appear.
    install_packs_requested_at: Option<Instant>,
    /// The root has come back for the install-packs window it created. eframe only paints
    /// a window it has been asked for, and the first request went out before the window
    /// existed.
    install_packs_asked: bool,
    install_packs_target: Option<egui::Pos2>,
    install_packs_size: Option<egui::Vec2>,
    pack_sizes: HashMap<String, egui::Vec2>,
    /// When each pack window was asked for, so a viewport that never reports its size
    /// still gets placed and shown instead of hanging invisibly.
    pack_first_seen: HashMap<String, Instant>,
    /// The settings window's number boxes, kept as text so they can be emptied while typing.
    cooldown_text: String,
    failsafe_text: String,
    /// The settings window's SteamGridDB key field (kept so an in-progress edit survives repaints).
    sgdb_text: String,
    /// Requests raised from child viewports, drained by the root App.
    outbox: Option<crossbeam_channel::Sender<Request>>,
    /// File dialogs started from a child viewport report back through this.
    pick_tx: Option<crossbeam_channel::Sender<Picked>>,
    /// Result of the last pack install/remove, shown in the settings window.
    last_pack_op: Option<(bool, String)>,
    /// Result of the last console-app add/remove, shown in the settings window.
    last_console_op: Option<(bool, String)>,
    /// Catalog (Phase 8) state for the settings window: the last check's result, whether a
    /// check is in flight, the pack ids on their way, and the verified temp files waiting
    /// for the daemon's install request.
    catalog: Option<Result<Vec<CatalogPack>, String>>,
    catalog_check_requested: bool,
    catalog_checking: bool,
    /// The packs that are on their way, in the order they were asked for. The row of a pack
    /// in here says "installing…" and offers no button. There is more than one: a second
    /// pack may be asked for while the first one is still coming down.
    catalog_installing: Vec<String>,
    /// The verified pack files waiting for the daemon's install request.
    catalog_install_path: Vec<PathBuf>,
    /// Per-pack windows: ids in open order.
    open_packs: Vec<String>,
    /// Per-pack window placement state: (stage 0 settle / 1 positioned / 2 shown, target).
    pack_windows: HashMap<String, (u8, Option<egui::Pos2>)>,
    /// The child windows in chain order, right to left: the settings window and the pack
    /// windows, in the order they joined the chain.
    chain: Vec<ChainItem>,
    /// Where each chain window is expected and how far it got there: the same state machine
    /// the dashboard uses, so a window we moved ourselves is not read as dragged.
    chain_place: HashMap<ChainItem, Placement>,
}

pub fn run() {
    if let Err(e) = armoury_core::logging::init("ui") {
        eprintln!("logging init failed: {e}");
    }
    log::info!("ui starting (v{})", armoury_core::VERSION);
    if let Some(code) = crate::dpi_error() {
        // The DPI switch failed before the logger existed; now it can be said.
        log::info!("DPI awareness was not set (error {code}): the process may already be aware");
    }

    // ---- connect to the daemon (retry briefly: the daemon just spawned us) ----
    let pipe_name = armoury_core::pipe_name();
    let mut stream = None;
    for attempt in 0..50 {
        let Ok(name) = pipe_name.as_str().to_ns_name::<GenericNamespaced>() else {
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
                    if tx.send(UiEvent::Response(Box::new(resp))).is_err() {
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
        outbox: Some(req_tx),
        pick_tx: Some(pick_tx.clone()),
        settings_viewport: Some(settings_viewport_id()),
        ..Default::default()
    }));

    let (icon_rgba, icon_w, icon_h) = armoury_core::icon::rgba_icon(32);

    // The running-app list is expensive (a process-table walk plus a query per process),
    // so a worker thread keeps it fresh and the egui thread only reads the latest
    // snapshot. The worker stops when the UI closes.
    let apps: AppSlot = Arc::new(Mutex::new(None));
    let apps_worker = AppListWorker::start(&apps);

    // Geometry is computed before the window exists: the very first frame is already in
    // the final spot, because eframe reveals the window right after its first frame.
    // The dashboard always opens at the work area's bottom-right corner, above the tray.
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
                // Nothing is restored: the dashboard always opens in the corner.
                start_anchor(),
                apps,
                apps_worker,
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
    /// The running apps offered in the picker (title, exe path). The worker publishes
    /// the latest snapshot here; the egui thread only ever reads it.
    apps: AppSlot,
    /// The worker that keeps that list fresh. It stops when the UI closes.
    apps_worker: AppListWorker,
    /// Search text for the running-app picker.
    pick_query: String,
    pick_tx: crossbeam_channel::Sender<Picked>,
    styled: bool,
    last_desired: Option<egui::Vec2>,
    /// Where the dashboard's top-left is anchored. `None` = the automatic bottom corner; `Some`
    /// once the user dragged it away, so it grows downwards from there for the rest of the run.
    anchor: Option<egui::Pos2>,
    /// The position this window asked for and how far it got there. A window that sits
    /// somewhere else was moved by the user - not by our own resize, which also changes the
    /// position.
    placement: Placement,
}

impl App {
    fn new(
        rx: Receiver<UiEvent>,
        shared: Arc<Mutex<Shared>>,
        send: Box<dyn Write>,
        pick_tx: crossbeam_channel::Sender<Picked>,
        pick_rx: Receiver<Picked>,
        req_rx: Receiver<Request>,
        anchor: Option<egui::Pos2>,
        apps: AppSlot,
        apps_worker: AppListWorker,
    ) -> Self {
        Self {
            rx,
            shared,
            send,
            pick_tx,
            pick_rx,
            req_rx,
            apps,
            apps_worker,
            pick_query: String::new(),
            styled: false,
            last_desired: None,
            // `None` = the automatic bottom corner; `Some` once the user dragged it away.
            anchor,
            placement: Placement::default(),
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
                UiEvent::Response(resp) => match *resp {
                    Response::HelloAck {
                        daemon_version,
                        status,
                    } => {
                        let mut s = self.shared.lock().unwrap();
                        s.daemon_version = daemon_version;
                        s.status = Some(status);
                        s.connected = true;
                    }
                    Response::Status { status } => {
                        let mut s = self.shared.lock().unwrap();
                        s.status = Some(status);
                        s.connected = true;
                    }
                    Response::Focus => {
                        log::info!("focus requested by daemon");
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    Response::PackOp { ok, message } => {
                        log::info!("pack op result: {message}");
                        let mut s = self.shared.lock().unwrap();
                        s.last_pack_op = Some((ok, message));
                        // The daemon has answered for the pack that was on its way, so its
                        // row may offer the button again. The answers come in the order the
                        // requests went out, so the first one is the first pack.
                        if !s.catalog_installing.is_empty() {
                            s.catalog_installing.remove(0);
                        }
                    }
                    Response::ConsoleOp { ok, message } => {
                        log::info!("console op result: {message}");
                        let mut s = self.shared.lock().unwrap();
                        s.last_console_op = Some((ok, message));
                    }
                },
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

        // Catalog: a check was asked for (the install-packs window is the one that asks).
        // HTTPS runs on a worker thread, never on the UI's — the result lands in Shared
        // and the windows pick it up.
        let check = {
            let mut s = self.shared.lock().unwrap();
            std::mem::take(&mut s.catalog_check_requested)
        };
        if check {
            let shared = Arc::clone(&self.shared);
            let ctx = ctx.clone();
            // The catalogue is named before the round trip starts, so the worker has
            // nothing to do but wait for the network.
            let url = catalogue_url();
            log::info!("catalog: checking {url}");
            let started = Instant::now();
            thread::spawn(move || {
                let result = catalog::fetch(&url);
                let since_request = {
                    let mut s = shared.lock().unwrap();
                    s.catalog_checking = false;
                    s.catalog = Some(result.map(|catalog| catalog.packs));
                    s.install_packs_requested_at.map(|at| at.elapsed())
                };
                match since_request {
                    Some(total) => log::info!(
                        "catalog: fetched in {} ms ({} ms after the request)",
                        started.elapsed().as_millis(),
                        total.as_millis()
                    ),
                    None => log::info!("catalog: fetched in {} ms", started.elapsed().as_millis()),
                }
                // The rows are here now: the window that shows them has to redraw at
                // once, not at its next tick.
                request_catalog_repaint(&ctx, &shared);
            });
        }
        // Verified catalog files waiting for the daemon: installing still goes through the
        // daemon's normal request (it validates and owns the packs folder). There is more
        // than one: a second pack may be asked for while the first one is coming down.
        let ready: Vec<PathBuf> = {
            let mut s = self.shared.lock().unwrap();
            std::mem::take(&mut s.catalog_install_path)
        };
        for path in ready {
            log::info!("installing catalog pack from {}", path.display());
            self.request(&Request::InstallPack {
                source: path.display().to_string(),
            });
        }

        // The running-app list is refreshed by the worker, not here: the walk costs
        // ~13 ms and would stall the thread that draws this window.

        // Keep the window live: pipe updates arrive at ~1 Hz, so the dashboard repaints once
        // a second. The settings window needs a faster beat while it is open, so typing in
        // its boxes never feels laggy (feedback 2026-10-17: a digit took ~2 s to vanish
        // because the child only repainted on the 1 s tick). A repaint request only ever
        // covers the viewport it is made for, so the fast beat is asked for the settings
        // window by id - asking for it here would only redraw the dashboard 6.7 times a
        // second.
        ctx.request_repaint_after(Duration::from_secs(1));
        let (settings_open, settings_viewport, install_packs_open, install_packs_asked) = {
            let s = self.shared.lock().unwrap();
            (
                s.settings_open,
                s.settings_viewport,
                s.install_packs_open,
                s.install_packs_asked,
            )
        };
        if settings_open && let Some(id) = settings_viewport {
            ctx.request_repaint_after_for(Duration::from_millis(150), id);
        }
        // The install-packs window shows a download in flight: it has to redraw on its own
        // beat, not only when the daemon says something.
        if install_packs_open {
            ctx.request_repaint_after_for(Duration::from_millis(150), install_packs_viewport_id());
            if !install_packs_asked {
                // The window was created in the root's last pass, and eframe only paints a
                // window it has been asked for: the request above went out before the
                // window existed. Come back for it now, so it comes up at once.
                self.shared.lock().unwrap().install_packs_asked = true;
                ctx.request_repaint();
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // The UI is closing: the running-app worker must not outlive it.
        self.apps_worker.stop();
        // And it must be gone, not merely asked to go. The walk is short, so the wait is
        // over in milliseconds; the limit is only there so a stuck walk cannot hang the
        // shutdown (the thread then dies with the process).
        self.apps_worker.wait_stopped(Duration::from_secs(1));
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
        let (
            status_text,
            status_color,
            rows,
            settings_open,
            install_packs_open,
            tracked,
            running,
            paused,
            packs,
            idle,
            console_open,
        ) = {
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
                        // The cooldown is the settling window after a change, not a pending
                        // revert: the number is what is left to settle.
                        match st.cooldown_remaining_ms {
                            Some(ms) => format!("Cooling down — {}s", ms / 1000),
                            None => "Cooling down".to_string(),
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
            let console_open = s
                .status
                .as_ref()
                .map(|st| st.console_open.clone())
                .unwrap_or_default();
            (
                text,
                color,
                rows,
                s.settings_open,
                s.install_packs_open,
                tracked,
                running,
                paused,
                packs,
                idle,
                console_open,
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
                        close_settings(&mut self.shared.lock().unwrap());
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
                        if settings_open {
                            close_settings(&mut s);
                        } else {
                            s.settings_open = true;
                        }
                        if s.settings_open {
                            // a fresh window: forget the previous install/remove message and
                            // seed the number boxes from what the daemon reports right now
                            s.last_pack_op = None;
                            let (cooldown_text, failsafe_text) = match &s.status {
                                Some(st) => (
                                    st.cooldown_seconds.to_string(),
                                    st.idle_failsafe_minutes.to_string(),
                                ),
                                None => (s.cooldown_text.clone(), s.failsafe_text.clone()),
                            };
                            s.cooldown_text = cooldown_text;
                            s.failsafe_text = failsafe_text;
                            // The daemon never sends the key: the field starts empty and only
                            // shows a mask while a key is set.
                            s.sgdb_text.clear();
                            s.settings_stage = 0;
                            s.settings_deadline = Some(Instant::now() + Duration::from_secs(6));
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
            if paused {
                // The packs are off on purpose. Say so in a band of its own: a pause must
                // never be mistaken for a pack that does not work.
                ui.add_space(6.0);
                egui::Frame::NONE
                    .fill(Color32::from_rgb(58, 32, 24))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(251, 191, 36)))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Paused — the packs will not switch. Press Start.")
                                .strong()
                                .color(Color32::from_rgb(253, 224, 71)),
                        );
                    });
            }
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
                // Type here: matching running apps appear right below (no popup to open). A text
                // field inside a ComboBox popup lost focus when clicked, so the search lives here
                // and the results are inline (user request 2026-10-18). The "All apps…" dropdown
                // was removed on 2026-10-19 (user request): the field covers it.
                // The field takes what Browse leaves over, so the row stays inside the window
                // border (fixed widths used to overflow it).
                let spacing = ui.spacing().item_spacing.x;
                let browse_w = 76.0;
                let field_w = (ui.available_width() - browse_w - spacing).max(140.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.pick_query)
                        .hint_text("type to search running apps…")
                        .desired_width(field_w),
                );
                if ui
                    .add_sized(
                        [browse_w, ui.spacing().interact_size.y],
                        egui::Button::new("Browse…"),
                    )
                    .clicked()
                {
                    spawn_picker(self.pick_tx.clone(), Picker::TrackedApp);
                }
            });

            // Live results for what was typed. Enter (or a click) takes the single match.
            // The list is the worker's latest snapshot: reading it never blocks.
            let query = self.pick_query.trim().to_ascii_lowercase();
            if !query.is_empty() {
                let apps = read_apps(&self.apps);
                let list: &[(String, String)] = match &apps {
                    Some(list) => list.as_slice(),
                    None => &[],
                };
                let matches: Vec<(String, String, String)> = list
                    .iter()
                    .filter(|(_, exe)| !tracked.iter().any(|a| a.exe.eq_ignore_ascii_case(exe)))
                    .map(|(title, exe)| (file_name(exe), exe.clone(), title.clone()))
                    .filter(|(name, _, title)| {
                        name.to_ascii_lowercase().contains(&query)
                            || title.to_ascii_lowercase().contains(&query)
                    })
                    .collect();
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if matches.is_empty() {
                    ui.small(no_match_message(apps.is_some()));
                } else {
                    for (name, exe, title) in matches.iter().take(APP_MATCH_ROWS) {
                        let label = if title.is_empty() {
                            name.clone()
                        } else {
                            format!("{name}  —  {title}")
                        };
                        if ui.selectable_label(false, label).clicked() {
                            pick = Some((name.clone(), exe.clone()));
                        }
                    }
                    // The list only shows so many matches: say how many are left, so a
                    // short list is never read as the whole truth.
                    if let Some(note) = more_matches_note(matches.len(), APP_MATCH_ROWS) {
                        ui.small(RichText::new(note).weak());
                    }
                    if matches.len() == 1 && enter {
                        pick = Some((matches[0].0.clone(), matches[0].1.clone()));
                    }
                }
            }
            if pick.is_some() {
                self.pick_query.clear();
            }
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
            // Built-in entry: the console launcher. It sits beside the packs but the daemon owns
            // it — no enable checkbox, no pack window, nothing to install or remove.
            let mut set_console_open: Option<String> = None;
            ui.horizontal(|ui| {
                ui.add_space(22.0); // lines up with a pack row's ‹ button + checkbox
                ui.label(CONSOLE_FEATURE);
                ui.small(RichText::new("(built-in)").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let combo = egui::ComboBox::from_id_salt("builtin-console-open")
                        .selected_text(console_open_label(&console_open))
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            for (mode, label) in CONSOLE_OPEN_MODES {
                                if ui
                                    .selectable_label(mode == console_open.as_str(), label)
                                    .clicked()
                                {
                                    set_console_open = Some(mode.to_string());
                                }
                            }
                        });
                    combo.response.on_hover_text(
                        "When the launcher opens by itself. The tray always opens it.",
                    );
                });
            });
            if let Some(mode) = set_console_open {
                self.request(&Request::SetConsoleOpen { mode });
            }
            if packs.is_empty() {
                ui.small("No packs found (looked in <app>/packs).");
            }
            let mut set_enabled: Option<(String, bool)> = None;
            let mut set_value: Option<(String, String, String)> = None;
            // The rows are shown by name. The console launcher above them is a built-in
            // feature and not a pack, so it keeps its place and is not in this list.
            for pack in sorted_packs(&packs) {
                ui.horizontal(|ui| {
                    let mut open_this = ui
                        .small_button("‹")
                        .on_hover_text("Open this pack's own window (or double-click the name)")
                        .clicked();
                    let mut enabled = pack.enabled;
                    if ui
                        .checkbox(&mut enabled, "")
                        .on_hover_text("Enable this pack")
                        .changed()
                    {
                        set_enabled = Some((pack.id.clone(), enabled));
                    }
                    let name = ui.add(
                        egui::Label::new(&pack.name).sense(egui::Sense::click()),
                    );
                    if name.double_clicked() {
                        open_this = true;
                    }
                    if open_this {
                        let mut s = self.shared.lock().unwrap();
                        if !s.open_packs.contains(&pack.id) {
                            s.open_packs.push(pack.id.clone());
                            s.pack_windows.remove(&pack.id);
                            s.pack_first_seen.insert(pack.id.clone(), Instant::now());
                            log::info!("pack window {} requested", pack.id);
                        }
                    }
                    if pack.applied {
                        ui.small(APPLIED_MARKER);
                    }
                    // The row shows one setting; the pack window has them all.
                    if let Some(extra) = extra_settings(pack.settings.len()) {
                        ui.small(RichText::new(extra).weak())
                            .on_hover_text("More settings - open the pack's own window");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(setting) = pack.settings.first() {
                            let current = setting.value.clone().unwrap_or_default();
                            // The pack id belongs in the salt: two packs can have a setting
                            // with the same key, and one id would make them share a widget.
                            egui::ComboBox::from_id_salt(format!("pack-{}-{}", pack.id, setting.key))
                                .selected_text(current.clone())
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for value in &setting.values {
                                        if ui.selectable_label(value == &current, value).clicked() {
                                            set_value = Some((
                                                pack.id.clone(),
                                                setting.key.clone(),
                                                value.clone(),
                                            ));
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
        let small_font = TextStyle::Small.resolve(ui.style());
        let probe = width_probe(&status_text, &rows, &packs);
        let desired = {
            let p = ui.painter();
            let measure = |kind: FontKind, text: &str| {
                let font = match kind {
                    FontKind::Title => title_font.clone(),
                    FontKind::Status => status_font.clone(),
                    FontKind::Body => body_font.clone(),
                    FontKind::Small => small_font.clone(),
                };
                p.layout_no_wrap(text.to_owned(), font, Color32::WHITE).rect.width()
            };
            let w = probe_width(&measure, &probe);
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
            // A window the user placed grows downwards from its top-left; the automatic one stays
            // pinned to the bottom corner: the position is computed from the new size, so the
            // corner stays glued to the corner of the work area.
            let pos = self.anchor.unwrap_or_else(|| bottom_corner(ctx, desired));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            self.last_desired = Some(desired);
            // The position we just asked for: the window is somewhere else until the OS
            // applies it, and that gap is not the user moving the window.
            self.placement.request(pos);
        }

        if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
            {
                let mut s = self.shared.lock().unwrap();
                if s.core_rect != Some(outer) {
                    s.core_rect = Some(outer);
                    // The dashboard moved or resized: the chain of child windows follows it.
                    reanchor(ctx, &mut s, outer);
                }
            }
            let pos = outer.min;
            match self.anchor {
                None => {
                    // Only a move away from the position we asked for counts as the user
                    // dragging it. Our own resize moves the window too, and the OS applies it
                    // a moment later, so the window has to stay away before it counts.
                    let pinned = (outer.max - work_area(ctx).max).length() <= DRAG_SLACK;
                    match classify(&mut self.placement, pos, pinned, Instant::now()) {
                        Spot::Home | Spot::Away => {}
                        Spot::Dragged(at) => {
                            // The user dragged it away from the automatic corner: anchor it there.
                            self.anchor = Some(at);
                        }
                    }
                }
                Some(prev) => {
                    if (pos - prev).length() > 2.0 {
                        // The user moved it: anchor it there.
                        self.anchor = Some(pos);
                    }
                }
            }
        }

        // ---- settings viewport (deferred window above the core) ----
        if settings_open {
            let shared = self.shared.clone();
            ctx.show_viewport_deferred(
                settings_viewport_id(),
                egui::ViewportBuilder::default()
                    .with_title("Settings")
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_visible(false)
                    .with_inner_size([MIN_SETTINGS_WIDTH, 300.0]),
                move |ui, _class| settings_ui(ui, &shared),
            );
        }
        // ---- install packs viewport (the catalog list, to the left of the settings) ----
        if install_packs_open {
            let shared = self.shared.clone();
            ctx.show_viewport_deferred(
                install_packs_viewport_id(),
                egui::ViewportBuilder::default()
                    .with_title("Install packs")
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_visible(false)
                    .with_inner_size([INSTALL_PACKS_WIDTH, INSTALL_PACKS_HEIGHT]),
                move |ui, _class| install_packs_ui(ui, &shared),
            );
        }
        // ---- per-pack windows (opened from the dashboard, stacked to the left) ----
        let open_packs: Vec<String> = { self.shared.lock().unwrap().open_packs.clone() };
        for (index, pack_id) in open_packs.iter().enumerate() {
            let shared = self.shared.clone();
            let id = pack_id.clone();
            ctx.show_viewport_deferred(
                pack_viewport_id(pack_id),
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
    if let Ok(s) = shared.lock()
        && let Some(tx) = &s.outbox
    {
        let _ = tx.send(req.clone());
    }
}

/// Bottom-aligned to the left of an anchor edge; `anchor_left_px` is the left edge of the
/// leftmost window we already have on screen (the dashboard when nothing else is open), in
/// physical pixels. The result is in points, the way the viewport command takes it.
fn place_left_px(
    ctx: &egui::Context,
    anchor_left_px: f32,
    core_bottom_px: f32,
    size: egui::Vec2,
) -> egui::Pos2 {
    let ppp = ctx.pixels_per_point().max(0.5);
    let size_px = egui::vec2((size.x * ppp).round(), (size.y * ppp).round());
    let pos_px = match work_area_px() {
        Some(work) => place_left_px_in(work, anchor_left_px, core_bottom_px, size_px),
        None => egui::pos2(
            anchor_left_px - CHAIN_GAP_PX - size_px.x,
            core_bottom_px - size_px.y,
        ),
    };
    // The position is a whole number of physical pixels, so the trip through points is
    // exact: the OS applies `round((px / ppp) * ppp)`, and that is `px` again.
    egui::pos2(pos_px.x / ppp, pos_px.y / ppp)
}

/// The chain: the new window goes immediately to the left of `anchor_left_px`, bottom-aligned
/// to the dashboard, and is clamped to the work area so it never lands off-screen. Everything
/// is in physical pixels: the gap is exactly `CHAIN_GAP_PX` and the bottom edges coincide.
fn place_left_px_in(
    work: Rect,
    anchor_left_px: f32,
    core_bottom_px: f32,
    size_px: egui::Vec2,
) -> egui::Pos2 {
    let x = (anchor_left_px - CHAIN_GAP_PX - size_px.x).max(work.left() + UI_MARGIN_PX);
    // The bottom edge comes from the dashboard, so the whole chain stands on one line. The
    // clamp only kicks in when the window does not fit the work area.
    let y = (core_bottom_px - size_px.y)
        .max(work.top() + UI_MARGIN_PX)
        .min(work.bottom() - size_px.y - UI_MARGIN_PX);
    egui::pos2(x, y)
}

/// Clamp a window's top-left corner into the work area, in physical pixels.
fn clamp_to_work_area_px(pos: egui::Pos2, work: Rect, size: egui::Vec2) -> egui::Pos2 {
    let max_x = (work.right() - size.x).max(work.left());
    let max_y = (work.bottom() - size.y).max(work.top());
    egui::pos2(
        pos.x.clamp(work.left(), max_x),
        pos.y.clamp(work.top(), max_y),
    )
}

/// Keep a window inside the work area after it grew: the position is clamped (in physical
/// pixels, like the placement) and given back in points. `None` when it already fits.
fn clamp_into_work_area(
    ctx: &egui::Context,
    pos: egui::Pos2,
    size: egui::Vec2,
) -> Option<egui::Pos2> {
    let ppp = ctx.pixels_per_point().max(0.5);
    let work = work_area_px()?;
    let size_px = egui::vec2((size.x * ppp).round(), (size.y * ppp).round());
    let pos_px = egui::pos2((pos.x * ppp).round(), (pos.y * ppp).round());
    if pos_px.x >= work.left()
        && pos_px.y >= work.top()
        && pos_px.x + size_px.x <= work.right()
        && pos_px.y + size_px.y <= work.bottom()
    {
        return None;
    }
    let clamped = clamp_to_work_area_px(pos_px, work, size_px);
    Some(egui::pos2(clamped.x / ppp, clamped.y / ppp))
}

/// Actual rect (physical pixels) of one of our own windows, straight from the OS.
fn window_rect_px(title: &str) -> Option<Rect> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};
    use windows::core::PCWSTR;

    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) }.ok()?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(Rect::from_min_max(
        egui::pos2(rect.left as f32, rect.top as f32),
        egui::pos2(rect.right as f32, rect.bottom as f32),
    ))
}

/// The dashboard's rect in physical pixels: what the OS says, or the points converted.
fn core_rect_px(ctx: &egui::Context, core: Rect) -> Rect {
    if let Some(rect) = window_rect_px("ArmouryDaemon") {
        return rect;
    }
    let ppp = ctx.pixels_per_point().max(0.5);
    Rect::from_min_max(
        egui::pos2((core.left() * ppp).round(), (core.top() * ppp).round()),
        egui::pos2((core.right() * ppp).round(), (core.bottom() * ppp).round()),
    )
}

/// Actual rect (logical points) of one of our own child windows, straight from the OS.
fn window_rect_pts(ctx: &egui::Context, title: &str) -> Option<Rect> {
    let ppp = ctx.pixels_per_point().max(0.5);
    window_rect_px(title).map(|r| Rect::from_min_max(
        egui::pos2(r.left() / ppp, r.top() / ppp),
        egui::pos2(r.right() / ppp, r.bottom() / ppp),
    ))
}

/// Left edge of the leftmost pack window already on screen (stage >= 2), other than `skip`,
/// in physical pixels.
fn leftmost_pack_edge_px(s: &Shared, skip: Option<&str>) -> Option<f32> {
    let mut left: Option<f32> = None;
    for (id, (stage, _)) in s.pack_windows.iter() {
        if Some(id.as_str()) == skip || *stage < 2 {
            continue;
        }
        if let Some(rect) = window_rect_px(&format!("Pack: {id}")) {
            left = Some(left.map_or(rect.left(), |l| l.min(rect.left())));
        }
    }
    left
}

/// The chain: the dashboard sits in the bottom-right corner and every other window is placed to
/// its left. The anchor is the left edge of the leftmost window already on screen, so a new
/// window always lands immediately to the left of its neighbour. Physical pixels.
fn chain_anchor(core_left_px: f32, others: &[f32]) -> f32 {
    others.iter().fold(core_left_px, |a, l| a.min(*l))
}

/// One window in the chain: the settings window, the install-packs window or a pack's own
/// window.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum ChainItem {
    Settings,
    InstallPacks,
    Pack(String),
}

/// The viewport a chain window lives in.
fn chain_viewport(item: &ChainItem) -> egui::ViewportId {
    match item {
        ChainItem::Settings => settings_viewport_id(),
        ChainItem::InstallPacks => install_packs_viewport_id(),
        ChainItem::Pack(id) => pack_viewport_id(id),
    }
}

/// The viewport a pack window lives in.
fn pack_viewport_id(id: &str) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(format!("armoury-pack-{id}"))
}

/// One window in the chain, as the re-anchor sees it.
#[derive(Clone)]
struct ChainWindow {
    item: ChainItem,
    /// The window's size, in physical pixels.
    size_px: egui::Vec2,
    /// Where the window is now, in physical pixels.
    current_px: egui::Pos2,
    /// The user dragged the window: it keeps its place.
    dragged: bool,
}

/// The chain rule: every window goes immediately left of the one to its right, bottom-aligned
/// to the dashboard and clamped to the work area. A window the user dragged keeps its place
/// and still anchors the window to its left. The order of `windows` (right to left) is kept.
/// Everything is in physical pixels, so the gap is exactly `CHAIN_GAP_PX` and every bottom
/// edge is exactly the dashboard's.
fn reanchor_chain(
    work: Rect,
    core_left_px: f32,
    core_bottom_px: f32,
    windows: &[ChainWindow],
) -> Vec<(ChainItem, egui::Pos2)> {
    let mut out = Vec::with_capacity(windows.len());
    let mut edges: Vec<f32> = Vec::with_capacity(windows.len());
    for w in windows {
        let pos = if w.dragged {
            w.current_px
        } else {
            place_left_px_in(
                work,
                chain_anchor(core_left_px, &edges),
                core_bottom_px,
                w.size_px,
            )
        };
        edges.push(pos.x);
        out.push((w.item.clone(), pos));
    }
    out
}

/// The dashboard's rect changed: re-anchor the chain. Every window that still sits in its slot
/// is placed again, immediately left of the one to its right; a window the user dragged keeps
/// its place. Called with the `Shared` lock already held by the caller.
fn reanchor(ctx: &egui::Context, s: &mut Shared, core: Rect) {
    let Some(work) = work_area_px() else {
        return;
    };
    let ppp = ctx.pixels_per_point().max(0.5);
    // The dashboard's own edges, in physical pixels: the OS rounds its size, and so ours.
    let core_px = core_rect_px(ctx, core);
    let (core_left_px, core_bottom_px) = (core_px.left(), core_px.bottom());
    let now = Instant::now();
    let mut windows: Vec<ChainWindow> = Vec::new();
    for item in s.chain.clone() {
        let (size, title) = match &item {
            ChainItem::Settings => (s.settings_size, "Settings".to_string()),
            ChainItem::InstallPacks => (s.install_packs_size, "Install packs".to_string()),
            ChainItem::Pack(id) => (s.pack_sizes.get(id).copied(), format!("Pack: {id}")),
        };
        let Some(size) = size else {
            continue;
        };
        let Some(slot) = s.chain_place.get(&item).and_then(|p| p.requested) else {
            continue;
        };
        // Where the window is now, straight from the OS.
        let real = window_rect_px(&title);
        // Our own move is not a drag: `classify` tells the two apart.
        let place = s.chain_place.entry(item.clone()).or_default();
        let (dragged, current_px, size_px) = match real {
            Some(rect) => {
                // The drag test is in points, like the dashboard's.
                let pt = egui::pos2(rect.min.x / ppp, rect.min.y / ppp);
                (
                    matches!(classify(place, pt, false, now), Spot::Dragged(_)),
                    rect.min,
                    // The window is already on screen: its settled size is the real one.
                    rect.size(),
                )
            }
            None => (
                false,
                egui::pos2((slot.x * ppp).round(), (slot.y * ppp).round()),
                egui::vec2((size.x * ppp).round(), (size.y * ppp).round()),
            ),
        };
        windows.push(ChainWindow {
            item,
            size_px,
            current_px,
            dragged,
        });
    }
    for (w, (item, pos_px)) in windows
        .iter()
        .zip(reanchor_chain(work, core_left_px, core_bottom_px, &windows))
    {
        if w.dragged {
            // The user owns this window: it keeps its place and its slot stays where it is.
            continue;
        }
        // The position the OS is given, in points: a whole number of physical pixels, so
        // the round trip through points is exact.
        let pos = egui::pos2(pos_px.x / ppp, pos_px.y / ppp);
        // The window is already where the chain wants it: leave it alone, so the state that
        // tells our own move from the user's drag is not reset over and over.
        let placed = s.chain_place.get(&item).and_then(|p| p.requested);
        if placed.is_some_and(|slot| (slot - pos).length() <= 0.5) {
            continue;
        }
        // The slot follows the window, so the next re-anchor knows where it is.
        s.chain_place.entry(item.clone()).or_default().request(pos);
        match &item {
            ChainItem::Settings => s.settings_target = Some(pos),
            ChainItem::InstallPacks => s.install_packs_target = Some(pos),
            ChainItem::Pack(id) => {
                if let Some((stage, _)) = s.pack_windows.get(id).copied() {
                    s.pack_windows.insert(id.clone(), (stage, Some(pos)));
                }
            }
        }
        ctx.send_viewport_cmd_to(chain_viewport(&item), egui::ViewportCommand::OuterPosition(pos));
    }
}

/// The settings window closed: it leaves the chain.
fn close_settings(s: &mut Shared) {
    s.settings_open = false;
    s.chain.retain(|c| *c != ChainItem::Settings);
    s.chain_place.remove(&ChainItem::Settings);
}

/// The install-packs window closed: it leaves the chain.
fn close_install_packs(s: &mut Shared) {
    s.install_packs_open = false;
    s.install_packs_requested_at = None;
    s.install_packs_asked = false;
    s.chain.retain(|c| *c != ChainItem::InstallPacks);
    s.chain_place.remove(&ChainItem::InstallPacks);
}

/// Left edge of the leftmost window already on screen among the settings window, the
/// install-packs window and the pack windows that have been placed (stage >= 2), in physical
/// pixels. A new pack window is placed against this edge, so it always lands next to its
/// neighbour — even when the user has dragged one somewhere else.
/// Called with the `Shared` lock already held by the caller.
fn leftmost_edge_px(s: &Shared, skip: &str, core_left_px: f32) -> f32 {
    let mut edges: Vec<f32> = Vec::new();
    if s.settings_open && s.settings_stage >= 2
        && let Some(rect) = window_rect_px("Settings")
    {
        edges.push(rect.left());
    }
    if s.install_packs_open
        && let Some(rect) = window_rect_px("Install packs")
    {
        edges.push(rect.left());
    }
    if let Some(left) = leftmost_pack_edge_px(s, Some(skip)) {
        edges.push(left);
    }
    chain_anchor(core_left_px, &edges)
}

/// Left edge of the leftmost window the install-packs window has to clear: the settings
/// window and the pack windows, in physical pixels. The install-packs window itself is left
/// out — it is the one being placed.
/// Called with the `Shared` lock already held by the caller.
fn leftmost_edge_to_clear_install_px(s: &Shared, core_left_px: f32) -> f32 {
    let mut edges: Vec<f32> = Vec::new();
    if s.settings_open && s.settings_stage >= 2
        && let Some(rect) = window_rect_px("Settings")
    {
        edges.push(rect.left());
    }
    if let Some(left) = leftmost_pack_edge_px(s, None) {
        edges.push(left);
    }
    chain_anchor(core_left_px, &edges)
}

/// Left edge of the leftmost window the settings has to clear: the dashboard and the open pack
/// windows, in physical pixels. The settings window itself is left out — it is the one being
/// placed.
/// Called with the `Shared` lock already held by the caller.
fn leftmost_edge_to_clear_px(s: &Shared, core_left_px: f32) -> f32 {
    let edges: Vec<f32> = leftmost_pack_edge_px(s, None).into_iter().collect();
    chain_anchor(core_left_px, &edges)
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
        s.chain.retain(|c| *c != ChainItem::Pack(id.to_string()));
        s.chain_place.remove(&ChainItem::Pack(id.to_string()));
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
        for setting in &pack.settings {
            if !setting.label.is_empty() {
                ui.small(&setting.label);
            }
            let current = setting.value.clone().unwrap_or_default();
            egui::ComboBox::from_id_salt(format!("pack-win-{}-{}", pack.id, setting.key))
                .selected_text(current.clone())
                .width(190.0)
                .show_ui(ui, |ui| {
                    for value in &setting.values {
                        if ui.selectable_label(value == &current, value).clicked() {
                            send_request(
                                shared,
                                &Request::SetPackValue {
                                    id: pack.id.clone(),
                                    key: setting.key.clone(),
                                    value: value.clone(),
                                },
                            );
                        }
                    }
                });
            ui.add_space(4.0);
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
        s.chain.retain(|c| *c != ChainItem::Pack(id.to_string()));
        s.chain_place.remove(&ChainItem::Pack(id.to_string()));
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
        for setting in &pack.settings {
            w = w.max(tw(&body_font, &setting.label) + 24.0);
            // the dropdown is 190 wide, plus its current text and the row padding
            w = w.max(
                tw(&body_font, setting.value.as_deref().unwrap_or("")) + 190.0 + 60.0,
            );
        }
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
        .is_none_or(|last| (*last - desired).length() > 0.5);
    match stored {
        0 => {
            let current = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            let settled =
                current.is_some_and(|c| size_settled(c, desired, ctx.pixels_per_point()));
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
                let core_px = core_rect_px(ctx, core);
                let anchor = leftmost_edge_px(&s, id, core_px.left());
                let pos = place_left_px(ctx, anchor, core_px.bottom(), desired);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                s.pack_windows.insert(id.to_string(), (2, Some(pos)));
                // The pack joins the chain at its left end: it is the leftmost window.
                let item = ChainItem::Pack(id.to_string());
                s.chain.retain(|c| *c != item);
                s.chain.push(item.clone());
                s.chain_place.entry(item).or_default().request(pos);
                log::info!("pack window {id}: placed at {:.0},{:.0}", pos.x, pos.y);
            }
        }
        2 => {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            // The position stays in the entry: it is the window's slot in the chain.
            let pos = s.pack_windows.get(id).and_then(|(_, p)| *p);
            s.pack_windows.insert(id.to_string(), (3, pos));
            log::info!("pack window {id}: shown");
        }
        _ => {
            // Up and running: follow the content if it grows, and keep the window in its slot
            // when that pushes it off the work area (the same rule the settings window uses).
            let target = s.pack_windows.get(id).and_then(|(_, p)| *p);
            // The user may have dragged the window: the real position wins over the stored one.
            let pos = window_rect_pts(ctx, &format!("Pack: {id}"))
                .map(|r| r.min)
                .or(target);
            if pos != target {
                s.pack_windows.insert(id.to_string(), (3, pos));
            }
            if size_changed {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
                s.pack_sizes.insert(id.to_string(), desired);
                log::info!("pack window {id}: resized to {:.0}x{:.0}", desired.x, desired.y);
                // The pack window keeps its slot in the chain: re-anchoring it against the
                // current leftmost window would swap it with a window opened after it. The
                // position is only clamped into the work area, so the window stays on screen.
                let clamped = pos.and_then(|p| clamp_into_work_area(ctx, p, desired));
                if let Some(pos) = clamped {
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                    s.pack_windows.insert(id.to_string(), (3, Some(pos)));
                }
            }
        }
    }
    // Keep the window live: the daemon changes the pack state on its own schedule, so the
    // "· applied" marker, the "Applied right now." line and the version bubble have to be
    // refreshed. Without this the window only redraws when the OS sends it an input event.
    ctx.request_repaint_after(Duration::from_secs(1));
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
                    close_settings(&mut shared.lock().unwrap());
                    // The root viewport owns the decision to keep this window alive.
                    ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(6.0);

        // The state, the daemon's version, the uptime, the cooldown and the pack count are on
        // the dashboard: the settings window only holds what can be changed here. The monitor
        // count is no permanent row anywhere: the dashboard's idle band reports it while idle
        // mode is up, which is the only time it means anything.

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
        let (
            packs,
            last_op,
            last_console_op,
            catalog_state,
            catalog_checking,
            console_open,
        ) = {
            let s = shared.lock().unwrap();
            let packs = s
                .status
                .as_ref()
                .map(|st| st.packs.clone())
                .unwrap_or_default();
            let console_open = s
                .status
                .as_ref()
                .map(|st| st.console_open.clone())
                .unwrap_or_default();
            (
                packs,
                s.last_pack_op.clone(),
                s.last_console_op.clone(),
                s.catalog.clone(),
                s.catalog_checking,
                console_open,
            )
        };

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
                // Only the installed packs: the console launcher is a built-in feature, not a
                // pack, and it has its own row further down.
                for pack in sorted_packs(&packs) {
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
        // Two buttons in one row. The layout wraps, so a button that does not fit moves
        // to the next line instead of being clipped.
        ui.horizontal_wrapped(|ui| {
            // A tighter gap than the app's default: the two buttons have to share one row.
            ui.spacing_mut().item_spacing.x = 6.0;
            if ui
                .button("Install packs")
                .on_hover_text("Open the window that lists the packs from the repo")
                .clicked()
            {
                let mut s = shared.lock().unwrap();
                if !s.install_packs_open {
                    s.install_packs_open = true;
                    s.install_packs_stage = 0;
                    s.install_packs_deadline = Some(Instant::now() + Duration::from_secs(6));
                    s.install_packs_requested_at = Some(Instant::now());
                    s.install_packs_asked = false;
                    log::info!("install packs window requested");
                }
                drop(s);
                // The root viewport owns the window: it creates it and does the fetch.
                ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
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
        // One line: what the last check found in the repo. The check itself is the
        // install-packs window's job, so the line says "last check" and not "checked".
        if catalog_checking {
            ui.add_space(2.0);
            ui.small(RichText::new("Checking the packs…").weak());
        } else if let Some(catalog) = &catalog_state {
            let summary = catalog_check_summary(catalog, &packs);
            ui.add_space(2.0);
            ui.small(
                RichText::new(summary).color(match catalog {
                    Ok(_) => Color32::from_gray(150),
                    Err(_) => Color32::from_rgb(248, 113, 113),
                }),
            );
        }

        // ---- the console launcher and its pinned apps ----
        // The console launcher is a built-in daemon feature, not an installed pack: it has
        // no heading of its own, it is a row in the settings body.
        let console_apps = {
            let s = shared.lock().unwrap();
            s.status
                .as_ref()
                .map(|x| x.console_apps.clone())
                .unwrap_or_default()
        };
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        let mut set_console_open: Option<String> = None;
        ui.horizontal(|ui| {
            ui.label(CONSOLE_FEATURE);
            ui.small(RichText::new("(built-in)").weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let combo = egui::ComboBox::from_id_salt("settings-console-open")
                    .selected_text(console_open_label(&console_open))
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for (mode, label) in CONSOLE_OPEN_MODES {
                            if ui
                                .selectable_label(mode == console_open.as_str(), label)
                                .clicked()
                            {
                                set_console_open = Some(mode.to_string());
                            }
                        }
                    });
                combo.response.on_hover_text(
                    "When the launcher opens by itself. The tray always opens it.",
                );
            });
        });
        if let Some(mode) = set_console_open {
            send_request(shared, &Request::SetConsoleOpen { mode });
        }
        let mut remove_console: Option<String> = None;
        if console_apps.is_empty() {
            // Short on purpose: a long hint here would set the window's width.
            ui.small(RichText::new("No console apps yet.").weak());
        } else {
            egui::Grid::new("settings-console-apps")
                .num_columns(2)
                .spacing(egui::vec2(10.0, 4.0))
                .show(ui, |ui| {
                    for a in &console_apps {
                        ui.horizontal(|ui| {
                            ui.label(&a.name);
                            ui.small(RichText::new(file_name(&a.exe)).weak());
                        });
                        if ui
                            .small_button("Remove")
                            .on_hover_text("Remove from the launcher (tracking is unaffected)")
                            .clicked()
                        {
                            remove_console = Some(a.exe.clone());
                        }
                        ui.end_row();
                    }
                });
        }
        if let Some(exe) = remove_console {
            send_request(shared, &Request::RemoveConsoleApp { exe });
        }
        if let Some((ok, message)) = last_console_op {
            ui.add_space(2.0);
            ui.small(
                RichText::new(message).color(if ok {
                    Color32::from_rgb(52, 211, 153)
                } else {
                    Color32::from_rgb(248, 113, 113)
                }),
            );
        }

        // ---- SteamGridDB: cover/hero art for games Steam doesn't know ----
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(RichText::new("SteamGridDB").strong());
        ui.add_space(4.0);
        let (sgdb_set, sgdb_hint) = {
            let s = shared.lock().unwrap();
            (
                s.status.as_ref().is_some_and(|st| st.sgdb_api_key_set),
                sgdb_field_hint(s.status.as_ref()),
            )
        };
        let mut sgdb_text = shared.lock().unwrap().sgdb_text.clone();
        let mut save_sgdb = false;
        let mut clear_sgdb = false;
        ui.horizontal(|ui| {
            let spacing = ui.spacing().item_spacing.x;
            let save_w = 52.0;
            // The key is masked, so the field stays narrow and does not stretch the window.
            let field_w = 190.0_f32.min((ui.available_width() - save_w - spacing).max(120.0));
            // Masked: the key is never shown in the clear. The daemon does not send it
            // either, so the field only ever holds a key the user is typing right now.
            let resp = ui.add(
                egui::TextEdit::singleline(&mut sgdb_text)
                    .password(true)
                    .hint_text(sgdb_hint)
                    .desired_width(field_w),
            );
            save_sgdb = ui
                .add_sized(
                    [save_w, ui.spacing().interact_size.y],
                    egui::Button::new("Save"),
                )
                .on_hover_text("Save this key")
                .clicked();
            save_sgdb |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if sgdb_set {
                clear_sgdb = ui
                    .add_sized(
                        [save_w, ui.spacing().interact_size.y],
                        egui::Button::new("Clear"),
                    )
                    .on_hover_text("Remove the saved key")
                    .clicked();
            }
        });
        // Keep the in-progress text so a repaint (or another window taking focus) keeps it.
        shared.lock().unwrap().sgdb_text = sgdb_text.clone();
        if save_sgdb && !sgdb_text.is_empty() {
            send_request(shared, &Request::SetSgdbKey { key: sgdb_text.clone() });
            // The key is never shown again: the field goes back to the mask.
            shared.lock().unwrap().sgdb_text.clear();
            ui.ctx().request_repaint();
        } else if clear_sgdb {
            send_request(shared, &Request::SetSgdbKey { key: String::new() });
            shared.lock().unwrap().sgdb_text.clear();
            ui.ctx().request_repaint();
        }
        ui.small(RichText::new(if sgdb_set { "Key set." } else { "No key set." }).weak());

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
    let small_font = TextStyle::Small.resolve(ui.style());
    let button_font = TextStyle::Button.resolve(ui.style());
    // The core (dashboard) window publishes its rect; the settings window takes its width
    // from it, so the two are exactly as wide as each other.
    let core_width = shared.lock().unwrap().core_rect.map(|r| r.width());
    let desired = {
        let p = ui.painter();
        let tw = |f: &FontId, t: &str| p.layout_no_wrap(t.to_owned(), f.clone(), Color32::WHITE).rect.width();
        // Pack names are dynamic, so read them from the status snapshot for the width probe.
        let pack_names: Vec<String> = shared
            .lock()
            .unwrap()
            .status
            .as_ref()
            .map(|st| st.packs.iter().map(|pack| pack.name.clone()).collect())
            .unwrap_or_default();
        // The title sits beside its close button.
        let mut w = tw(&title_font, "Settings") + 24.0 + 20.0;
        // Helper lines are drawn with `ui.small(...)`: measuring them at the body font (as this
        // used to) inflated the window by ~30%. Measure each string at the font it is drawn in.
        for text in ["No console apps yet.", "No key set."] {
            w = w.max(tw(&small_font, text));
        }
        for text in ["Leave idle after", "Revert after", "Idle failsafe"] {
            w = w.max(tw(&body_font, text));
        }
        // Two buttons share a row, so the pair has to fit side by side.
        for [a, b] in [["Reveal logs", "Open config folder"]] {
            w = w.max(tw(&button_font, a) + 24.0 + tw(&button_font, b) + 24.0 + 8.0);
        }
        // The packs row: two buttons side by side.
        w = w.max(
            tw(&button_font, "Install packs")
                + 24.0
                + tw(&button_font, "Open packs folder")
                + 24.0
                + 6.0,
        );
        // The SteamGridDB row: the masked field, Save and Clear.
        w = w.max(190.0 + 52.0 + 52.0 + 24.0);
        // The built-in console row: name + "(built-in)" + its 120 pt dropdown.
        w = w.max(
            tw(&body_font, CONSOLE_FEATURE) + 10.0 + tw(&small_font, "(built-in)") + 10.0 + 120.0,
        );
        // A pack row: name + version bubble + the Remove button.
        w = w.max(tw(&body_font, "Wallpaper Engine") + 32.0 + tw(&button_font, "Remove") + 24.0);
        for pack in &pack_names {
            w = w.max(tw(&body_font, pack) + 32.0 + tw(&button_font, "Remove") + 24.0);
        }
        // The content width is only a fallback (the first frame, before the core has
        // published its width): it never makes the window wider than the core.
        let content = snug_width(w, work_area_pts(ui.ctx()).map(|(left, _, right, _)| right - left));
        egui::vec2(
            core_width.map_or(content, |cw| cw.max(MIN_SETTINGS_WIDTH)),
            (ui.min_rect().height() + 4.0).max(170.0),
        )
    };

    let ctx = ui.ctx();
    let mut s = shared.lock().unwrap();
    let expired = s.settings_deadline.is_none_or(|d| Instant::now() > d);
    let size_changed = s
        .settings_size
        .is_none_or(|last| (last - desired).length() > 0.5);
    match s.settings_stage {
        0 => {
            let cur = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            let settled =
                cur.is_some_and(|c| size_settled(c, desired, ctx.pixels_per_point()));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
            s.settings_size = Some(desired);
            if settled || expired {
                s.settings_stage = 1;
            }
        }
        1 => {
            // The settings window is too big and can't sit on top of the main window, so it
            // joins the same leftward chain as the pack windows: immediately left of the
            // current leftmost window, bottom-aligned to the dashboard. Never above it.
            let pos = match s.core_rect {
                Some(core) => {
                    let core_px = core_rect_px(ctx, core);
                    let anchor = leftmost_edge_to_clear_px(&s, core_px.left());
                    place_left_px(ctx, anchor, core_px.bottom(), desired)
                }
                // No dashboard yet (the settings is opened from the dashboard, so this is a
                // corner case): the work area's bottom-right corner.
                None => bottom_right(work_area(ctx), desired, UI_MARGIN),
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            s.settings_target = Some(pos);
            // The settings joins the chain at its left end: it clears the pack windows.
            // The chain runs right to left, so the left end is the end of the list.
            s.chain.retain(|c| *c != ChainItem::Settings);
            s.chain.push(ChainItem::Settings);
            s.chain_place
                .entry(ChainItem::Settings)
                .or_default()
                .request(pos);
            s.settings_stage = 2;
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
            let target = s.settings_target;
            // The user may have dragged the window: the real position wins over the stored one.
            let pos = window_rect_pts(ctx, "Settings").map(|r| r.min).or(target);
            if pos != target {
                s.settings_target = pos;
            }
            if size_changed {
                log::info!("settings window resized ({:.0}x{:.0})", desired.x, desired.y);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
                s.settings_size = Some(desired);
                // The settings window keeps its slot in the chain: re-anchoring it against the
                // current leftmost window would swap it with a pack window opened after it.
                // The stored position is only clamped, so the window stays on screen.
                let clamped = pos.and_then(|p| clamp_into_work_area(ctx, p, desired));
                if let Some(pos) = clamped {
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                    s.settings_target = Some(pos);
                }
            }
        }
    }
    drop(s);

    // keep the settings values live while open
    ctx.request_repaint_after(Duration::from_secs(1));
}

/// The install-packs window: every pack in the catalog, each with a download button.
/// The list is the only place the catalog shows: the settings window has no copy of it.
fn install_packs_ui(ui: &mut egui::Ui, shared: &Arc<Mutex<Shared>>) {
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

    // Whole-body drag (the bottom layer; the buttons added later sit on top).
    let bg = ui.interact(
        ui.max_rect(),
        ui.scope_id().with("install-packs-drag"),
        egui::Sense::click_and_drag(),
    );
    if bg.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    let (catalog_state, catalog_checking, catalog_installing, last_op, packs) = {
        let s = shared.lock().unwrap();
        (
            s.catalog.clone(),
            s.catalog_checking,
            s.catalog_installing.clone(),
            s.last_pack_op.clone(),
            s.status
                .as_ref()
                .map(|st| st.packs.clone())
                .unwrap_or_default(),
        )
    };

    let mut close = false;
    let mut refresh = false;
    let mut install_file = false;

    let frame = egui::Frame::NONE.inner_margin(egui::Margin::same(PAD));
    frame.show(ui, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

        // ---- title bar ----
        ui.horizontal(|ui| {
            ui.label(RichText::new("Install packs").strong().size(16.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, 20.0, draw_close).clicked() {
                    close = true;
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(6.0);

        // ---- the catalog ----
        ui.horizontal(|ui| {
            if ui
                .button("Check for updates")
                .on_hover_text("Fetch the pack catalog again")
                .clicked()
                && !catalog_checking
            {
                refresh = true;
            }
            if catalog_checking {
                ui.small(RichText::new("Checking…").weak());
            }
        });
        ui.add_space(4.0);

        // The list takes the room that is left above the foot of the window, so the
        // "Local pack" button is always inside it. The list also shrinks to the
        // height its rows need: there is no dead space above the rule, and the
        // catalog scrolls inside the list when it holds more than that.
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - INSTALL_PACKS_FOOTER_HEIGHT).max(0.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if let Some((ok, message)) = last_op {
                    ui.small(
                        RichText::new(message).color(if ok {
                            Color32::from_rgb(52, 211, 153)
                        } else {
                            Color32::from_rgb(248, 113, 113)
                        }),
                    );
                    ui.add_space(4.0);
                }
                match &catalog_state {
                    None => {
                        if catalog_checking {
                            ui.small(RichText::new("Fetching the packs from the repo…").weak());
                        } else {
                            ui.small(RichText::new("The catalog has not been fetched yet.").weak());
                        }
                    }
                    Some(Err(message)) => {
                        ui.small(RichText::new(message).color(Color32::from_rgb(248, 113, 113)));
                    }
                    Some(Ok(entries)) if entries.is_empty() => {
                        ui.small(RichText::new("The packs repo has no packs.").weak());
                    }
                    Some(Ok(entries)) => {
                        // The rows are shown by name: the catalogue's own order is the
                        // repo's, not one to read.
                        for entry in sorted_catalog(entries) {
                            catalog_row(
                                ui,
                                shared,
                                entry,
                                &packs,
                                &catalog_installing,
                            );
                        }
                    }
                }
            });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);
        if ui
            .small_button("Local pack")
            .on_hover_text("Pick a pack.json from disk; it is validated before it is installed")
            .clicked()
        {
            install_file = true;
        }
    });

    if close {
        let mut s = shared.lock().unwrap();
        close_install_packs(&mut s);
        // The root viewport owns the decision to keep this window alive.
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
        return;
    }
    if refresh {
        let mut s = shared.lock().unwrap();
        s.catalog_check_requested = true;
        s.catalog_checking = true;
        drop(s);
        // The root does the HTTPS round trip on its own thread.
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
    }
    if install_file {
        let tx = shared.lock().unwrap().pick_tx.clone();
        if let Some(tx) = tx {
            spawn_picker(tx, Picker::PackJson);
        }
    }

    // ---- place the window, then show it ----
    let ctx = ui.ctx();
    let mut s = shared.lock().unwrap();
    let expired = s.install_packs_deadline.is_none_or(|d| Instant::now() > d);
    let stage_before = s.install_packs_stage;
    match s.install_packs_stage {
        0 => {
            let desired = egui::vec2(INSTALL_PACKS_WIDTH, INSTALL_PACKS_HEIGHT);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
            s.install_packs_size = Some(desired);
            let cur = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            let settled =
                cur.is_some_and(|c| size_settled(c, desired, ctx.pixels_per_point()));
            if settled || expired {
                // The window fetches the catalog itself when it opens: the request goes to
                // the root, which does the round trip on a worker thread.
                if !s.catalog_checking {
                    s.catalog_check_requested = true;
                    s.catalog_checking = true;
                    // The root picks the request up on its next frame, and its next frame
                    // is a tick away: ask for it now, so the round trip starts now.
                    ctx.request_repaint_of(egui::ViewportId::ROOT);
                }
                s.install_packs_stage = 1;
            }
        }
        1 => {
            let desired = s
                .install_packs_size
                .unwrap_or_else(|| egui::vec2(INSTALL_PACKS_WIDTH, INSTALL_PACKS_HEIGHT));
            let pos = match s.core_rect {
                Some(core) => {
                    // The window joins the chain at its left end: it clears every window
                    // that is already on screen.
                    let core_px = core_rect_px(ctx, core);
                    let anchor = leftmost_edge_to_clear_install_px(&s, core_px.left());
                    place_left_px(ctx, anchor, core_px.bottom(), desired)
                }
                // No dashboard yet: the work area's bottom-right corner.
                None => bottom_right(work_area(ctx), desired, UI_MARGIN),
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            s.install_packs_target = Some(pos);
            // The chain runs right to left, so the left end is the end of the list.
            s.chain.retain(|c| *c != ChainItem::InstallPacks);
            s.chain.push(ChainItem::InstallPacks);
            s.chain_place
                .entry(ChainItem::InstallPacks)
                .or_default()
                .request(pos);
            s.install_packs_stage = 2;
        }
        2 => {
            // Same rule as the dashboard: only show once the OS reports the position.
            let ppp = ctx.pixels_per_point().max(0.5);
            let placed = match (s.install_packs_target, window_topleft_px("Install packs")) {
                (Some(t), Some((x, y))) => {
                    let want = ((t.x * ppp).round() as i32, (t.y * ppp).round() as i32);
                    (x - want.0).abs() <= 2 && (y - want.1).abs() <= 2
                }
                _ => false,
            };
            if placed || expired {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                s.install_packs_stage = 3;
                // How long the window took to appear, measured from the click that asked
                // for it: this is the number the "it opens slowly" complaint is about.
                match s.install_packs_requested_at {
                    Some(at) => log::info!(
                        "install packs window shown ({} ms after the request)",
                        at.elapsed().as_millis()
                    ),
                    None => log::info!("install packs window shown"),
                }
            } else if let Some(t) = s.install_packs_target {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(t));
            }
        }
        _ => {
            // The window is up. It keeps its slot in the chain; the user may have dragged
            // it, and then the real position wins over the stored one.
            let target = s.install_packs_target;
            let pos = window_rect_pts(ctx, "Install packs").map(|r| r.min).or(target);
            if pos != target {
                s.install_packs_target = pos;
            }
        }
    }
    let stage_after = s.install_packs_stage;
    drop(s);

    // The window has to be on the screen now, not at the next tick. Every step of the
    // way asks for the next one right away, so the whole way from the request to the
    // window on the screen is a couple of frames instead of a couple of seconds.
    if stage_after != stage_before {
        ctx.request_repaint();
    }

    // Keep the window live: a download in flight and the result of the last install have
    // to show up on their own.
    ctx.request_repaint_after(Duration::from_secs(1));
}

/// The width of the description column in the install-packs window. A one-sentence
/// description wraps into two or three rows in it. The column is narrower than the
/// window on purpose: a description that ran the full width of the window used to
/// widen the whole list, and that pushed the state of the rows below it off the screen.
const CATALOG_DESC_WIDTH: f32 = 240.0;

/// One pack of the catalog: its name, its version, its description and the button that
/// downloads it.
fn catalog_row(
    ui: &mut egui::Ui,
    shared: &Arc<Mutex<Shared>>,
    entry: &CatalogPack,
    packs: &[PackStatus],
    installing: &[String],
) {
    let installed = packs.iter().find(|p| p.id == entry.id);
    let installed_version = installed.map(|p| p.version.as_str());
    let name = if entry.name.is_empty() {
        &entry.id
    } else {
        &entry.name
    };
    // The name and the state share the first line. The state sits against the right
    // edge, and the description below never widens the row, so the state is in the
    // same place on every row.
    ui.horizontal(|ui| {
        ui.label(RichText::new(name.as_str()).strong());
        if !entry.version.is_empty() {
            ui.small(RichText::new(format!("v{}", entry.version)).weak());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // A pack that is on its way has no button: the row says so instead, and the
            // button comes back when the daemon has answered.
            if installing.iter().any(|id| id == &entry.id) {
                ui.small(RichText::new("installing…").weak());
                return;
            }
            // A pack this daemon is too old to run gets no button: the reason is shown
            // instead, so the user reads why it cannot be installed here.
            if let EngineGate::Refused { reason, .. } = entry.engine_gate() {
                ui.small(RichText::new(reason).weak());
                return;
            }
            match pack_button(&entry.version, installed_version) {
                PackButton::Download => {
                    if ui
                        .small_button("Download")
                        .on_hover_text("Download, verify and install this pack")
                        .clicked()
                    {
                        start_catalog_install(shared, entry, ui.ctx());
                    }
                }
                PackButton::Update => {
                    let hover = format!(
                        "installed v{} → v{}",
                        installed_version.unwrap_or_default(),
                        entry.version
                    );
                    if ui.small_button("Update").on_hover_text(hover).clicked() {
                        start_catalog_install(shared, entry, ui.ctx());
                    }
                }
                PackButton::Installed(version) => {
                    ui.small(RichText::new(format!("v{version} installed")).weak());
                }
            }
        });
    });
    if !entry.description.is_empty() {
        // The description is a column of its own, indented under the name. It wraps
        // into two or three rows instead of running off the edge of the window, and it
        // never grows wider than its column, so it cannot push the state of the rows
        // below it out of the window.
        ui.indent(("catalog-description", entry.id.as_str()), |ui| {
            let width = CATALOG_DESC_WIDTH.min(ui.available_width());
            let rect = Rect::from_min_size(
                ui.cursor().min,
                egui::vec2(width, ui.available_height().max(60.0)),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                ui.small(RichText::new(entry.description.as_str()).weak());
            });
        });
    }
    ui.add_space(6.0);
}

/// What the button on a catalog row does.
#[derive(Debug, PartialEq, Eq)]
enum PackButton {
    /// The pack is not installed: download it.
    Download,
    /// A newer version is available: update it.
    Update,
    /// The installed version is the current one: there is nothing to do.
    Installed(String),
}

/// The button a catalog row shows: the catalog's version against the installed one.
fn pack_button(catalog_version: &str, installed_version: Option<&str>) -> PackButton {
    match installed_version {
        None => PackButton::Download,
        Some(current) if catalog::is_newer(catalog_version, current) => PackButton::Update,
        Some(current) => PackButton::Installed(current.to_string()),
    }
}

/// The one line the settings panel shows after a catalog check: what the repo holds and
/// which of those packs are out of date. The check is the install-packs window's job, so
/// the line says what the last check found and never that this window did it.
///
/// A pack this daemon is too old to run is not offered: it is neither out of date nor a
/// pack this daemon could install, so it stays out of the counts below.
fn catalog_check_summary(
    catalog: &Result<Vec<CatalogPack>, String>,
    packs: &[PackStatus],
) -> String {
    let entries = match catalog {
        Err(message) => return format!("Last check failed: {message}"),
        Ok(entries) => entries,
    };
    if entries.is_empty() {
        return "Last check: the packs repo has no packs.".to_string();
    }
    let installable: Vec<&CatalogPack> = entries
        .iter()
        .filter(|entry| entry.engine_gate() == EngineGate::Supported)
        .collect();
    if installable.is_empty() {
        return format!(
            "Last check: {} packs — none can be installed by this daemon",
            entries.len()
        );
    }
    let available: Vec<String> = installable
        .iter()
        .filter(|entry| {
            packs
                .iter()
                .find(|p| p.id == entry.id)
                .is_some_and(|p| catalog::is_newer(&entry.version, &p.version))
        })
        .map(|entry| {
            let name = if entry.name.is_empty() {
                &entry.id
            } else {
                &entry.name
            };
            format!("{name} v{}", entry.version)
        })
        .collect();
    match available.len() {
        0 => format!("Last check: {} packs — all up to date", installable.len()),
        1 => format!("Last check: {} is available", available[0]),
        _ => format!(
            "Last check: {} packs are out of date: {}",
            available.len(),
            available.join(", ")
        ),
    }
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
        if let Some(repo) = dir.parent().and_then(|d| d.parent()).map(|d| d.join("packs"))
            && repo.is_dir()
        {
            return repo;
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
    const EST_HEIGHT: f32 = 218.0; // measured height with padding and the four status rows
    let size = egui::vec2(MIN_WINDOW_WIDTH, EST_HEIGHT);
    (size, bottom_right(work_area_rect(), size, UI_MARGIN))
}

/// The anchor the dashboard starts with: `None` = the automatic bottom corner above the
/// tray. Nothing is restored from the config, so the window can never come up off-screen.
fn start_anchor() -> Option<egui::Pos2> {
    None
}

/// The settings window's viewport id - the same value `show_viewport_deferred` registers
/// it under, so another window can ask for its repaints.
fn settings_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of(SETTINGS_VIEWPORT)
}

/// The install-packs window's viewport id, the same way as the settings window's.
fn install_packs_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of(INSTALL_PACKS_VIEWPORT)
}

/// The windows that show what the catalogue holds: the install-packs window with its rows
/// and the settings window with its one-line summary. They have to redraw as soon as the
/// catalogue lands - their next tick is a second away, and the rows are already here.
fn request_catalog_repaint(ctx: &egui::Context, shared: &Arc<Mutex<Shared>>) {
    let (install_open, settings_open) = {
        let s = shared.lock().unwrap();
        (s.install_packs_open, s.settings_open)
    };
    if install_open {
        ctx.request_repaint_of(install_packs_viewport_id());
    }
    if settings_open {
        ctx.request_repaint_of(settings_viewport_id());
    }
}

/// The badge on a dashboard pack row that says how many settings the pack has that the row
/// does not show: one setting → no badge, two settings → "+1".
fn extra_settings(count: usize) -> Option<String> {
    (count > 1).then(|| format!("+{}", count - 1))
}

/// What the SteamGridDB field shows: the key never leaves the daemon, so a key that is set
/// shows as a row of dots and no key shows a prompt for one. The key in the snapshot is
/// never read: the field must not be able to show the key back.
fn sgdb_field_hint(status: Option<&StatusSnapshot>) -> &'static str {
    if status.is_some_and(|st| st.sgdb_api_key_set) {
        SGDB_MASK
    } else {
        "paste your API key…"
    }
}

/// Use the machine's own fonts instead of egui's embedded ones: Segoe UI Variable Text
/// (Windows 11) → Segoe UI → Arial. Exactly one file is loaded (the first that exists) and
/// it also backs the monospace family, so nothing extra is kept in memory.
pub(crate) fn install_fonts(ctx: &egui::Context) {
    let (fonts, chosen) = system_fonts();
    match chosen {
        Some(label) => log::info!("using the system font: {label}"),
        None => log::warn!("no system UI font found; text may not render"),
    }
    ctx.set_fonts(fonts);
}

/// The fonts the app draws with, and the name of the one that was found (None when the
/// machine has none of them and egui's own font is used).
fn system_fonts() -> (egui::FontDefinitions, Option<&'static str>) {
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

    if chosen.is_some() {
        let family = vec!["ui".to_owned()];
        fonts
            .families
            .insert(egui::FontFamily::Proportional, family.clone());
        fonts.families.insert(egui::FontFamily::Monospace, family);
    }

    (fonts, chosen)
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

/// Download one catalog pack on its own thread, then hand the file to the daemon: the App
/// drains `catalog_install_path` and sends the normal install request (the daemon still
/// verifies and owns the packs folder).
///
/// The download must not run on the thread that draws the window: this returns at once and
/// the row says the pack is on its way, so the window keeps drawing while the pack comes
/// down. `ctx` is the window's context, so the result can be shown at once and not at the
/// window's next tick.
fn start_catalog_install(shared: &Arc<Mutex<Shared>>, entry: &CatalogPack, ctx: &egui::Context) {
    // The catalogue is named before the download starts, so the worker has nothing to do
    // but wait for the network.
    let url = catalogue_url();
    download_pack_in_background(shared, entry, &url, ctx);
}

/// The download itself, on a worker thread. `url` is the catalogue manifest to read.
///
/// The pack is marked as on its way before the thread starts, so the row that asked for it
/// says so at once. When the download ends, the verified file waits for the daemon. A pack
/// that does not match the manifest never gets that far: the error goes to the user and
/// nothing is installed.
fn download_pack_in_background(
    shared: &Arc<Mutex<Shared>>,
    entry: &CatalogPack,
    url: &str,
    ctx: &egui::Context,
) {
    let shared = Arc::clone(shared);
    let entry = entry.clone();
    let url = url.to_string();
    let ctx = ctx.clone();
    {
        let mut s = shared.lock().unwrap();
        s.catalog_installing.push(entry.id.clone());
    }
    // The download runs on its own thread: the thread that draws the window returns at
    // once, so the window keeps drawing while the pack comes down.
    thread::spawn(move || {
        let dest = catalog_temp_path(&entry.id);
        log::info!("catalog: downloading {}", entry.id);
        let started = Instant::now();
        let result = catalog::download_to(&url, &entry, &dest);
        {
            let mut s = shared.lock().unwrap();
            match result {
                Ok(()) => {
                    log::info!(
                        "catalog: downloaded {} in {} ms",
                        entry.id,
                        started.elapsed().as_millis()
                    );
                    s.catalog_install_path.push(dest);
                }
                Err(e) => {
                    // The pack never lands: the row must not keep saying "installing…".
                    s.catalog_installing.retain(|id| id != &entry.id);
                    s.last_pack_op = Some((false, e));
                }
            }
        }
        // The result is here now: the window that shows it has to redraw at once, not at
        // its next tick.
        request_catalog_repaint(&ctx, &shared);
    });
}

/// The catalogue to read: the one the config names, else the official one.
fn catalogue_url() -> String {
    let configured = armoury_core::config::load_or_create()
        .ok()
        .and_then(|cfg| cfg.catalog_url);
    catalog::url_or_default(configured.as_deref())
}

/// The file a pack is downloaded into: the temp folder, under a name only the pack's own id
/// can make.
///
/// The id comes from the network, so it is not trusted with the file system: a name like
/// `../../x` must not write outside the temp folder.
fn catalog_temp_path(id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("armoury-catalog-{safe}.json"))
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

/// The picker's dialogs, by kind.
fn pick_dialog(kind: Picker) -> Option<PathBuf> {
    match kind {
        Picker::TrackedApp => {
            crate::pick::open_dialog("Pick an application to track", "Applications", "*.exe")
        }
        Picker::PackJson => crate::pick::open_dialog(
            "Pick a pack.json to install",
            "ArmouryDaemon packs",
            "pack.json",
        ),
    }
}

/// The running apps the picker offers: (window title, exe path).
type AppList = Arc<Vec<(String, String)>>;
/// Where the worker publishes the latest list. `None` until the first refresh lands.
type AppSlot = Arc<Mutex<Option<AppList>>>;

/// How often the worker refreshes the running-app list (~2 s is plenty).
const APPS_REFRESH_EVERY: Duration = Duration::from_secs(2);

/// Publish the latest list into the slot. The slot holds one snapshot, never a queue.
fn publish_apps(slot: &AppSlot, list: Vec<(String, String)>) {
    *slot.lock().unwrap() = Some(Arc::new(list));
}

/// The latest published list, or `None` while the first refresh has not landed yet.
fn read_apps(slot: &AppSlot) -> Option<AppList> {
    slot.lock().unwrap().clone()
}

/// What the picker says when nothing matches. While the list is still being built
/// "no match" would be a lie: the app may well be running.
fn no_match_message(ready: bool) -> &'static str {
    if ready {
        "No running app matches — it has to be open, or use Browse…"
    } else {
        "Looking for running apps…"
    }
}

/// How many matches the picker's inline list shows at once.
const APP_MATCH_ROWS: usize = 8;

/// The line under a list of matches that was cut short: how many matches are not
/// shown. A list that reads as the whole truth but is not is a lie, so the count is
/// always said.
fn more_matches_note(total: usize, shown: usize) -> Option<String> {
    let more = total.checked_sub(shown)?;
    if more == 0 {
        return None;
    }
    let word = if more == 1 { "match" } else { "matches" };
    Some(format!("… {more} more {word} — type to narrow the list"))
}

/// The worker that keeps the running-app list fresh.
///
/// Building the list costs a process-table walk plus a query per process (~13 ms), and it
/// is only needed by the add-app picker. So it is built here, on a worker thread, and the
/// egui thread only reads the latest snapshot - it never waits for the walk.
///
/// The worker stops when the UI closes: it is woken up at once and ends its loop.
struct AppListWorker {
    /// Asks the worker to stop.
    stop: Option<crossbeam_channel::Sender<()>>,
    /// Says the worker has ended.
    done: Option<crossbeam_channel::Receiver<()>>,
    /// The worker's thread.
    join: Option<thread::JoinHandle<()>>,
}

impl AppListWorker {
    /// Start the worker: it publishes a fresh list every `APPS_REFRESH_EVERY`, starting
    /// right away so the picker is not empty for the first seconds.
    fn start(slot: &AppSlot) -> Self {
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let slot = Arc::clone(slot);
        let join = thread::spawn(move || {
            loop {
                let list = list_running_apps();
                publish_apps(&slot, list);
                match stop_rx.recv_timeout(APPS_REFRESH_EVERY) {
                    // Asked to stop, or nobody is listening any more.
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    // The time is up: refresh again.
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                }
            }
            log::info!("the running-app worker stopped");
            let _ = done_tx.send(());
        });
        Self {
            stop: Some(stop_tx),
            done: Some(done_rx),
            join: Some(join),
        }
    }

    /// Ask the worker to stop. It is woken up at once.
    fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.try_send(());
        }
    }

    /// Wait for the worker to end. `true` when it is gone.
    fn wait_stopped(&mut self, limit: Duration) -> bool {
        let Some(rx) = &self.done else {
            return true;
        };
        if rx.recv_timeout(limit).is_err() {
            return false;
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        true
    }
}

impl Drop for AppListWorker {
    fn drop(&mut self) {
        // The UI is gone: the worker must not outlive it.
        self.stop();
    }
}

/// Every running process: (window title or empty, exe path), deduplicated by path.
/// The picker lists all of them (user decision 2026-10-18) — background apps like G-Helper have no
/// window at all, and they are exactly the kind of thing people want to track.
fn list_running_apps() -> Vec<(String, String)> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GWL_EXSTYLE, GW_OWNER, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
        GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    // pid -> a visible top-level window title, used as a friendly label when there is one
    static TITLES: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
    unsafe extern "system" fn collect_titles(hwnd: HWND, _lparam: LPARAM) -> windows_sys::core::BOOL {
        unsafe {
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            if !GetWindow(hwnd, GW_OWNER).is_null() {
                return 1;
            }
            if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & (WS_EX_TOOLWINDOW as isize) != 0 {
                return 1;
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
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != 0
                && let Ok(mut titles) = TITLES.lock()
                && !titles.iter().any(|(p, _)| *p == pid)
            {
                titles.push((pid, String::from_utf16_lossy(&buf)));
            }
        }
        1
    }

    if let Ok(mut titles) = TITLES.lock() {
        titles.clear();
    }
    unsafe {
        EnumWindows(Some(collect_titles), 0);
    }
    let titles = TITLES.lock().map(|t| t.clone()).unwrap_or_default();

    let own = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut out: Vec<(String, String)> = Vec::new();

    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() || snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return out;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while ok {
        let pid = entry.th32ProcessID;
        if pid != 0 && pid != 4
            && let Some(path) = crate::watch::process_path(pid)
        {
            if !own.is_empty() && path.eq_ignore_ascii_case(&own) {
                ok = unsafe { Process32NextW(snap, &mut entry) } != 0;
                continue;
            }
            if !out.iter().any(|(_, p)| p.eq_ignore_ascii_case(&path)) {
                let title = titles
                    .iter()
                    .find(|(p, _)| *p == pid)
                    .map(|(_, t)| t.clone())
                    .unwrap_or_default();
                out.push((title, path));
            }
        }
        ok = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    unsafe { windows_sys::Win32::Foundation::CloseHandle(snap) };
    out.sort_by_key(|(_, path)| {
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or(path)
            .to_ascii_lowercase()
    });
    out
}

/// Primary monitor's work area in physical pixels.
fn work_area_px() -> Option<Rect> {
    let (left, top, right, bottom) = work_area_physical_px()?;
    Some(Rect::from_min_max(
        egui::pos2(left as f32, top as f32),
        egui::pos2(right as f32, bottom as f32),
    ))
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
    let ppp = ctx.pixels_per_point().max(0.5);
    Some((
        work.left as f32 / ppp,
        work.top as f32 / ppp,
        work.right as f32 / ppp,
        work.bottom as f32 / ppp,
    ))
}

/// Primary monitor's work area in logical points, before any window exists (the DPI scale is the
/// system one). Falls back to the console band's synthetic 1080p work area if the query fails.
fn work_area_rect() -> Rect {
    let scale = system_scale().max(0.5);
    let (left, top, right, bottom) = work_area_physical_px().unwrap_or((0, 0, 1920, 1040));
    Rect::from_min_max(
        egui::pos2(left as f32 / scale, top as f32 / scale),
        egui::pos2(right as f32 / scale, bottom as f32 / scale),
    )
}

/// Primary monitor's work area in the running window's logical points.
fn work_area(ctx: &egui::Context) -> Rect {
    match work_area_pts(ctx) {
        Some((left, top, right, bottom)) => Rect::from_min_max(
            egui::pos2(left, top),
            egui::pos2(right, bottom),
        ),
        None => work_area_rect(),
    }
}

/// Top-left corner for a window of `size` in `work`'s bottom-right corner, `margin` points in
/// from the right and bottom edges (i.e. the spot directly above the system tray).
fn bottom_right(work: Rect, size: egui::Vec2, margin: f32) -> egui::Pos2 {
    egui::pos2(
        (work.right() - size.x - margin).max(work.left()),
        (work.bottom() - size.y - margin).max(work.top()),
    )
}

/// Bottom-right corner of the primary monitor: right-aligned and bottom-aligned to the work area,
/// so the window opens directly above the system tray.
fn bottom_corner(ctx: &egui::Context, size: egui::Vec2) -> egui::Pos2 {
    bottom_right(work_area(ctx), size, UI_MARGIN)
}

/// Snug width for a panel: the longest line plus the frame padding, clamped to the shared minimum
/// and to the work area (keeping `UI_MARGIN` free on each side).
fn snug_width(longest_line: f32, work_width: Option<f32>) -> f32 {
    let wanted = longest_line + 2.0 * PAD_F + 8.0;
    let cap = work_width
        .map(|w| (w - 2.0 * UI_MARGIN).max(MIN_SETTINGS_WIDTH))
        .unwrap_or(f32::INFINITY);
    wanted.clamp(MIN_SETTINGS_WIDTH, cap)
}

/// The marker on a pack row that says the pack is applied. It comes and goes as the packs
/// switch, so its space is always reserved: the width of the window drives its size, and
/// the size moves the window.
const APPLIED_MARKER: &str = "· applied";
/// The badge on a pack row that says how many settings the row does not show. Reserved
/// like the applied marker, for the same reason.
const BADGE_RESERVE: &str = "+9";
/// The status lines the dashboard can show, with the countdown at its widest and the
/// numbers at their largest. The window is measured from these, so a ticking countdown, a
/// pause or a killswitch never resizes it.
const STATUS_RESERVE: [&str; 6] = [
    "Idle",
    "Idle mode",
    "Waiting for daemon…",
    "Triggered",
    "Cooling down — 0000s",
    "Killswitch — 99 app(s) hidden + muted",
];
/// The status lines the "Paused — " prefix goes in front of. The killswitch and the idle
/// line are not among them: they are only shown while the daemon is not paused.
const PAUSED_RESERVE: [&str; 4] = [
    "Idle",
    "Waiting for daemon…",
    "Triggered",
    "Cooling down — 0000s",
];
/// The rows whose value ticks: the uptime and the cooldown.
const TICKING_ROWS: [&str; 2] = ["uptime", "cooldown"];

/// The fonts the dashboard's width is measured in.
#[derive(Clone, Copy)]
enum FontKind {
    Title,
    Status,
    Body,
    Small,
}

/// The content the dashboard's width is measured from.
struct WidthProbe {
    /// The status line as shown, plus the reserved variants.
    status: Vec<String>,
    /// The rows: (key, value), with the ticking numbers normalized.
    rows: Vec<(String, String)>,
    /// The pack rows: (name, has a dropdown).
    packs: Vec<(String, bool)>,
}

/// The content the width is measured from. The ticking numbers are normalized and the
/// status line is joined by the variants it can take, so the width only changes when the
/// content really changed.
fn width_probe(status: &str, rows: &[(String, String)], packs: &[PackStatus]) -> WidthProbe {
    let mut lines = vec![status.to_string()];
    lines.extend(STATUS_RESERVE.iter().map(|s| s.to_string()));
    lines.extend(PAUSED_RESERVE.iter().map(|s| format!("Paused — {s}")));
    WidthProbe {
        status: lines,
        rows: rows
            .iter()
            .map(|(k, v)| {
                let v = if TICKING_ROWS.contains(&k.as_str()) {
                    pad_numbers(v)
                } else {
                    v.clone()
                };
                (k.clone(), v)
            })
            .collect(),
        packs: packs
            .iter()
            .map(|p| (p.name.clone(), !p.settings.is_empty()))
            .collect(),
    }
}

/// Replace every number with a two-digit placeholder, so a ticking counter does not change
/// the width of the window: "7s left" is measured as "07s left", the same as "48s left".
fn pad_numbers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut digits = 0;
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else {
            if digits > 0 {
                out.push_str("00");
                digits = 0;
            }
            out.push(c);
        }
    }
    if digits > 0 {
        out.push_str("00");
    }
    out
}

/// The width the dashboard needs, measured from the probe. The "· applied" marker and the
/// "+N" badge are always measured, even when they are not shown: a pack switching would
/// otherwise change the width, and the width drives the window's size - which moves the
/// window.
fn probe_width(measure: &dyn Fn(FontKind, &str) -> f32, probe: &WidthProbe) -> f32 {
    let mut w = measure(FontKind::Title, "ArmouryDaemon") + 72.0; // room for gear + close
    for line in &probe.status {
        w = w.max(measure(FontKind::Status, line) + 26.0);
    }
    for (key, value) in &probe.rows {
        w = w.max(measure(FontKind::Body, key) + 32.0 + measure(FontKind::Body, value));
    }
    for (name, dropdown) in &probe.packs {
        let mut row = measure(FontKind::Body, name) + 56.0; // ‹ button + checkbox
        row += measure(FontKind::Small, APPLIED_MARKER) + 10.0; // always reserved
        row += measure(FontKind::Small, BADGE_RESERVE) + 10.0; // always reserved
        if *dropdown {
            row += 130.0; // the dropdown
        }
        w = w.max(row);
    }
    w
}

/// How far the window may sit from the position we asked for before it counts as moved.
const DRAG_SLACK: f32 = 4.0;
/// How long the window has to sit away from the position we asked for before it is read
/// as the user moving it. Our own resize moves the window too, and the OS applies it a
/// moment later: that gap must never be read as a drag.
const DRAG_SETTLE: Duration = Duration::from_millis(300);

/// Where the dashboard is expected to be, and how far it got there.
#[derive(Default, Clone, Copy)]
struct Placement {
    /// The position we last asked for.
    requested: Option<egui::Pos2>,
    /// The window was seen at the requested position since the last request. Only then can
    /// a deviation be read as the user moving it.
    settled: bool,
    /// When the window was first seen away from the requested position.
    off_since: Option<Instant>,
}

impl Placement {
    /// Remember the position we just asked for. The window is somewhere else until the OS
    /// applies it, so a mismatch right after this is not a drag.
    fn request(&mut self, pos: egui::Pos2) {
        self.requested = Some(pos);
        self.settled = false;
        self.off_since = None;
    }
}

/// What the window's actual position says.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Spot {
    /// The window is where it belongs.
    Home,
    /// The window is somewhere else, but it may be our own resize still landing.
    Away,
    /// The user moved the window: it stays where they put it.
    Dragged(egui::Pos2),
}

/// Read the window's actual position. A position that differs from the one we asked for is
/// not a drag on its own: our own resize moves the window too, and the OS applies it a
/// moment later. The window has to sit away from the requested position for `DRAG_SETTLE`,
/// and only after it has been where we asked for it, before it counts as the user moving
/// it.
fn classify(place: &mut Placement, pos: egui::Pos2, pinned: bool, now: Instant) -> Spot {
    let Some(req) = place.requested else {
        return Spot::Home;
    };
    if (pos - req).length() <= DRAG_SLACK {
        place.settled = true;
        place.off_since = None;
        return Spot::Home;
    }
    if pinned {
        // The window sits in the corner of the work area: that is where it belongs, whatever
        // the position we asked for was.
        place.off_since = None;
        return Spot::Home;
    }
    if !place.settled {
        // The window has not been where we asked for it since the last request: this is our
        // own resize still being applied, not a drag.
        place.off_since = None;
        return Spot::Away;
    }
    let since = *place.off_since.get_or_insert(now);
    if now.saturating_duration_since(since) >= DRAG_SETTLE {
        place.off_since = None;
        return Spot::Dragged(pos);
    }
    Spot::Away
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A pack as the dashboard sees it.
    fn pack(name: &str, applied: bool, settings: usize) -> PackStatus {
        PackStatus {
            id: name.to_lowercase().replace(' ', "-"),
            name: name.to_string(),
            version: "1.0.0".to_string(),
            enabled: true,
            settings: (0..settings)
                .map(|i| armoury_core::status::PackSettingStatus {
                    key: format!("k{i}"),
                    label: format!("K{i}"),
                    values: vec!["a".to_string(), "b".to_string()],
                    value: Some("a".to_string()),
                })
                .collect(),
            applied,
        }
    }

    /// The rows the dashboard shows.
    fn rows(cooldown: &str) -> Vec<(String, String)> {
        vec![
            ("daemon".to_string(), "v1.0.0".to_string()),
            ("uptime".to_string(), "1:02:03".to_string()),
            ("cooldown".to_string(), cooldown.to_string()),
            (
                "packs".to_string(),
                "3 installed · 2 enabled".to_string(),
            ),
        ]
    }

    /// A measurer that stands in for the real fonts: every character is 7 points wide.
    fn fake_measure() -> impl Fn(FontKind, &str) -> f32 {
        |_kind: FontKind, text: &str| text.chars().count() as f32 * 7.0
    }

    /// A status snapshot with the SteamGridDB key fields set.
    fn status_with_key(set: bool, key: &str) -> StatusSnapshot {
        StatusSnapshot {
            daemon_version: "1.0.0".to_string(),
            uptime_secs: 0,
            state: RunState::Idle,
            trigger_source: None,
            cooldown_remaining_ms: None,
            cooldown_action: None,
            cooldown_seconds: 0,
            paused: false,
            killswitch_active: false,
            killswitch_count: 0,
            idle_active: false,
            idle_monitors: 0,
            idle_devices: 0,
            idle_failsafe_enabled: false,
            idle_failsafe_minutes: 0,
            tracked_apps: Vec::new(),
            tracked_apps_running: Vec::new(),
            packs: Vec::new(),
            console_open: "never".to_string(),
            console_apps: Vec::new(),
            console_hidden: Vec::new(),
            last_played: Default::default(),
            sgdb_api_key_set: set,
            sgdb_api_key: key.to_string(),
            last_error: None,
        }
    }

    /// A window in the chain, sitting in the slot the chain gave it (physical pixels).
    fn chain_window(item: ChainItem, size_px: egui::Vec2, slot_px: egui::Pos2) -> ChainWindow {
        ChainWindow {
            item,
            size_px,
            current_px: slot_px,
            dragged: false,
        }
    }

    /// A rect in physical pixels, the way the placement sees it.
    fn to_px(rect: Rect, ppp: f32) -> Rect {
        Rect::from_min_max(
            egui::pos2((rect.left() * ppp).round(), (rect.top() * ppp).round()),
            egui::pos2((rect.right() * ppp).round(), (rect.bottom() * ppp).round()),
        )
    }

    /// The chain at a given scale: the work area and the dashboard in points (the way the app
    /// keeps them), the windows' sizes in points, the result in physical pixels.
    fn chain_px(
        work: Rect,
        core: Rect,
        windows: &[(ChainItem, egui::Vec2)],
        ppp: f32,
    ) -> (Rect, f32, f32, Vec<ChainWindow>) {
        let core_px = to_px(core, ppp);
        let chain = windows
            .iter()
            .map(|(item, size)| ChainWindow {
                item: item.clone(),
                size_px: egui::vec2((size.x * ppp).round(), (size.y * ppp).round()),
                current_px: egui::pos2(0.0, 0.0),
                dragged: false,
            })
            .collect();
        (to_px(work, ppp), core_px.left(), core_px.bottom(), chain)
    }

    /// The chain's positions in points, the way the app asks the OS for them.
    fn chain_points(
        work: Rect,
        core: Rect,
        windows: &[(ChainItem, egui::Vec2)],
        ppp: f32,
    ) -> Vec<egui::Pos2> {
        let (work_px, core_left_px, core_bottom_px, chain) = chain_px(work, core, windows, ppp);
        reanchor_chain(work_px, core_left_px, core_bottom_px, &chain)
            .into_iter()
            .map(|(_, pos_px)| egui::pos2(pos_px.x / ppp, pos_px.y / ppp))
            .collect()
    }

    /// Where a window goes when it joins the chain, in physical pixels: the anchor's left
    /// edge (physical pixels), the dashboard and the window's size (points, at a given scale).
    fn place_px(
        work: Rect,
        core: Rect,
        anchor_left_px: f32,
        size: egui::Vec2,
        ppp: f32,
    ) -> egui::Pos2 {
        let core_px = to_px(core, ppp);
        place_left_px_in(
            to_px(work, ppp),
            chain_anchor(core_px.left(), &[anchor_left_px]),
            core_px.bottom(),
            egui::vec2((size.x * ppp).round(), (size.y * ppp).round()),
        )
    }

    #[test]
    fn a_pack_row_keeps_its_width_when_the_applied_marker_comes_and_goes() {
        // The "· applied" marker and the "+N" badge are always measured, so a pack
        // switching does not change the width: the width drives the window's size, and the
        // size moves the window.
        let measure = fake_measure();
        let applied = vec![
            pack("Wallpaper Engine", true, 2),
            pack("G-Helper", false, 1),
            pack("Rainmeter", false, 1),
        ];
        let plain = applied
            .iter()
            .map(|p| pack(&p.name, false, p.settings.len()))
            .collect::<Vec<_>>();
        let with = probe_width(&measure, &width_probe("Idle", &rows("60s"), &applied));
        let without = probe_width(&measure, &width_probe("Idle", &rows("60s"), &plain));
        assert_eq!(with, without);
    }

    #[test]
    fn the_countdown_does_not_change_the_width() {
        // The status line ticks every second and the cooldown row counts down: neither
        // may resize the window.
        let measure = fake_measure();
        let packs = vec![pack("Wallpaper Engine", true, 2)];
        let wide = probe_width(&measure, &width_probe("Cooling down — 48s", &rows("48s left"), &packs));
        let narrow = probe_width(&measure, &width_probe("Cooling down — 7s", &rows("7s left"), &packs));
        assert_eq!(wide, narrow);
        // The numbers themselves are measured with a fixed width.
        assert_eq!(pad_numbers("48s left"), pad_numbers("7s left"));
        assert_eq!(pad_numbers("1:02:03"), pad_numbers("1:02:09"));
        assert_eq!(pad_numbers("9:59"), pad_numbers("9:01"));
    }

    #[test]
    fn our_own_resize_is_not_read_as_a_drag() {
        // The window is in the corner and we ask for a new position: the OS applies it a
        // moment later, so the window is somewhere else for a frame. That is our own
        // resize, not the user dragging the window.
        let corner = egui::pos2(1668.0, 886.0);
        let mut place = Placement::default();
        let now = Instant::now();
        place.request(corner);
        assert_eq!(classify(&mut place, corner, false, now), Spot::Home);
        // The resize: we ask for a new position, the window has not moved yet.
        let moved = egui::pos2(1700.0, 900.0);
        place.request(moved);
        assert_eq!(
            classify(&mut place, corner, false, now + Duration::from_millis(16)),
            Spot::Away
        );
        // Even a long time later it is still our own resize, as long as the window was
        // never where we asked for it.
        assert_eq!(
            classify(&mut place, corner, false, now + Duration::from_secs(5)),
            Spot::Away
        );
        // The window arrives.
        assert_eq!(
            classify(&mut place, moved, false, now + Duration::from_millis(32)),
            Spot::Home
        );
    }

    #[test]
    fn a_mismatch_that_stays_is_a_drag() {
        // The user drags the window away from the corner: the first frame is only a
        // difference, but it stays there.
        let corner = egui::pos2(1668.0, 886.0);
        let dragged = egui::pos2(1000.0, 400.0);
        let mut place = Placement::default();
        let now = Instant::now();
        place.request(corner);
        assert_eq!(classify(&mut place, corner, false, now), Spot::Home);
        assert_eq!(
            classify(&mut place, dragged, false, now + Duration::from_millis(16)),
            Spot::Away
        );
        assert_eq!(
            classify(
                &mut place,
                dragged,
                false,
                now + Duration::from_millis(16) + DRAG_SETTLE
            ),
            Spot::Dragged(dragged)
        );
    }

    #[test]
    fn a_window_in_the_corner_stays_in_the_corner() {
        // The work area's bottom-right corner is where the window belongs, whatever the
        // position we asked for was: the OS may put it a point or two elsewhere.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let size = egui::vec2(416.0, 520.0);
        let pos = bottom_right(work, size, UI_MARGIN);
        assert!((pos.x + size.x - work.right()).abs() < 1e-3);
        assert!((pos.y + size.y - work.bottom()).abs() < 1e-3);
    }

    #[test]
    fn the_real_font_measures_one_width_for_the_whole_dashboard() {
        // The same check with the real system font: the width of the window must not
        // depend on the countdown or on a pack being applied.
        let ctx = egui::Context::default();
        // The fonts are only built once the context has run a pass.
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        install_fonts(&ctx);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        let p = ctx.debug_painter();
        let measure = |kind: FontKind, text: &str| {
            let font = match kind {
                FontKind::Title | FontKind::Status => FontId::new(17.0, FontFamily::Proportional),
                FontKind::Body => FontId::new(15.5, FontFamily::Proportional),
                FontKind::Small => FontId::new(13.0, FontFamily::Proportional),
            };
            p.layout_no_wrap(text.to_owned(), font, Color32::WHITE)
                .rect
                .width()
        };
        let packs = vec![
            pack("Wallpaper Engine", true, 2),
            pack("G-Helper", false, 1),
            pack("Rainmeter", false, 1),
        ];
        let cooling = probe_width(&measure, &width_probe("Cooling down — 48s", &rows("48s left"), &packs));
        let nearly = probe_width(&measure, &width_probe("Cooling down — 7s", &rows("7s left"), &packs));
        assert_eq!(cooling, nearly);
        // The packs switch: the applied marker comes and goes.
        let plain = packs
            .iter()
            .map(|p| pack(&p.name, false, p.settings.len()))
            .collect::<Vec<_>>();
        let switched = probe_width(&measure, &width_probe("Cooling down — 48s", &rows("48s left"), &plain));
        assert_eq!(cooling, switched);

        // The paused band has to fit on one line, or it wraps and the window grows.
        let band = p
            .layout_no_wrap(
                "Paused — the packs will not switch. Press Start.".to_owned(),
                FontId::new(15.5, FontFamily::Proportional),
                Color32::WHITE,
            )
            .rect
            .width();
        assert!(band <= cooling - 20.0, "the paused band does not fit on one line");
    }

    #[test]
    fn snug_width_follows_the_content_and_the_screen() {
        // A ~382 pt helper line plus the frame padding is the snug width.
        assert!((snug_width(382.0, Some(1638.4)) - 410.0).abs() < 1e-3);
        // Narrow content still gets the shared minimum.
        assert!((snug_width(10.0, Some(1638.4)) - MIN_SETTINGS_WIDTH).abs() < 1e-3);
        // Wide content is capped so the window still fits inside the work area.
        assert!((snug_width(5000.0, Some(1638.4)) - (1638.4 - 2.0 * UI_MARGIN)).abs() < 1e-3);
        // A work area narrower than the minimum cannot push the width below it.
        assert!((snug_width(5000.0, Some(300.0)) - MIN_SETTINGS_WIDTH).abs() < 1e-3);
        // No work area (the query failed) means no cap.
        assert!((snug_width(5000.0, None) - 5028.0).abs() < 1e-3);
    }

    #[test]
    fn bottom_right_sits_flush_in_the_work_area_corner() {
        // 2560x1380 px work area at 125% → 2048 x 1104 pt on the primary monitor.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let size = egui::vec2(475.0, 749.0);
        let pos = bottom_right(work, size, UI_MARGIN);
        // both edges sit exactly on the work area's right and bottom edges
        assert!((pos.x + size.x - work.right()).abs() < 1e-3);
        assert!((pos.y + size.y - work.bottom()).abs() < 1e-3);
        assert!((pos.x - 1573.0).abs() < 1e-3);
        assert!((pos.y - 355.0).abs() < 1e-3);
    }

    #[test]
    fn growing_the_window_keeps_the_bottom_edge() {
        // The window is anchored by its bottom edge: when the content grows, the top moves up
        // and the bottom stays on the work area - the window never drops off the screen.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let small = bottom_right(work, egui::vec2(475.0, 400.0), UI_MARGIN);
        let large = bottom_right(work, egui::vec2(475.0, 749.0), UI_MARGIN);
        assert!((small.y + 400.0 - work.bottom()).abs() < 1e-3);
        assert!((large.y + 749.0 - work.bottom()).abs() < 1e-3);
        // the top edge moved up by exactly the growth
        assert!((small.y - large.y - 349.0).abs() < 1e-3);
    }

    #[test]
    fn the_dashboard_always_opens_in_the_corner() {
        // Nothing is restored from the config: the window always comes up in the corner.
        assert!(start_anchor().is_none());
        // The corner is the bottom-right of the work area, flush above the tray.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let size = egui::vec2(MIN_WINDOW_WIDTH, 218.0);
        let pos = bottom_right(work, size, UI_MARGIN);
        // Both edges sit exactly on the work area's right and bottom edges.
        assert!((pos.x + size.x - work.right()).abs() < 1e-3);
        assert!((pos.y + size.y - work.bottom()).abs() < 1e-3);
        assert!((pos.x - 1668.0).abs() < 1e-3);
        assert!((pos.y - 886.0).abs() < 1e-3);
    }

    #[test]
    fn the_settings_joins_the_chain_left_of_the_dashboard() {
        // 2560x1380 px work area at 125% -> 2048 x 1104 pt on the primary monitor.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        // The dashboard sits in the bottom-right corner, flush with the work area.
        let core = Rect::from_min_max(egui::pos2(1668.0, 886.0), egui::pos2(2048.0, 1104.0));
        let size = egui::vec2(420.0, 500.0);
        let ppp = 1.25;
        let core_left_px = (core.left() * ppp).round();

        // Nothing else open: the settings goes immediately left of the dashboard.
        let pos = chain_points(work, core, &[(ChainItem::Settings, size)], ppp)[0];
        // x = core.left() - 5 px - width = 1668 - 4 - 420, y = core.bottom() - height: the
        // settings stands on the dashboard's bottom edge.
        assert!((pos.x - 1244.0).abs() < 1e-3);
        assert!((pos.y - 604.0).abs() < 1e-3);
        // Exactly 5 physical pixels (4 pt at 125%) clear of the dashboard.
        assert!((pos.x + size.x - (core.left() - CHAIN_GAP_PX / ppp)).abs() < 1e-3);
        // Bottom-aligned with the dashboard: the very same bottom edge.
        assert!((pos.y + size.y - core.bottom()).abs() < 1e-3);
        // Beside the dashboard, never above it: the vertical spans overlap, the horizontal ones
        // do not.
        assert!(pos.y < core.bottom() && pos.y + size.y > core.top());
        assert!(pos.x + size.x <= core.left());
        // On screen.
        assert!(pos.x >= work.left() && pos.x + size.x <= work.right());
        assert!(pos.y >= work.top() && pos.y + size.y <= work.bottom());

        // A pack window is already open, to the left of the dashboard: the settings has to
        // clear that one too and goes to its left.
        let pack_left_px = 1500.0;
        let pos_px = place_px(work, core, pack_left_px, size, ppp);
        let pos = egui::pos2(pos_px.x / ppp, pos_px.y / ppp);
        // x = pack.left() - 5 px - width = 1200 - 4 - 420, still bottom-aligned.
        assert!((pos.x - 776.0).abs() < 1e-3);
        assert!((pos.y - 604.0).abs() < 1e-3);
        assert!((pos.x + size.x - (pack_left_px / ppp - CHAIN_GAP_PX / ppp)).abs() < 1e-3);
        // The order from the right is: dashboard, pack, settings.
        assert!(core.left() > pack_left_px / ppp && pack_left_px / ppp > pos.x);

        // The settings was open first, a pack window opens after it: the pack goes to the left
        // of the settings, so the order from the right is: dashboard, settings, pack.
        let settings_px = place_px(work, core, core_left_px, size, ppp);
        let new_pack_px = place_px(work, core, settings_px.x, egui::vec2(380.0, 300.0), ppp);
        assert!((new_pack_px.x - (settings_px.x - CHAIN_GAP_PX - 475.0)).abs() < 1e-3);
        assert!((new_pack_px.x + 475.0 - (settings_px.x - CHAIN_GAP_PX)).abs() < 1e-3);
        assert!(core_left_px > settings_px.x && settings_px.x > new_pack_px.x);

        // A dashboard filling the whole work area leaves no room on the left: the settings stays
        // on the screen anyway.
        let full = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let pos_px = place_px(work, full, 0.0, size, ppp);
        assert!((pos_px.x - 0.0).abs() < 1e-3);
        assert!(pos_px.x >= 0.0 && pos_px.x + 525.0 <= (work.right() * ppp).round());
        assert!(pos_px.y >= 0.0 && pos_px.y + 625.0 <= (work.bottom() * ppp).round());
    }

    #[test]
    fn the_settings_keeps_its_slot_when_the_content_grows() {
        // 2560x1380 px work area at 125% -> 2048 x 1104 pt on the primary monitor.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let core = Rect::from_min_max(egui::pos2(1668.0, 886.0), egui::pos2(2048.0, 1104.0));
        let ppp = 1.25;
        let core_left_px = (core.left() * ppp).round();
        let pos_px = place_px(work, core, core_left_px, egui::vec2(420.0, 500.0), ppp);
        let work_px = to_px(work, ppp);

        // The content grows but the window still fits: the stored position is kept as it is.
        let kept = clamp_to_work_area_px(pos_px, work_px, egui::vec2(525.0, 625.0));
        assert!((kept.x - pos_px.x).abs() < 1e-3);
        assert!((kept.y - pos_px.y).abs() < 1e-3);
        // A pack window opened after the settings sits to its left; a resize does not change
        // that order, because the settings never re-anchors against another window.
        let pack_px = place_px(work, core, pos_px.x, egui::vec2(380.0, 300.0), ppp);
        assert!(pack_px.x + 475.0 <= pos_px.x);
        let grown = clamp_to_work_area_px(pos_px, work_px, egui::vec2(525.0, 700.0));
        assert!((grown.x - pos_px.x).abs() < 1e-3);
        assert!(grown.x > pack_px.x);

        // Grown past the bottom of the work area: the position is clamped into it, the slot (the
        // x) is untouched.
        assert!((grown.y - (work_px.bottom() - 700.0)).abs() < 1e-3);
        assert!(grown.y >= work_px.top() && grown.y + 700.0 <= work_px.bottom());
        // Still to the left of the dashboard.
        assert!(grown.x + 525.0 <= core_left_px);
    }

    #[test]
    fn the_badge_counts_the_settings_the_row_does_not_show() {
        // A pack with one setting: nothing to point at.
        assert_eq!(extra_settings(0), None);
        assert_eq!(extra_settings(1), None);
        // Wallpaper Engine: the row shows the first setting, the pack window has both.
        assert_eq!(extra_settings(2), Some("+1".to_string()));
        assert_eq!(extra_settings(4), Some("+3".to_string()));
    }

    #[test]
    fn the_pack_window_keeps_its_slot_when_the_content_grows() {
        // 2560x1380 px work area at 125% -> 2048 x 1104 pt on the primary monitor.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let core = Rect::from_min_max(egui::pos2(1668.0, 886.0), egui::pos2(2048.0, 1104.0));
        let ppp = 1.25;
        let core_left_px = (core.left() * ppp).round();
        // The pack opened first, the settings after it: the settings sits to the left of the pack.
        let pack_px = place_px(work, core, core_left_px, egui::vec2(380.0, 300.0), ppp);
        let settings_px = place_px(work, core, pack_px.x, egui::vec2(420.0, 500.0), ppp);
        assert!(core_left_px > pack_px.x && pack_px.x > settings_px.x);

        // The pack grows: the position is kept as it is, only clamped into the work area.
        let work_px = to_px(work, ppp);
        let grown = clamp_to_work_area_px(pack_px, work_px, egui::vec2(475.0, 700.0));
        assert!((grown.x - pack_px.x).abs() < 1e-3);
        // The order from the right is unchanged: the pack did not jump over the settings.
        assert!(grown.x > settings_px.x);
        // Grown past the bottom of the work area: the y is clamped, the slot (the x) is not.
        assert!((grown.y - (work_px.bottom() - 700.0)).abs() < 1e-3);
        assert!(grown.y >= work_px.top() && grown.y + 700.0 <= work_px.bottom());
    }

    #[test]
    fn the_key_field_shows_dots_and_never_the_key() {
        // A key is set: the field shows a row of dots.
        let set = status_with_key(true, "");
        assert_eq!(sgdb_field_hint(Some(&set)), SGDB_MASK);
        // The key is never read back, not even when the daemon still sends one.
        let leaky = status_with_key(true, "s3cret-key");
        let hint = sgdb_field_hint(Some(&leaky));
        assert_eq!(hint, SGDB_MASK);
        assert!(!hint.contains("s3cret-key"));
        // No key: the field asks for one.
        assert_eq!(
            sgdb_field_hint(Some(&status_with_key(false, ""))),
            "paste your API key…"
        );
        // No status yet: the field asks for one.
        assert_eq!(sgdb_field_hint(None), "paste your API key…");
    }

    #[test]
    fn the_chain_is_reanchored_when_the_dashboard_changes() {
        // 2560x1380 px work area at 125% -> 2048 x 1104 pt on the primary monitor.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        // The dashboard sits in the bottom-right corner, flush with the work area.
        let core = Rect::from_min_max(egui::pos2(1668.0, 886.0), egui::pos2(2048.0, 1104.0));
        let ppp = 1.25;
        // The settings is immediately left of the dashboard, the pack window to its left.
        let settings = chain_window(ChainItem::Settings, egui::vec2(525.0, 625.0), egui::pos2(1555.0, 755.0));
        let pack = chain_window(
            ChainItem::Pack("g-helper".to_string()),
            egui::vec2(475.0, 375.0),
            egui::pos2(1075.0, 1005.0),
        );
        // The chain as it stands: every window touches the one to its right.
        let (work_px, core_left_px, core_bottom_px, _) =
            chain_px(work, core, &[], ppp);
        let out = reanchor_chain(work_px, core_left_px, core_bottom_px, &[settings.clone(), pack.clone()]);
        assert!((out[0].1.x - 1555.0).abs() < 1e-3);
        assert!((out[1].1.x - 1075.0).abs() < 1e-3);
        // 5 physical pixels between two windows.
        assert!((out[0].1.x + 525.0 - (core_left_px - CHAIN_GAP_PX)).abs() < 1e-3);
        assert!((out[1].1.x + 475.0 - (out[0].1.x - CHAIN_GAP_PX)).abs() < 1e-3);
        // The whole chain stands on the dashboard's bottom edge.
        assert!((out[0].1.y + 625.0 - core_bottom_px).abs() < 1e-3);

        // The dashboard changed: the chain is re-anchored against its new rect.
        let wider = Rect::from_min_max(egui::pos2(1500.0, 700.0), egui::pos2(2048.0, 1104.0));
        let (work_px, wider_left_px, wider_bottom_px, _) = chain_px(work, wider, &[], ppp);
        let out = reanchor_chain(work_px, wider_left_px, wider_bottom_px, &[settings, pack]);
        let (settings_pos, pack_pos) = (out[0].1, out[1].1);
        // The order from the right is kept: the settings, then the pack window.
        assert!(wider_left_px > settings_pos.x && settings_pos.x > pack_pos.x);
        // Every window sits immediately left of the one to its right, with the gap.
        assert!((settings_pos.x + 525.0 - (wider_left_px - CHAIN_GAP_PX)).abs() < 1e-3);
        assert!((pack_pos.x + 475.0 - (settings_pos.x - CHAIN_GAP_PX)).abs() < 1e-3);
        // Bottom-aligned with the dashboard.
        assert!((settings_pos.y + 625.0 - wider_bottom_px).abs() < 1e-3);
    }

    #[test]
    fn a_window_the_user_dragged_is_not_moved() {
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let core = Rect::from_min_max(egui::pos2(1668.0, 886.0), egui::pos2(2048.0, 1104.0));
        let ppp = 1.25;
        let slot = egui::pos2(1244.0, 604.0);
        // The user drags the settings window away from its slot: `classify` sees the drag.
        let dragged_to = egui::pos2(300.0, 400.0);
        let now = Instant::now();
        let mut place = Placement::default();
        place.request(slot);
        assert_eq!(classify(&mut place, slot, false, now), Spot::Home);
        assert_eq!(
            classify(&mut place, dragged_to, false, now + Duration::from_millis(16)),
            Spot::Away
        );
        let dragged = matches!(
            classify(
                &mut place,
                dragged_to,
                false,
                now + Duration::from_millis(16) + DRAG_SETTLE
            ),
            Spot::Dragged(_)
        );
        assert!(dragged);

        // The chain leaves it where the user put it.
        let mut window = chain_window(ChainItem::Settings, egui::vec2(525.0, 625.0), egui::pos2(1555.0, 755.0));
        window.current_px = egui::pos2(dragged_to.x * ppp, dragged_to.y * ppp);
        window.dragged = dragged;
        let (work_px, core_left_px, core_bottom_px, _) = chain_px(work, core, &[], ppp);
        let out = reanchor_chain(work_px, core_left_px, core_bottom_px, &[window]);
        let kept = egui::pos2(out[0].1.x / ppp, out[0].1.y / ppp);
        assert!((kept - dragged_to).length() < 1e-3);
    }

    #[test]
    fn the_reanchored_chain_stays_inside_the_work_area() {
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let ppp = 1.25;
        // A dashboard that fills the whole work area leaves no room on the left.
        let core = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let settings = chain_window(ChainItem::Settings, egui::vec2(525.0, 625.0), egui::pos2(0.0, 755.0));
        let pack = chain_window(
            ChainItem::Pack("g-helper".to_string()),
            egui::vec2(475.0, 375.0),
            egui::pos2(0.0, 1005.0),
        );
        let (work_px, core_left_px, core_bottom_px, _) = chain_px(work, core, &[], ppp);
        let out = reanchor_chain(work_px, core_left_px, core_bottom_px, &[settings, pack]);
        for ((_, pos), size) in out
            .iter()
            .zip([egui::vec2(525.0, 625.0), egui::vec2(475.0, 375.0)])
        {
            assert!(pos.x >= work_px.left() && pos.x + size.x <= work_px.right());
            assert!(pos.y >= work_px.top() && pos.y + size.y <= work_px.bottom());
        }

        // A dashboard that is taller than the work area: the chain is clamped into it.
        let tall = Rect::from_min_max(egui::pos2(1668.0, -400.0), egui::pos2(2048.0, 1600.0));
        let (work_px, tall_left_px, tall_bottom_px, _) = chain_px(work, tall, &[], ppp);
        let out = reanchor_chain(
            work_px,
            tall_left_px,
            tall_bottom_px,
            &[chain_window(
                ChainItem::Settings,
                egui::vec2(525.0, 625.0),
                egui::pos2(1555.0, 755.0),
            )],
        );
        for ((_, pos), size) in out.iter().zip([egui::vec2(525.0, 625.0)]) {
            assert!(pos.x >= work_px.left() && pos.x + size.x <= work_px.right());
            assert!(pos.y >= work_px.top() && pos.y + size.y <= work_px.bottom());
        }
    }

    #[test]
    fn the_chain_gap_is_five_physical_pixels_at_any_scale() {
        // The gap is a count of physical pixels, so the display's scale cannot change it.
        // The sizes stay in points and are scaled, which is what used to make the realised
        // gap four or six pixels.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let core = Rect::from_min_max(egui::pos2(1668.0, 886.0), egui::pos2(2048.0, 1104.0));
        let windows = [
            (ChainItem::Settings, egui::vec2(420.0, 500.0)),
            (ChainItem::InstallPacks, egui::vec2(INSTALL_PACKS_WIDTH, INSTALL_PACKS_HEIGHT)),
            (ChainItem::Pack("g-helper".to_string()), egui::vec2(380.0, 300.0)),
        ];
        for ppp in [1.0_f32, 1.25, 1.5, 2.0] {
            let (work_px, core_left_px, core_bottom_px, chain) = chain_px(work, core, &windows, ppp);
            let out = reanchor_chain(work_px, core_left_px, core_bottom_px, &chain);
            for (index, window) in chain.iter().enumerate() {
                let pos_px = out[index].1;
                // The gap to the window on the right is exactly 5 physical pixels.
                if index > 0 {
                    let right = out[index - 1].1;
                    assert_eq!(
                        right.x - (pos_px.x + window.size_px.x),
                        CHAIN_GAP_PX,
                        "the gap at {ppp}x is not 5 physical pixels"
                    );
                }
                // The bottom edge is the dashboard's, to the pixel.
                assert_eq!(
                    pos_px.y + window.size_px.y,
                    core_bottom_px,
                    "the bottom edge at {ppp}x does not coincide"
                );
                // The trip through points is exact: the OS applies the position we ask for.
                let pos_pt = egui::pos2(pos_px.x / ppp, pos_px.y / ppp);
                assert_eq!((pos_pt.x * ppp).round(), pos_px.x);
                assert_eq!((pos_pt.y * ppp).round(), pos_px.y);
            }
            // The first window clears the dashboard by the same 5 pixels.
            assert_eq!(
                core_left_px - (out[0].1.x + chain[0].size_px.x),
                CHAIN_GAP_PX,
                "the gap to the dashboard at {ppp}x is not 5 physical pixels"
            );
        }
    }

    /// A catalog entry as the manifest holds it.
    fn catalog_entry(id: &str, name: &str, version: &str) -> CatalogPack {
        CatalogPack {
            id: id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            description: String::new(),
            file: format!("packs/{id}/pack.json"),
            sha256: "00".to_string(),
            size: None,
            min_engine: None,
        }
    }

    /// A catalog entry that asks for a daemon newer than this one.
    fn catalog_entry_needing(id: &str, name: &str, version: &str, min_engine: &str) -> CatalogPack {
        CatalogPack {
            min_engine: Some(min_engine.to_string()),
            ..catalog_entry(id, name, version)
        }
    }

    /// A catalog entry that says what the pack does, as the manifest holds it.
    fn catalog_entry_with_description(
        id: &str,
        name: &str,
        version: &str,
        description: &str,
    ) -> CatalogPack {
        CatalogPack {
            description: description.to_string(),
            ..catalog_entry(id, name, version)
        }
    }

    /// An installed pack as the daemon reports it.
    fn installed_pack(id: &str, version: &str) -> PackStatus {
        PackStatus {
            id: id.to_string(),
            name: id.to_string(),
            version: version.to_string(),
            enabled: true,
            settings: Vec::new(),
            applied: false,
        }
    }

    /// An installed pack with the name the row shows.
    fn named_pack(id: &str, name: &str) -> PackStatus {
        PackStatus {
            name: name.to_string(),
            ..installed_pack(id, "1.0.0")
        }
    }

    /// The names of the pack rows, in the order they are shown.
    fn shown_names(packs: &[PackStatus]) -> Vec<String> {
        sorted_packs(packs)
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
    }

    #[test]
    fn the_pack_rows_are_shown_by_name() {
        // The packs as the daemon hands them over: the order it applies them in.
        let packs = vec![
            named_pack("wallpaper-engine", "Wallpaper Engine"),
            named_pack("g-helper", "G-Helper"),
            named_pack("rainmeter", "Rainmeter"),
        ];
        // The rows are shown by name instead.
        assert_eq!(
            shown_names(&packs),
            vec!["G-Helper", "Rainmeter", "Wallpaper Engine"]
        );
    }

    #[test]
    fn the_pack_rows_are_sorted_without_caring_about_the_case() {
        let packs = vec![
            named_pack("beta", "beta"),
            named_pack("alpha", "Alpha"),
            named_pack("charlie", "CHARLIE"),
        ];
        assert_eq!(shown_names(&packs), vec!["Alpha", "beta", "CHARLIE"]);
    }

    #[test]
    fn two_packs_with_the_same_name_are_told_apart_by_their_id() {
        let packs = vec![
            named_pack("zebra", "Same name"),
            named_pack("aardvark", "Same name"),
        ];
        let ids: Vec<&str> = sorted_packs(&packs)
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(ids, vec!["aardvark", "zebra"]);
    }

    #[test]
    fn a_renamed_pack_moves_to_its_new_place() {
        let mut packs = vec![
            named_pack("rainmeter", "Rainmeter"),
            named_pack("g-helper", "G-Helper"),
            named_pack("wallpaper-engine", "Wallpaper Engine"),
        ];
        assert_eq!(
            shown_names(&packs),
            vec!["G-Helper", "Rainmeter", "Wallpaper Engine"]
        );
        // The pack is renamed: it moves with the name, because the rows are sorted when
        // they are drawn and not by an index that was stored somewhere.
        packs[0].name = "Aardvark".to_string();
        assert_eq!(
            shown_names(&packs),
            vec!["Aardvark", "G-Helper", "Wallpaper Engine"]
        );
    }

    #[test]
    fn the_application_order_is_not_touched_by_the_display_sort() {
        // The shipped packs, as the daemon loads them: the order it applies them in.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let loaded = crate::packs::load(&[root]);
        let orders: Vec<i32> = loaded.iter().map(|p| p.pack.order).collect();
        assert_eq!(
            orders,
            vec![10, 20, 30],
            "the shipped packs keep the order the daemon applies them in"
        );
        let packs: Vec<PackStatus> = loaded
            .iter()
            .map(|p| named_pack(&p.pack.id, &p.pack.name))
            .collect();
        // The rows are shown by name: G-Helper, Rainmeter, Wallpaper Engine.
        assert_eq!(
            shown_names(&packs),
            vec!["G-Helper", "Rainmeter", "Wallpaper Engine"]
        );
        // And the daemon's own order is still there: the display sort works on a copy.
        let order: Vec<&str> = packs.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            order,
            vec!["wallpaper-engine", "g-helper", "rainmeter"],
            "the display sort must not reorder the list the daemon applies the packs from"
        );
    }

    #[test]
    fn the_catalogue_rows_are_shown_by_name() {
        let catalog = vec![
            catalog_entry("wallpaper-engine", "Wallpaper Engine", "1.4.0"),
            catalog_entry("g-helper", "G-Helper", "1.3.1"),
            catalog_entry("rainmeter", "Rainmeter", "1.5.0"),
        ];
        let shown: Vec<&str> = sorted_catalog(&catalog)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(shown, vec!["G-Helper", "Rainmeter", "Wallpaper Engine"]);

        // And the window draws them in that order.
        let (text, _) = install_packs_frame_with(catalog);
        let row = |name: &str| {
            text.iter()
                .find(|(t, _, _)| t == name)
                .map(|(_, rect, _)| *rect)
                .unwrap_or_else(|| panic!("the install-packs window has no {name:?} row"))
        };
        let g_helper = row("G-Helper");
        let rainmeter = row("Rainmeter");
        let wallpaper = row("Wallpaper Engine");
        assert!(
            g_helper.top() < rainmeter.top() && rainmeter.top() < wallpaper.top(),
            "the catalogue rows are not sorted by name: {g_helper:?}, {rainmeter:?}, {wallpaper:?}"
        );
    }

    #[test]
    fn the_console_launcher_is_not_one_of_the_sorted_packs() {
        // The console launcher is a built-in feature and not a pack: it has its own row
        // in the settings window and is not in the list the sort works on.
        let packs = vec![
            named_pack("wallpaper-engine", "Wallpaper Engine"),
            named_pack("g-helper", "G-Helper"),
        ];
        assert!(
            !shown_names(&packs).iter().any(|n| n == CONSOLE_FEATURE),
            "the console launcher must not be a pack"
        );

        // The settings window: the pack rows are sorted by name, and the console
        // launcher sits in its own section below them.
        let text = settings_frame(packs);
        let row = |name: &str| {
            text.iter()
                .find(|(t, _, _)| t == name)
                .map(|(_, rect, _)| *rect)
                .unwrap_or_else(|| panic!("the settings window has no {name:?} row"))
        };
        let g_helper = row("G-Helper");
        let wallpaper = row("Wallpaper Engine");
        let console = row(CONSOLE_FEATURE);
        assert!(
            g_helper.top() < wallpaper.top(),
            "the pack rows are not sorted by name: {g_helper:?} and {wallpaper:?}"
        );
        assert!(
            console.top() > wallpaper.top(),
            "the console launcher belongs in its own section, below the packs: {console:?} and \
             {wallpaper:?}"
        );
    }

    /// The settings window as it is drawn, with the given installed packs.
    fn settings_frame(packs: Vec<PackStatus>) -> Vec<(String, Rect, usize)> {
        let ctx = egui::Context::default();
        app_style(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(420.0, 700.0),
            )),
            ..Default::default()
        };
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut s = shared.lock().unwrap();
            let mut status = status_with_key(false, "");
            status.packs = packs;
            s.status = Some(status);
        }
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| settings_ui(ui, &shared));
        });
        let text = painted_text(&output);
        output.drop_without_applying_deltas();
        text
    }

    #[test]
    fn a_window_size_that_the_os_rounded_counts_as_settled() {
        // 430 pt at 125% is 538 px, which is 430.4 pt again. The window has the size it
        // asked for, so it is settled - even though the two sizes differ by 0.4 pt on
        // each axis, which is more than half a point.
        assert!(size_settled(
            egui::vec2(430.4, 470.4),
            egui::vec2(430.0, 470.0),
            1.25
        ));
        // A window that really is a different size is not settled.
        assert!(!size_settled(
            egui::vec2(400.0, 470.4),
            egui::vec2(430.0, 470.0),
            1.25
        ));
        // At 100% the size matches to the point.
        assert!(size_settled(
            egui::vec2(430.0, 470.0),
            egui::vec2(430.0, 470.0),
            1.0
        ));
    }

    #[test]
    fn the_install_packs_window_is_not_stuck_on_its_size() {
        // The window was created with the size it wants, and Windows rounds a window's
        // size to whole pixels: 430 pt at 125% is 538 px, which is 430.4 pt again. The
        // window has the size it asked for, so it must not sit in the first step until
        // its six-second deadline.
        let ctx = egui::Context::default();
        app_style(&ctx);
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut s = shared.lock().unwrap();
            s.install_packs_open = true;
            s.install_packs_stage = 0;
            // The deadline is far away: the window may not use it to get past the size.
            s.install_packs_deadline = Some(Instant::now() + Duration::from_secs(6));
        }
        let input = egui::RawInput {
            viewports: {
                let mut viewports = egui::ViewportIdMap::default();
                viewports.insert(
                    egui::ViewportId::ROOT,
                    egui::ViewportInfo {
                        inner_rect: Some(Rect::from_min_size(
                            egui::pos2(0.0, 0.0),
                            egui::vec2(INSTALL_PACKS_WIDTH + 0.4, INSTALL_PACKS_HEIGHT + 0.4),
                        )),
                        native_pixels_per_point: Some(1.25),
                        ..Default::default()
                    },
                );
                viewports
            },
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| install_packs_ui(ui, &shared));
        });
        output.drop_without_applying_deltas();
        let stage = shared.lock().unwrap().install_packs_stage;
        assert_eq!(
            stage, 1,
            "the window is still waiting for its size: it is at step {stage}"
        );
    }

    #[test]
    fn the_check_summary_says_what_is_out_of_date() {
        // Everything is current.
        let catalog = Ok(vec![
            catalog_entry("g-helper", "G-Helper", "1.3.1"),
            catalog_entry("rainmeter", "Rainmeter", "1.4.0"),
        ]);
        assert_eq!(
            catalog_check_summary(&catalog, &[installed_pack("g-helper", "1.3.1")]),
            "Last check: 2 packs — all up to date"
        );
        // One pack is behind the repo.
        assert_eq!(
            catalog_check_summary(
                &catalog,
                &[
                    installed_pack("g-helper", "1.3.1"),
                    installed_pack("rainmeter", "1.3.0"),
                ]
            ),
            "Last check: Rainmeter v1.4.0 is available"
        );
        // Two of them are.
        assert_eq!(
            catalog_check_summary(
                &catalog,
                &[
                    installed_pack("g-helper", "1.2.0"),
                    installed_pack("rainmeter", "1.3.0"),
                ]
            ),
            "Last check: 2 packs are out of date: G-Helper v1.3.1, Rainmeter v1.4.0"
        );
        // A pack that is not installed is not out of date either.
        assert_eq!(
            catalog_check_summary(&catalog, &[]),
            "Last check: 2 packs — all up to date"
        );
        // The fetch failed: the error is the result.
        let failed: Result<Vec<CatalogPack>, String> = Err("catalog: no network".to_string());
        assert_eq!(
            catalog_check_summary(&failed, &[]),
            "Last check failed: catalog: no network"
        );
        // An empty repo.
        let empty: Result<Vec<CatalogPack>, String> = Ok(Vec::new());
        assert_eq!(
            catalog_check_summary(&empty, &[]),
            "Last check: the packs repo has no packs."
        );
    }

    #[test]
    fn the_check_summary_leaves_out_a_pack_this_daemon_cannot_install() {
        // The daemon is too old for one of the two packs: it is neither out of date
        // nor a pack this daemon could install.
        let too_new = catalog_entry_needing("too-new", "Too New", "2.0.0", "99.0.0");
        let catalog = Ok(vec![
            catalog_entry("rainmeter", "Rainmeter", "1.4.0"),
            too_new.clone(),
        ]);
        let summary = catalog_check_summary(&catalog, &[installed_pack("rainmeter", "1.3.0")]);
        assert!(summary.contains("Rainmeter v1.4.0"), "the installable pack is named: {summary}");
        assert!(!summary.contains("Too New"), "a refused pack is not named: {summary}");
        assert!(!summary.contains("2.0.0"), "a refused pack is not named: {summary}");
        assert!(!summary.contains("out of date"), "a refused pack is not out of date: {summary}");
        // Nothing is out of date: the refused pack is not counted either.
        let current = catalog_check_summary(&catalog, &[installed_pack("rainmeter", "1.4.0")]);
        assert!(current.contains("all up to date"), "the installable pack is current: {current}");
        assert!(!current.contains("2 packs"), "the refused pack is not counted: {current}");
        // Nothing at all can be installed here: the summary says so instead of claiming
        // the repo is up to date.
        let all_refused = Ok(vec![too_new]);
        let summary = catalog_check_summary(&all_refused, &[]);
        assert!(!summary.contains("all up to date"), "it is not up to date: {summary}");
        assert!(summary.contains("none can be installed"), "it says why: {summary}");
    }

    #[test]
    fn the_two_packs_buttons_fit_on_one_row() {
        // The settings window is as wide as the dashboard (418 pt) and the frame's padding
        // takes 20 pt of it: the row has 398 pt.
        const ROW_WIDTH: f32 = 398.0;
        let ctx = egui::Context::default();
        let chosen = app_style(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(ROW_WIDTH, 200.0),
            )),
            ..Default::default()
        };
        let mut rows: Vec<f32> = Vec::new();
        let mut widths: Vec<f32> = Vec::new();
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        // The gap the packs row uses.
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // The catalogue is checked in the install-packs window, so the
                        // settings window has two buttons and not three.
                        for label in ["Install packs", "Open packs folder"] {
                            let rect = ui.button(label).rect;
                            rows.push(rect.top());
                            widths.push(rect.width());
                        }
                    });
                });
        });
        output.drop_without_applying_deltas();
        assert_eq!(rows.len(), 2);
        if chosen.is_some() {
            // The system font is the one the app draws with: the two buttons have to
            // share one row, otherwise the row wraps and the panel grows.
            let total: f32 = widths.iter().sum::<f32>() + 6.0;
            assert!(
                (rows[0] - rows[1]).abs() < 0.5,
                "the two packs buttons do not fit on one row: {widths:?} ({total:.0} pt) in \
                 {ROW_WIDTH} pt (rows at {rows:?})"
            );
        }
    }

    #[test]
    fn the_download_button_says_what_the_next_step_is() {
        // Not installed: download it.
        assert_eq!(pack_button("1.4.0", None), PackButton::Download);
        // A newer version is available: update it.
        assert_eq!(pack_button("1.4.0", Some("1.3.1")), PackButton::Update);
        assert_eq!(pack_button("1.10.0", Some("1.9.9")), PackButton::Update);
        // The installed version is the current one: nothing to do.
        assert_eq!(
            pack_button("1.4.0", Some("1.4.0")),
            PackButton::Installed("1.4.0".to_string())
        );
        // The installed one is ahead of the catalog: still nothing to do.
        assert_eq!(
            pack_button("1.3.1", Some("1.4.0")),
            PackButton::Installed("1.4.0".to_string())
        );
    }

    #[test]
    fn the_install_packs_window_joins_the_chain_at_the_left_end() {
        // 2560x1380 px work area at 125% -> 2048 x 1104 pt on the primary monitor.
        let work = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let core = Rect::from_min_max(egui::pos2(1668.0, 886.0), egui::pos2(2048.0, 1104.0));
        let ppp = 1.25;
        let size = egui::vec2(INSTALL_PACKS_WIDTH, INSTALL_PACKS_HEIGHT);
        let (work_px, core_left_px, core_bottom_px, chain) = chain_px(
            work,
            core,
            &[
                (ChainItem::Settings, egui::vec2(420.0, 500.0)),
                (ChainItem::InstallPacks, size),
            ],
            ppp,
        );
        let out = reanchor_chain(work_px, core_left_px, core_bottom_px, &chain);
        // The settings is immediately left of the dashboard.
        assert!((out[0].1.x - 1555.0).abs() < 1e-3);
        // 1555 - 5 - 538 px: the install-packs window touches the settings.
        assert!((out[1].1.x - 1012.0).abs() < 1e-3);
        // The order from the right: dashboard, settings, install packs.
        assert!(core_left_px > out[0].1.x && out[0].1.x > out[1].1.x);
        // The whole chain stands on the dashboard's bottom edge.
        assert!((out[1].1.y + 588.0 - core_bottom_px).abs() < 1e-3);
        // The chain fits the work area.
        assert!(out[1].1.x >= work_px.left() && out[1].1.x + 538.0 <= work_px.right());
        assert!(out[1].1.y >= work_px.top() && out[1].1.y + 588.0 <= work_px.bottom());
    }

    #[test]
    fn closing_the_install_packs_window_takes_it_out_of_the_chain() {
        let mut s = Shared::default();
        s.install_packs_open = true;
        s.chain.push(ChainItem::Settings);
        s.chain.push(ChainItem::InstallPacks);
        s.chain_place.insert(ChainItem::InstallPacks, Placement::default());
        close_install_packs(&mut s);
        assert!(!s.install_packs_open);
        // The settings stays in the chain, the install-packs window is gone.
        assert_eq!(s.chain, vec![ChainItem::Settings]);
        assert!(!s.chain_place.contains_key(&ChainItem::InstallPacks));
    }

    /// The app's own fonts and styles, so a test measures the text as it is drawn.
    /// Returns the name of the system font, or None when the machine has none.
    fn app_style(ctx: &egui::Context) -> Option<&'static str> {
        let (fonts, chosen) = system_fonts();
        ctx.set_fonts(fonts);
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
        chosen
    }

    /// Every piece of text a frame painted: what it says, where it landed and how many
    /// rows it took.
    fn painted_text(output: &egui::FullOutput) -> Vec<(String, Rect, usize)> {
        output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(t) => Some((
                    t.galley.job.text.clone(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                    t.galley.rows.len(),
                )),
                _ => None,
            })
            .collect()
    }

    /// The horizontal rules a frame painted, from the top down. The X of a close
    /// button is drawn with two slanting lines, so it is not one of them.
    fn painted_rules(output: &egui::FullOutput) -> Vec<f32> {
        let mut ys: Vec<f32> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::LineSegment { points, .. }
                    if (points[0].y - points[1].y).abs() < 0.5 =>
                {
                    Some(points[0].y)
                }
                _ => None,
            })
            .collect();
        ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        ys
    }

    #[test]
    fn the_settings_window_opens_with_one_rule() {
        // The four status rows are gone, and so is the rule that framed them: the
        // window opens with the title bar's own rule and straight into the content.
        let ctx = egui::Context::default();
        app_style(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(420.0, 500.0),
            )),
            ..Default::default()
        };
        let shared = Arc::new(Mutex::new(Shared::default()));
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| settings_ui(ui, &shared));
        });
        let text = painted_text(&output);
        let revert = text
            .iter()
            .find(|(t, _, _)| t == "Revert after")
            .map(|(_, rect, _)| *rect)
            .expect("the settings window has no 'Revert after' row");
        let rules_above = painted_rules(&output)
            .into_iter()
            .filter(|y| *y < revert.top())
            .count();
        output.drop_without_applying_deltas();
        assert_eq!(
            rules_above, 1,
            "the settings window opens with {rules_above} rules above the first row, not one"
        );
    }

    #[test]
    fn every_catalog_row_shows_its_state_on_screen() {
        // The state of a pack (the "vX installed" label, the update button or the
        // download button) is at the right of the row, and the description under the
        // name wraps into two or three rows. A description that ran the full width of
        // the window used to widen the whole list, and that pushed the state of the
        // rows below it off the screen.
        let ctx = egui::Context::default();
        app_style(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(INSTALL_PACKS_WIDTH, INSTALL_PACKS_HEIGHT),
            )),
            ..Default::default()
        };
        let shared = Arc::new(Mutex::new(Shared::default()));
        let entries = vec![
            // The installed version is the one in the repo: the row says so.
            catalog_entry_with_description(
                "wallpaper-engine",
                "Wallpaper Engine",
                "1.4.0",
                "Stops the animated wallpaper while a tracked app runs and restores it afterwards.",
            ),
            // The repo has a newer one: the row offers the update.
            catalog_entry_with_description(
                "g-helper",
                "G-Helper",
                "1.3.1",
                "Switches G-Helper's performance mode (Turbo / Balanced / Silent) while a tracked app runs.",
            ),
            // Not installed at all: the row offers the download.
            catalog_entry_with_description(
                "rainmeter",
                "Rainmeter",
                "1.4.0",
                "Swaps Rainmeter to the dropdown's layout while a tracked app runs and back again.",
            ),
        ];
        let packs = vec![
            installed_pack("wallpaper-engine", "1.4.0"),
            installed_pack("g-helper", "1.2.0"),
        ];
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    for entry in &entries {
                        catalog_row(ui, &shared, entry, &packs, &[]);
                    }
                });
        });
        let text = painted_text(&output);
        let state = |want: &str| {
            text.iter()
                .find(|(t, _, _)| t == want)
                .map(|(_, rect, _)| *rect)
        };
        // The installed version is the one in the repo: the row says so.
        let installed = state("v1.4.0 installed");
        // A newer version in the repo: the row offers the update.
        let update = state("Update");
        // Not installed: the row offers the download.
        let download = state("Download");
        // The description wraps: two or three rows, and it is indented under the name.
        let description = text
            .iter()
            .find(|(t, _, _)| t.starts_with("Stops the animated wallpaper"))
            .map(|(_, rect, rows)| (*rect, *rows));
        let off_window: Vec<String> = text
            .iter()
            .filter(|(_, rect, _)| rect.right() > INSTALL_PACKS_WIDTH + 0.5)
            .map(|(t, rect, _)| format!("{t:?} at {rect:?}"))
            .collect();
        output.drop_without_applying_deltas();
        // Nothing runs off the window: every row is fully on screen.
        assert!(
            off_window.is_empty(),
            "these run off the window: {off_window:?}"
        );
        let installed = installed.expect("no row says the pack is installed");
        assert!(update.is_some(), "no row offers the update");
        assert!(download.is_some(), "no row offers the download");
        // The label is against the right edge of the window, where it belongs.
        assert!(
            (installed.right() - INSTALL_PACKS_WIDTH).abs() < 0.5,
            "the installed label is not at the right edge: {installed:?}"
        );
        let (rect, rows) = description.expect("the description is not on screen");
        assert!(
            (2..=3).contains(&rows),
            "the description is {rows} rows, not two or three"
        );
        assert!(rect.left() > 0.0, "the description is not indented");
    }

    /// The install-packs window as it is drawn, with a catalog of `count` packs.
    /// Returns the painted text, the rules and the window's frame.
    fn install_packs_frame(count: usize) -> (Vec<(String, Rect, usize)>, Vec<f32>) {
        let entries: Vec<CatalogPack> = (0..count)
            .map(|i| {
                catalog_entry_with_description(
                    &format!("pack-{i}"),
                    &format!("Pack number {i}"),
                    "1.4.0",
                    "Stops the animated wallpaper while a tracked app runs and restores it \
                     afterwards.",
                )
            })
            .collect();
        install_packs_frame_with(entries)
    }

    /// The install-packs window as it is drawn, with the given catalogue.
    fn install_packs_frame_with(entries: Vec<CatalogPack>) -> (Vec<(String, Rect, usize)>, Vec<f32>) {
        let ctx = egui::Context::default();
        app_style(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(INSTALL_PACKS_WIDTH, INSTALL_PACKS_HEIGHT),
            )),
            ..Default::default()
        };
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut s = shared.lock().unwrap();
            s.catalog = Some(Ok(entries));
        }
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| install_packs_ui(ui, &shared));
        });
        let text = painted_text(&output);
        let rules = painted_rules(&output);
        output.drop_without_applying_deltas();
        (text, rules)
    }

    #[test]
    fn the_local_pack_button_is_inside_the_window() {
        // The catalog list used to claim its full height even when its rows needed
        // less, and the "Local pack" button below it fell out of the window. The
        // button has to be on screen, whatever the catalog holds.
        for count in [0usize, 3, 12] {
            let (text, rules) = install_packs_frame(count);
            let button = text
                .iter()
                .find(|(t, _, _)| t == "Local pack")
                .map(|(_, rect, _)| *rect)
                .expect("the install-packs window has no 'Local pack' button");
            // The whole label is inside the window, padding included.
            assert!(
                button.top() >= PAD_F && button.bottom() <= INSTALL_PACKS_HEIGHT - PAD_F,
                "the 'Local pack' button is not inside the window with {count} packs: {button:?}"
            );
            // The rule above the button sits between the last row and the button.
            let rule = rules
                .last()
                .copied()
                .expect("the install-packs window has no rule above the button");
            assert!(
                rule < button.top(),
                "the rule is not above the 'Local pack' button with {count} packs: {rule} vs \
                 {button:?}"
            );
            // The button hugs the rule: no dead space between the two.
            assert!(
                button.top() - rule < 30.0,
                "the 'Local pack' button has drifted away from the rule with {count} packs: \
                 {button:?} vs {rule}"
            );
            // Nothing is pushed above the rule either: the list ends where the rows end.
            let last_row = text
                .iter()
                .filter(|(t, _, _)| t.starts_with("Pack number") || t.starts_with("Stops the"))
                .map(|(_, rect, _)| rect.bottom())
                .fold(0.0_f32, f32::max);
            if count > 0 {
                assert!(
                    rule - last_row < 40.0,
                    "there is dead space above the rule with {count} packs: {rule} vs {last_row}"
                );
            }
        }
    }

    #[test]
    fn a_long_catalog_scrolls_inside_the_list() {
        // The list has a fixed room: a catalog that does not fit scrolls inside it,
        // and the packs below the fold are not on screen.
        let (text, _) = install_packs_frame(12);
        let shown = text
            .iter()
            .filter(|(t, _, _)| t.starts_with("Pack number"))
            .count();
        assert!(
            shown > 0 && shown < 12,
            "the catalog does not scroll: {shown} of 12 packs are on screen"
        );
    }

    #[test]
    fn the_picker_says_how_many_matches_it_did_not_show() {
        // A list that reads as complete but is not is a lie: the count is said.
        assert_eq!(more_matches_note(8, APP_MATCH_ROWS), None);
        assert_eq!(more_matches_note(3, APP_MATCH_ROWS), None);
        assert_eq!(
            more_matches_note(9, APP_MATCH_ROWS).as_deref(),
            Some("… 1 more match — type to narrow the list")
        );
        assert_eq!(
            more_matches_note(20, APP_MATCH_ROWS).as_deref(),
            Some("… 12 more matches — type to narrow the list")
        );
    }

    #[test]
    fn closing_the_settings_window_takes_it_out_of_the_chain() {
        let mut s = Shared {
            settings_open: true,
            ..Default::default()
        };
        s.chain.push(ChainItem::Settings);
        s.chain.push(ChainItem::InstallPacks);
        s.chain_place.insert(ChainItem::Settings, Placement::default());
        close_settings(&mut s);
        assert!(!s.settings_open);
        // The install-packs window stays in the chain, the settings window is gone.
        assert_eq!(s.chain, vec![ChainItem::InstallPacks]);
        assert!(!s.chain_place.contains_key(&ChainItem::Settings));
    }

    #[test]
    fn a_window_the_root_stops_showing_is_closed() {
        // The ✕ of the settings window does not tell the OS to close the window: it
        // stops the root from showing it, and egui closes a child viewport that was
        // not used in a pass. This is that rule, exercised.
        let ctx = egui::Context::default();
        // eframe does this on a platform that has real windows; without it egui
        // embeds the child window in the root one.
        ctx.set_embed_viewports(false);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(420.0, 500.0),
            )),
            ..Default::default()
        };
        let id = settings_viewport_id();
        // The root shows the window.
        let output = ctx.run_ui(input.clone(), |_ui| {
            ctx.show_viewport_deferred(
                id,
                egui::ViewportBuilder::default().with_title("Settings"),
                |_ui, _class| {},
            );
        });
        output.drop_without_applying_deltas();
        let title = ctx.viewport_for(id, |state| state.builder.title.clone());
        assert_eq!(
            title.as_deref(),
            Some("Settings"),
            "the settings window was never opened"
        );
        // The window is closed: the root stops showing it.
        let output = ctx.run_ui(input, |_ui| {});
        output.drop_without_applying_deltas();
        let title = ctx.viewport_for(id, |state| state.builder.title.clone());
        assert!(title.is_none(), "the settings window is still open");
    }

    /// Wait until the worker has published a list (or the time is up).
    fn wait_for_apps(slot: &AppSlot, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if read_apps(slot).is_some() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    #[test]
    fn the_running_app_list_hands_over_the_latest_value() {
        // The slot holds one snapshot, never a queue: a new list replaces the old one,
        // so a slow egui thread cannot make the picker fall behind.
        let slot: AppSlot = Arc::new(Mutex::new(None));
        // Nothing has landed yet: the picker says the list is still being built.
        assert!(read_apps(&slot).is_none());
        assert_eq!(no_match_message(false), "Looking for running apps…");
        publish_apps(
            &slot,
            vec![("first".to_string(), "C:\\first.exe".to_string())],
        );
        publish_apps(
            &slot,
            vec![("second".to_string(), "C:\\second.exe".to_string())],
        );
        let latest = read_apps(&slot).expect("the list has landed");
        assert_eq!(latest.len(), 1, "the slot holds one snapshot, not a queue");
        assert_eq!(latest[0].1, "C:\\second.exe", "the latest value wins");
        assert_eq!(
            no_match_message(true),
            "No running app matches — it has to be open, or use Browse…"
        );
    }

    #[test]
    fn the_running_app_worker_publishes_the_list_and_stops() {
        // The list is built off the egui thread: the worker publishes it by itself, and
        // the UI only reads it.
        let slot: AppSlot = Arc::new(Mutex::new(None));
        let mut worker = AppListWorker::start(&slot);
        assert!(
            wait_for_apps(&slot, Duration::from_secs(10)),
            "the worker published no list"
        );
        // The UI closes: the worker must end instead of lingering.
        worker.stop();
        assert!(
            worker.wait_stopped(Duration::from_secs(5)),
            "the worker did not stop"
        );
    }

    /// A stand-in for the packs repo: it answers every request with the same bytes, after
    /// a delay. The catalogue tests are the only place a network is used, and this is the
    /// only network they use.
    struct TestRepo {
        /// The catalogue manifest URL. The pack files hang under it.
        url: String,
    }

    impl TestRepo {
        fn start(body: Vec<u8>, delay: Duration) -> TestRepo {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("the test repo must bind");
            let port = listener
                .local_addr()
                .expect("the test repo must have an address")
                .port();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let body = body.clone();
                    thread::spawn(move || {
                        // The request head is read and ignored: the answer is the same for
                        // every path.
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
                            match stream.read(&mut byte) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => head.push(byte[0]),
                            }
                        }
                        thread::sleep(delay);
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(&body);
                    });
                }
            });
            TestRepo {
                url: format!("http://127.0.0.1:{port}/catalog/index.json"),
            }
        }
    }

    /// A pack id of this test's own, so the temp file of one test never touches another's.
    fn test_pack_id(name: &str) -> String {
        format!("ui-test-{name}-{}", std::process::id())
    }

    /// An entry that promises exactly the bytes the test repo serves.
    fn entry_for(body: &[u8], id: &str) -> CatalogPack {
        let mut entry = catalog_entry(id, id, "1.0.0");
        entry.size = Some(body.len() as u64);
        entry.sha256 = catalog::hex_sha256(body);
        entry
    }

    /// Ask for one pack and wait until the download has ended.
    fn download_from(repo: &TestRepo, entry: &CatalogPack) -> Arc<Mutex<Shared>> {
        let shared: Arc<Mutex<Shared>> = Arc::new(Mutex::new(Shared::default()));
        let ctx = egui::Context::default();
        download_pack_in_background(&shared, entry, &repo.url, &ctx);
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let done = {
                let s = shared.lock().unwrap();
                s.last_pack_op.is_some() || !s.catalog_install_path.is_empty()
            };
            if done {
                return shared;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("the download never ended");
    }

    /// Wait until the queue of verified files holds `count` of them.
    fn wait_for_files(shared: &Arc<Mutex<Shared>>, count: usize) -> Vec<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let landed = {
                let s = shared.lock().unwrap();
                s.catalog_install_path.len()
            };
            if landed >= count {
                return shared.lock().unwrap().catalog_install_path.clone();
            }
            if Instant::now() >= deadline {
                panic!("only {landed} of {count} downloads landed");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_catalogue_download_does_not_hold_up_the_thread_that_asked_for_it() {
        // The pack takes a moment to come down. The call that asks for it must come back
        // long before that: the thread that draws the window may not wait for the network.
        let body = b"the bytes the manifest promises".to_vec();
        let repo = TestRepo::start(body.clone(), Duration::from_millis(700));
        let id = test_pack_id("slow");
        let entry = entry_for(&body, &id);

        let shared: Arc<Mutex<Shared>> = Arc::new(Mutex::new(Shared::default()));
        let ctx = egui::Context::default();
        let started = Instant::now();
        download_pack_in_background(&shared, &entry, &repo.url, &ctx);
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(300),
            "the caller waited for the download: {waited:?}"
        );
        // The pack is on its way: the row says so before the download ends.
        assert_eq!(shared.lock().unwrap().catalog_installing, vec![id.clone()]);
        // And the download ends on its own, on the worker.
        let paths = wait_for_files(&shared, 1);
        assert_eq!(
            std::fs::read(&paths[0]).expect("the verified file"),
            body,
            "the file holds the bytes that were served"
        );
        let _ = std::fs::remove_file(&paths[0]);
    }

    #[test]
    fn a_pack_whose_size_does_not_match_the_manifest_is_not_installed() {
        // The manifest promises a size the bytes on the wire do not have.
        let body = b"the bytes the wire carries".to_vec();
        let repo = TestRepo::start(body.clone(), Duration::from_millis(0));
        let id = test_pack_id("size");
        let mut entry = entry_for(&body, &id);
        entry.size = Some(body.len() as u64 + 1);

        let shared = download_from(&repo, &entry);
        let s = shared.lock().unwrap();
        let (ok, message) = s.last_pack_op.clone().expect("the failure reaches the user");
        assert!(!ok, "the download failed: {message}");
        assert!(message.contains("size"), "the message says what is wrong: {message}");
        assert!(s.catalog_install_path.is_empty(), "nothing waits for the daemon");
        assert!(
            s.catalog_installing.is_empty(),
            "the row does not stay on 'installing…'"
        );
        drop(s);
        assert!(!catalog_temp_path(&id).exists(), "nothing was written");
    }

    #[test]
    fn a_pack_whose_hash_does_not_match_the_manifest_is_not_installed() {
        // The manifest promises a hash the bytes on the wire do not have.
        let body = b"the bytes the wire carries".to_vec();
        let repo = TestRepo::start(body.clone(), Duration::from_millis(0));
        let id = test_pack_id("hash");
        let mut entry = entry_for(&body, &id);
        entry.sha256 = "0".repeat(64);

        let shared = download_from(&repo, &entry);
        let s = shared.lock().unwrap();
        let (ok, message) = s.last_pack_op.clone().expect("the failure reaches the user");
        assert!(!ok, "the download failed: {message}");
        assert!(message.contains("sha256"), "the message says what is wrong: {message}");
        assert!(s.catalog_install_path.is_empty(), "nothing waits for the daemon");
        assert!(
            s.catalog_installing.is_empty(),
            "the row does not stay on 'installing…'"
        );
        drop(s);
        assert!(!catalog_temp_path(&id).exists(), "nothing was written");
    }

    #[test]
    fn a_second_pack_does_not_throw_the_first_one_away() {
        // Two packs are asked for while the first one is still coming down. Both are on
        // their way, and both files wait for the daemon: the second one must not push the
        // first one out of the way.
        let body = b"the bytes the manifest promises".to_vec();
        let repo = TestRepo::start(body.clone(), Duration::from_millis(0));
        let first = entry_for(&body, &test_pack_id("first"));
        let second = entry_for(&body, &test_pack_id("second"));

        let shared: Arc<Mutex<Shared>> = Arc::new(Mutex::new(Shared::default()));
        let ctx = egui::Context::default();
        download_pack_in_background(&shared, &first, &repo.url, &ctx);
        download_pack_in_background(&shared, &second, &repo.url, &ctx);
        assert_eq!(
            shared.lock().unwrap().catalog_installing,
            vec![first.id.clone(), second.id.clone()],
            "both packs are on their way"
        );
        let paths = wait_for_files(&shared, 2);
        assert_eq!(paths.len(), 2, "both downloads are kept");
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn the_temp_file_of_a_pack_cannot_leave_the_temp_folder() {
        // The id comes from the network, so it is not trusted with the file system.
        let path = catalog_temp_path("../../Windows/System32/evil");
        assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
            "the name holds nothing a path can walk: {path:?}"
        );
    }
}
