//! ArmouryDaemon UI — on-demand process.
//! Phase 1: one auto-fitting, draggable-by-body window placed bottom-left above the taskbar;
//! connects to the daemon over the named pipe and displays live status. Exits when closed.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use armoury_core::protocol::{read_msg, write_msg, Request, Response};
use armoury_core::status::{RunState, StatusSnapshot};
use crossbeam_channel::Receiver;
use eframe::egui;
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced};
use std::io::BufReader;
use std::thread;
use std::time::Duration;

enum UiEvent {
    Response(Response),
    Eof,
}

fn main() -> eframe::Result {
    if let Err(e) = armoury_core::logging::init("ui") {
        eprintln!("logging init failed: {e}");
    }
    log::info!("ui starting (v{})", armoury_core::VERSION);

    // ---- connect to the daemon (retry briefly: the daemon spawns us and may still be settling) ----
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

    let (icon_rgba, icon_w, icon_h) = armoury_core::icon::rgba_icon(32);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ArmouryDaemon")
            .with_inner_size([260.0, 180.0])
            .with_resizable(false)
            .with_visible(false)
            .with_icon(egui::IconData {
                rgba: icon_rgba,
                width: icon_w,
                height: icon_h,
            }),
        ..Default::default()
    };

    eframe::run_native(
        "ArmouryDaemon",
        options,
        Box::new(move |_cc| {
            let app: Box<dyn eframe::App> = Box::new(App::new(rx));
            Ok(app)
        }),
    )
}

struct App {
    rx: Receiver<UiEvent>,
    status: Option<StatusSnapshot>,
    daemon_version: String,
    connected: bool,
    settled: bool,
    place_pending: bool,
    frames: u32,
}

impl App {
    fn new(rx: Receiver<UiEvent>) -> Self {
        Self {
            rx,
            status: None,
            daemon_version: String::new(),
            connected: false,
            settled: false,
            place_pending: false,
            frames: 0,
        }
    }
}

impl eframe::App for App {
    /// Non-drawing per-frame logic: drain pipe events, react to daemon messages.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::Response(Response::HelloAck {
                    daemon_version,
                    status,
                }) => {
                    self.daemon_version = daemon_version;
                    self.status = Some(status);
                    self.connected = true;
                }
                UiEvent::Response(Response::Status { status }) => {
                    self.status = Some(status);
                    self.connected = true;
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

        // Whole-body drag (bottom layer; interactive widgets added later sit on top).
        let bg = ui.interact(
            ui.max_rect(),
            ui.scope_id().with("bg-drag"),
            egui::Sense::click_and_drag(),
        );
        if bg.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        // ---- content ----
        ui.add_space(2.0);
        ui.heading("ArmouryDaemon");
        ui.add_space(6.0);

        let (state_text, color) = match &self.status {
            Some(s) => match s.state {
                RunState::Idle => ("Idle".to_string(), egui::Color32::from_rgb(52, 211, 153)),
                RunState::Triggered => (
                    match &s.trigger_source {
                        Some(src) => format!("Triggered — {src}"),
                        None => "Triggered".to_string(),
                    },
                    egui::Color32::from_rgb(251, 191, 36),
                ),
                RunState::Cooldown => (
                    match s.cooldown_remaining_ms {
                        Some(ms) => format!("Cooldown — {}s left", ms / 1000),
                        None => "Cooldown".to_string(),
                    },
                    egui::Color32::from_rgb(96, 165, 250),
                ),
            },
            None => ("Waiting for daemon…".to_string(), egui::Color32::GRAY),
        };
        ui.horizontal(|ui| {
            status_dot(ui, color);
            ui.strong(&state_text);
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        let version_text = if self.connected {
            format!("v{}", self.daemon_version)
        } else {
            "offline".to_string()
        };
        let mut rows: Vec<(String, String)> = vec![("daemon".to_string(), version_text)];
        if let Some(s) = &self.status {
            rows.push(("uptime".to_string(), fmt_uptime(s.uptime_secs)));
            rows.push((
                "cooldown".to_string(),
                match s.cooldown_remaining_ms {
                    Some(ms) => format!("{}s", ms / 1000),
                    None => "—".to_string(),
                },
            ));
            rows.push(("packs".to_string(), "none installed".to_string()));
        }
        for (key, value) in &rows {
            row(ui, key, value);
        }

        // ---- shrink-wrap auto-fit (intrinsic width from text metrics), then place ----
        let desired_w = measured_width(ui, &state_text, &rows);
        let content = egui::vec2(
            (desired_w + 24.0).max(190.0),
            (ui.min_rect().height() + 20.0).max(110.0),
        );
        let ctx = ui.ctx();
        if !self.settled {
            self.frames += 1;
            let current = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            match current {
                Some(cur) if (cur - content).length() <= 0.5 => {
                    self.settled = true;
                    self.place_pending = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                Some(_) if self.frames < 240 => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(content));
                }
                Some(_) => {
                    // safety valve: stop resizing and show what we have
                    self.settled = true;
                    self.place_pending = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(content));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                None => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(content));
                }
            }
        } else if self.place_pending {
            if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
                place_bottom_left(ctx, outer);
                self.place_pending = false;
                log::info!(
                    "window settled and placed (content {}x{})",
                    content.x as i32,
                    content.y as i32
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------

fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.small(key);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.small(value);
        });
    });
}

/// Intrinsic content width: measured from text metrics, independent of the current window size.
fn measured_width(ui: &egui::Ui, status_text: &str, rows: &[(String, String)]) -> f32 {
    let heading_font = egui::TextStyle::Heading.resolve(ui.style());
    let body_font = egui::TextStyle::Body.resolve(ui.style());
    let small_font = egui::TextStyle::Small.resolve(ui.style());
    let painter = ui.painter();
    let text_w = |font: &egui::FontId, text: &str| -> f32 {
        painter
            .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE)
            .rect
            .width()
    };

    let mut w = text_w(&heading_font, "ArmouryDaemon");
    w = w.max(18.0 + text_w(&body_font, status_text)); // dot + spacing + text
    for (key, value) in rows {
        w = w.max(text_w(&small_font, key) + 28.0 + text_w(&small_font, value)); // gap between key and value
    }
    w
}

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

/// Bottom-left of the primary monitor's work area, margins as % of the work area.
fn place_bottom_left(ctx: &egui::Context, outer: egui::Rect) {
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
        log::warn!("SPI_GETWORKAREA failed; leaving default position");
        return;
    }

    let ppp = ctx.pixels_per_point().max(0.5) as f32;
    let left = work.left as f32 / ppp;
    let right = work.right as f32 / ppp;
    let top = work.top as f32 / ppp;
    let bottom = work.bottom as f32 / ppp;
    let work_w = right - left;
    let work_h = bottom - top;

    let margin_x = (work_w * 0.005).max(6.0);
    let margin_y = (work_h * 0.005).max(6.0);

    let x = left + margin_x;
    let y = bottom - outer.height() - margin_y;
    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
}
