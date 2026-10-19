//! Console launcher (`--console`): the pad-driven tile strip the daemon spawns on a fresh
//! controller connect (the tray menu and the settings preview can open it too).
//!
//! Its own process, like `--ui`: it connects to the daemon over the same JSONL pipe, receives
//! status broadcasts (which carry the app lists) and sends launch / add requests back.
//! Input is XInput polled per frame; transitions ride egui's `animate_*` helpers.

use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use armoury_core::protocol::{read_msg, write_msg, Request, Response};
use armoury_core::status::StatusSnapshot;
use crossbeam_channel::Receiver;
use eframe::egui;
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced};

use crate::pad;

/// The launcher's fixed window: a bottom-anchored band, console style.
const WIN_W: f32 = 1200.0;
const WIN_H: f32 = 360.0;
const TILE_W: f32 = 196.0;
const TILE_H: f32 = 240.0;
const GAP: f32 = 18.0;
const SIDE: f32 = 48.0;
/// How far above the work-area bottom the band sits.
const BOTTOM_LIFT: f32 = 56.0;

const BG: egui::Color32 = egui::Color32::from_rgb(9, 10, 13);
const TILE: egui::Color32 = egui::Color32::from_rgb(24, 26, 32);
const TILE_SEL: egui::Color32 = egui::Color32::from_rgb(38, 42, 54);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(92, 148, 255);
const TEXT: egui::Color32 = egui::Color32::from_rgb(232, 236, 244);
const MUTED: egui::Color32 = egui::Color32::from_rgb(128, 134, 148);

enum Ev {
    Response(Response),
    Eof,
}

pub fn run() {
    if let Err(e) = armoury_core::logging::init("console") {
        eprintln!("logging init failed: {e}");
    }
    log::info!("console starting (v{})", armoury_core::VERSION);

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
    if let Err(e) = write_msg(&mut send_half, &Request::ConsoleHello { pid: std::process::id() }) {
        log::error!("hello failed: {e}");
        std::process::exit(1);
    }
    log::info!("connected to daemon");

    let (tx, rx) = crossbeam_channel::unbounded::<Ev>();
    thread::spawn(move || {
        let mut reader = BufReader::new(recv_half);
        let mut line = String::new();
        loop {
            match read_msg::<_, Response>(&mut reader, &mut line) {
                Ok(Some(resp)) => {
                    if tx.send(Ev::Response(resp)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Ev::Eof);
                    break;
                }
                Err(e) => {
                    log::warn!("pipe read error: {e}");
                    let _ = tx.send(Ev::Eof);
                    break;
                }
            }
        }
    });

    let size = egui::vec2(WIN_W, WIN_H);
    let (icon_rgba, icon_w, icon_h) = armoury_core::icon::rgba_icon(32);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ArmouryDaemon Console")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(egui::WindowLevel::AlwaysOnTop)
            .with_inner_size([size.x, size.y])
            .with_position(console_position(size))
            .with_icon(egui::IconData {
                rgba: icon_rgba,
                width: icon_w,
                height: icon_h,
            }),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "ArmouryDaemon Console",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(App::new(rx, Box::new(send_half))) as Box<dyn eframe::App>)
        }),
    );
}

/// Center-bottom of the primary work area, in logical points, before any window exists.
fn console_position(size: egui::Vec2) -> egui::Pos2 {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};

    let dpi = unsafe { GetDpiForSystem() };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }.max(0.5);
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
        return egui::pos2(200.0, 400.0);
    }
    let (l, t, r, b) = (
        work.left as f32 / scale,
        work.top as f32 / scale,
        work.right as f32 / scale,
        work.bottom as f32 / scale,
    );
    egui::pos2(
        (l + (r - l - size.x) / 2.0).max(l + 8.0),
        (b - size.y - BOTTOM_LIFT).max(t + 8.0),
    )
}

struct App {
    rx: Receiver<Ev>,
    out: Box<dyn Write>,
    status: Option<StatusSnapshot>,
    selected: usize,
    repeat: pad::Repeat,
    prev_a: bool,
    prev_b: bool,
    pad_slot: Option<u32>,
    entering: bool,
    leaving: bool,
    first_frame: bool,
    add_dialog: Option<Receiver<PathBuf>>,
}

