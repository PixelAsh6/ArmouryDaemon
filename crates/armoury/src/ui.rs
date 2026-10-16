//! UI mode (`--ui`): the dashboard window plus a settings viewport.
//! Undecorated (custom title bar: title, settings gear, close), auto-fitting,
//! placed at the bottom exterior corner of the primary monitor, above the taskbar.
//! A second launch of the same exe carries the daemon's pipe traffic.

use armoury_core::protocol::{read_msg, write_msg, Request, Response};
use armoury_core::status::{RunState, StatusSnapshot};
use crossbeam_channel::Receiver;
use eframe::egui::{self, Color32, FontFamily, FontId, Rect, RichText, Stroke, TextStyle};
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SETTINGS_VIEWPORT: &str = "armoury-settings";
const MIN_WINDOW_WIDTH: f32 = 380.0;
const MIN_SETTINGS_WIDTH: f32 = 300.0;
/// Inner padding of every window (text must never hug the border).
const PAD: i8 = 10;
const PAD_F: f32 = PAD as f32;

enum UiEvent {
    Response(Response),
    Eof,
}

#[derive(Default)]
struct Shared {
    status: Option<StatusSnapshot>,
    connected: bool,
    daemon_version: String,
    monitors: usize,
    /// Outer rect (logical points) of the core window, for stacking the settings above it.
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
            let app: Box<dyn eframe::App> = Box::new(App::new(rx, shared));
            Ok(app)
        }),
    );
}

struct App {
    rx: Receiver<UiEvent>,
    shared: Arc<Mutex<Shared>>,
    styled: bool,
    last_desired: Option<egui::Vec2>,
}

impl App {
    fn new(rx: Receiver<UiEvent>, shared: Arc<Mutex<Shared>>) -> Self {
        Self {
            rx,
            shared,
            styled: false,
            last_desired: None,
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            self.styled = true;
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
                UiEvent::Eof => {
                    log::info!("daemon went away — exiting");
                    std::process::exit(0);
                }
            }
        }

        // Keep the window live: pipe updates arrive at ~1 Hz.
        ctx.request_repaint_after(Duration::from_secs(1));
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
        let (status_text, status_color, rows, settings_open) = {
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
                ("packs".to_string(), "none installed".to_string()),
            ];
            (text, color, rows, s.settings_open)
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
                    if icon_button(ui, 22.0, draw_gear).clicked() {
                        let mut s = self.shared.lock().unwrap();
                        s.settings_open = !settings_open;
                        if s.settings_open {
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
            ui.horizontal(|ui| {
                status_dot(ui, status_color);
                ui.label(RichText::new(&status_text).strong().size(17.0));
            });
            ui.add_space(8.0);
            for (key, value) in &rows {
                row(ui, key, value);
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
                    .with_inner_size([MIN_SETTINGS_WIDTH, 250.0]),
                move |ui, _class| settings_ui(ui, &shared),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Settings window

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
        let st = s.status.as_ref().map(|st| match st.state {
            RunState::Idle => "Idle".to_string(),
            RunState::Triggered => format!(
                "Triggered ({})",
                st.trigger_source.as_deref().unwrap_or("?")
            ),
            RunState::Cooldown => format!(
                "Cooldown ({}s left)",
                st.cooldown_remaining_ms.unwrap_or(0) / 1000
            ),
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

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        if ui.button("Open config folder").clicked() {
            open_folder(&config_dir);
        }
        ui.add_space(4.0);
        ui.small("Full settings UI arrives in Phase 6.");
    });

    // ---- shrink-wrap, position above the core window, then show ----
    let title_font = FontId::new(16.0, FontFamily::Proportional);
    let body_font = TextStyle::Body.resolve(ui.style());
    let desired = {
        let p = ui.painter();
        let tw = |f: &FontId, t: &str| p.layout_no_wrap(t.to_owned(), f.clone(), Color32::WHITE).rect.width();
        let mut w = tw(&title_font, "Settings") + 60.0;
        w = w.max(tw(&body_font, "Open config folder") + 24.0);
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
    match s.settings_stage {
        0 => {
            let cur = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            let settled = cur.map_or(false, |c| (c - desired).length() <= 0.5);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
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
                log::info!("settings window shown");
            } else if let Some(t) = s.settings_target {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(t));
            }
        }
        _ => {
            // Follow the dashboard: whenever its rect (or our own size) changes, re-stack
            // above it so the gap stays constant. A plain drag of this window changes
            // neither, so the user can still move it freely.
            if let Some(core) = s.core_rect {
                if s.settings_anchor != Some((core, desired)) {
                    let pos = above_core(ctx, core, desired);
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                    s.settings_target = Some(pos);
                    s.settings_anchor = Some((core, desired));
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
    if let Err(e) = std::process::Command::new("explorer").arg(path).spawn() {
        log::warn!("failed to open {}: {e}", path.display());
    }
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

/// Position a window above the core window, right edges aligned.
fn above_core(ctx: &egui::Context, core: Rect, size: egui::Vec2) -> egui::Pos2 {
    let gap = 8.0;
    let x = core.right() - size.x;
    let mut y = core.top() - gap - size.y;
    if let Some((left, top, right, _bottom)) = work_area_pts(ctx) {
        let margin = 6.0;
        if y < top + margin {
            y = core.bottom() + gap; // fall back to below the core
        }
        let x = x.max(left + margin).min(right - size.x - margin);
        return egui::pos2(x, y);
    }
    egui::pos2(x, y)
}