impl App {
    fn new(rx: Receiver<Ev>, out: Box<dyn Write>) -> Self {
        Self {
            rx,
            out,
            status: None,
            selected: 0,
            repeat: pad::Repeat::default(),
            prev_a: false,
            prev_b: false,
            pad_slot: None,
            entering: false,
            leaving: false,
            first_frame: true,
            add_dialog: None,
        }
    }

    fn send(&mut self, req: Request) {
        if let Err(e) = write_msg(&mut self.out, &req) {
            log::warn!("send failed: {e}");
        }
    }

    /// The launchable list: tracked apps first, then console-only additions (deduplicated by
    /// file name). The `＋ Add app…` tile trails it in the UI without being an entry here.
    fn entries(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(s) = &self.status {
            for a in &s.tracked_apps {
                out.push((a.name.clone(), a.exe.clone()));
            }
            for a in &s.console_apps {
                let base = basename_lower(&a.exe);
                if !out.iter().any(|(_, exe)| basename_lower(exe) == base) {
                    out.push((a.name.clone(), a.exe.clone()));
                }
            }
        }
        out
    }

    /// A lands here: launch the selected app, or open the file dialog on the add tile.
    fn activate(&mut self, entries: &[(String, String)]) {
        if self.leaving {
            return;
        }
        match entries.get(self.selected) {
            Some((_, exe)) => {
                log::info!("launching {exe}");
                self.send(Request::LaunchApp { exe: exe.clone() });
                self.leaving = true; // the game is starting; slide away
            }
            None => {
                // the ＋ Add app… tile
                let (tx, rx) = crossbeam_channel::bounded(1);
                thread::spawn(move || {
                    if let Some(path) = crate::pick::open_dialog(
                        "Pick an app for the console",
                        "Applications",
                        "*.exe",
                    ) {
                        let _ = tx.send(path);
                    }
                });
                self.add_dialog = Some(rx);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.first_frame {
            crate::ui::install_fonts(&ctx);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            // The moment the window comes up was not on record before; the handshake line
            // ("got the app list") lands around the same time, not before it.
            log::info!("window shown");
            self.first_frame = false;
        }
        ctx.request_repaint(); // the pad is polled per frame

        // ---- daemon events ----
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Response(Response::HelloAck { status, .. }) => {
                    self.status = Some(status);
                    log::info!("got the app list");
                }
                Ev::Response(Response::Status { status }) => self.status = Some(status),
                Ev::Response(Response::Focus) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Ev::Response(Response::PackOp { .. } | Response::ConsoleOp { .. }) => {}
                Ev::Eof => {
                    log::info!("daemon went away — exiting");
                    std::process::exit(0);
                }
            }
        }

        // ---- file dialog result: pin the app ----
        if let Some(rx) = &self.add_dialog {
            match rx.try_recv() {
                Ok(path) => {
                    log::info!("adding {} to the console", path.display());
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    self.send(Request::AddConsoleApp {
                        name,
                        exe: path.to_string_lossy().into_owned(),
                    });
                    self.add_dialog = None;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    log::info!("file picker cancelled");
                    self.add_dialog = None;
                }
            }
        }

        let entries = self.entries();
        let tiles = entries.len() + 1; // + the add tile
        self.selected = self.selected.min(tiles.saturating_sub(1));

        // ---- input: pad first, keyboard as the desktop fallback ----
        let had_pad = self.pad_slot.is_some();
        let mut pad_state = self.pad_slot.and_then(pad::read);
        if pad_state.is_none() {
            pad_state = pad::first_connected().and_then(|slot| {
                self.pad_slot = Some(slot);
                pad::read(slot)
            });
        }
        if pad_state.is_none() && had_pad {
            log::info!("controller disconnected — closing");
            self.pad_slot = None;
            self.leaving = true;
        }

        let (key_left, key_right, key_enter, key_esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });

        let dir = if key_left {
            -1
        } else if key_right {
            1
        } else {
            pad_state.map(|p| p.move_x).unwrap_or(0)
        };
        let step = self.repeat.tick(dir, Instant::now());
        if step != 0 {
            let last = tiles.saturating_sub(1) as i32;
            self.selected = (self.selected as i32 + step as i32).clamp(0, last) as usize;
        }

        let a = pad_state.map(|p| p.a).unwrap_or(false);
        let b = pad_state.map(|p| p.b).unwrap_or(false);
        if (a && !self.prev_a) || key_enter {
            self.activate(&entries);
        }
        if (b && !self.prev_b) || key_esc {
            self.leaving = true;
        }
        self.prev_a = a;
        self.prev_b = b;

        if !self.entering && !self.leaving {
            self.entering = true;
        }
        let enter = ctx.animate_bool_with_time(
            egui::Id::unique("console-enter"),
            self.entering && !self.leaving,
            0.28,
        );
        if self.leaving && enter < 0.03 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // ---- render: one horizontal strip, selection glides, scroll follows ----
        // The band itself is solid (the OS window pops instantly); the content fades and
        // rises into place on entry and dips out again on the way to a launch or close.
        ui.painter().rect_filled(ui.max_rect(), 0.0, BG);
        ui.set_opacity(enter.max(0.01));
        let full = ui.max_rect();
        let rise = (1.0 - enter) * 40.0;

        let view_w = (full.width() - 2.0 * SIDE).max(TILE_W);
        let view_left = full.left() + SIDE;
        let pitch = TILE_W + GAP;
        let total = tiles as f32 * pitch - GAP;
        let target_off = if total <= view_w {
            -(view_w - total) / 2.0
        } else {
            (self.selected as f32 * pitch + TILE_W / 2.0 - view_w / 2.0)
                .clamp(0.0, total - view_w)
        };
        let off = ctx.animate_value_with_time(egui::Id::unique("console-scroll"), target_off, 0.22);
        let center_y = full.center().y - 14.0 + rise;

        for i in 0..tiles {
            let sel = ctx.animate_bool_with_time(
                egui::Id::unique(("console-sel", i)),
                i == self.selected,
                0.18,
            );
            let scale = 1.0 + 0.055 * sel;
            let size = egui::vec2(TILE_W, TILE_H) * scale;
            let x = view_left + i as f32 * pitch + TILE_W / 2.0 - off;
            let center = egui::pos2(x, center_y - 12.0 * sel);
            let rect = egui::Rect::from_center_size(center, size);
            if rect.right() < view_left - 60.0 || rect.left() > view_left + view_w + 60.0 {
                continue; // off the visible strip: don't paint or catch clicks
            }

            let response = ui.interact(
                rect,
                egui::Id::unique(("console-tile", i)),
                egui::Sense::click(),
            );
            let glow = (sel * 0.8 + if response.hovered() { 0.5 } else { 0.0 }).min(1.0);

            ui.painter().rect_filled(
                rect,
                16.0,
                if i == self.selected { TILE_SEL } else { TILE },
            );
            if glow > 0.01 {
                ui.painter().rect_stroke(
                    rect,
                    16.0,
                    egui::Stroke::new(2.0, ACCENT.gamma_multiply(glow)),
                    egui::StrokeKind::Outside,
                );
            }

            match entries.get(i) {
                Some((name, _)) => {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        short_name(name, 16),
                        egui::FontId::proportional(22.0),
                        TEXT,
                    );
                }
                None => {
                    ui.painter().text(
                        rect.center() - egui::vec2(0.0, 18.0),
                        egui::Align2::CENTER_CENTER,
                        "+",
                        egui::FontId::proportional(44.0),
                        MUTED,
                    );
                    ui.painter().text(
                        rect.center() + egui::vec2(0.0, 26.0),
                        egui::Align2::CENTER_CENTER,
                        "Add app…",
                        egui::FontId::proportional(17.0),
                        MUTED,
                    );
                }
            }

            if response.clicked() {
                self.selected = i;
                self.activate(&entries);
            }
        }

        // ---- the hint bar ----
        let hint = if self.pad_slot.is_some() {
            "A  open      B  close      D-pad / left stick  scroll"
        } else {
            "No controller — connect one, or open this from the tray"
        };
        ui.painter().text(
            egui::pos2(full.center().x, full.bottom() - 18.0 + rise * 0.5),
            egui::Align2::CENTER_CENTER,
            hint,
            egui::FontId::proportional(14.0),
            MUTED,
        );
    }
}

fn basename_lower(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| path.to_ascii_lowercase())
}

/// Trim long names for a tile, keeping the start readable.
fn short_name(name: &str, max: usize) -> String {
    let mut out: String = name.chars().take(max).collect();
    if name.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_trims_visibly() {
        assert_eq!(short_name("RainbowSix", 16), "RainbowSix");
        assert_eq!(short_name("Rainbow Six Extraction", 16), "Rainbow Six Extr…");
        // unicode-safe: never split a multi-byte character
        assert_eq!(short_name("Æther 全角テスト長い名前", 12), "Æther 全角テスト長…");
    }
}
