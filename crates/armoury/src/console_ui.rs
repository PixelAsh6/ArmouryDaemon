//! Console launcher (`--console`): the pad-driven cover-art strip the daemon spawns on a fresh
//! controller connect (the tray menu and the settings preview can open it too).
//!
//! Its own process, like `--ui`: it connects to the daemon over the same JSONL pipe, receives
//! status broadcasts (which carry the app lists) and sends launch / add requests back.
//! Input is XInput polled per frame; transitions ride egui's `animate_*` helpers.
//! Covers are fetched by the daemon into `%APPDATA%\ArmouryDaemon\art`; this side only reads
//! that folder (hand-dropped files win) and falls back to the exe icon, then a monogram.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use armoury_core::config::TrackedApp;
use armoury_core::protocol::{read_msg, write_msg, Request, Response};
use armoury_core::status::StatusSnapshot;
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use eframe::egui::epaint::{CornerRadius, Mesh, RectShape};
use interprocess::local_socket::{prelude::*, ConnectOptions, GenericNamespaced};

use crate::art;
use crate::pad;

/// The launcher's window is the band itself: a fixed 90% x 35% of the full screen, console style.
const BAND_W_FRAC: f32 = 0.90;
const BAND_H_FRAC: f32 = 0.35;
/// Tile height as a fraction of the band; the width keeps the old 196:240 cover ratio.
const TILE_H_FRAC: f32 = 0.60;
const TILE_ASPECT: f32 = 196.0 / 240.0;
const GAP: f32 = 18.0;
const SIDE: f32 = 48.0;
const TILE_RADIUS: f32 = 16.0;
/// Fallback icon box inside a tile (square covers and exe icons).
const ICON: f32 = 108.0;
/// How far above the work-area bottom the band sits: docked just above the taskbar.
const BOTTOM_LIFT: f32 = 8.0;
/// Gap under the tiles before the hint line (the names sit inside the tiles).
const HINT_GAP: f32 = 14.0;
/// Reserved height of the one-line hint: tiles + gap + hint center as one block.
const HINT_H: f32 = 18.0;
/// Hero background: how long the crossfade to a newly selected entry takes.
const BG_FADE: f32 = 0.30;
/// Hero background: how tall the bottom gradient (the hint line's dark bed) is.
const BG_GRADIENT: f32 = 140.0;

const BG: egui::Color32 = egui::Color32::from_rgb(9, 10, 13);
const TILE: egui::Color32 = egui::Color32::from_rgb(24, 26, 32);
const TILE_SEL: egui::Color32 = egui::Color32::from_rgb(38, 42, 54);
/// Focus ring gradient, left to right, and the glow's mid tone.
const FOCUS_FROM: egui::Color32 = egui::Color32::from_rgb(167, 108, 255);
const FOCUS_TO: egui::Color32 = egui::Color32::from_rgb(233, 84, 190);
const FOCUS_MID: egui::Color32 = egui::Color32::from_rgb(200, 96, 222);
const TEXT: egui::Color32 = egui::Color32::from_rgb(232, 236, 244);
const MUTED: egui::Color32 = egui::Color32::from_rgb(128, 134, 148);
/// A failed launch: the reason has to stand out against the muted hint line.
const WARN: egui::Color32 = egui::Color32::from_rgb(255, 138, 122);

const PAD_HINT: &str =
    "A  open      B  close      X  add app      Y  remove      D-pad / left stick  scroll";
const KEY_HINT: &str = "Arrows / WASD move · Enter open · Esc close · Delete remove";
const EMPTY_HINT: &str = "No apps yet — press X to add one";
const LAUNCHING_HINT: &str = "starting…";

/// How long the band waits for the daemon's answer before it gives up on a launch.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(8);

/// The band's launch state: nothing asked for, a launch in flight, or a failure to show.
#[derive(Debug, Clone, PartialEq)]
enum Launch {
    Idle,
    Pending,
    Failed(String),
}

/// Apply the daemon's answer to a launch request. True = the band closes (the game
/// started); false = it stays open and shows why the launch failed.
fn apply_launch_ack(state: &mut Launch, ok: bool, message: String) -> bool {
    if ok {
        *state = Launch::Idle;
        true
    } else {
        *state = Launch::Failed(message);
        false
    }
}

/// The hint line, its font size and its colour. A launch in flight and a failed launch
/// take the line over: the user has to see that the game did not start.
fn hint_line(n: usize, pad: bool, launch: &Launch) -> (String, f32, egui::Color32) {
    match launch {
        Launch::Failed(message) => (shorten(message, 140), 16.0, WARN),
        Launch::Pending => (LAUNCHING_HINT.to_string(), 16.0, MUTED),
        Launch::Idle => {
            let (text, size) = if n == 0 {
                (EMPTY_HINT, 18.0)
            } else if pad {
                (PAD_HINT, 14.0)
            } else {
                (KEY_HINT, 14.0)
            };
            (text.to_string(), size, MUTED)
        }
    }
}

enum Ev {
    Response(Response),
    Eof,
}

/// Decoded cover art from the worker: the key plus RGBA pixels (`None` = nothing readable yet).
struct ArtMsg {
    key: String,
    image: Option<(Vec<u8>, u32, u32)>,
}

/// Extracted exe icon from the worker.
struct IconMsg {
    key: String,
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

pub fn run() {
    if let Err(e) = armoury_core::logging::init("console") {
        eprintln!("logging init failed: {e}");
    }
    log::info!("console starting (v{})", armoury_core::VERSION);
    if let Some(code) = crate::dpi_error() {
        // The DPI switch failed before the logger existed; now it can be said.
        log::info!("DPI awareness was not set (error {code}): the process may already be aware");
    }
    if console_already_open() {
        log::info!("another console is already open — exiting");
        return;
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
    if let Err(e) = write_msg(
        &mut send_half,
        &Request::ConsoleHello {
            pid: std::process::id(),
        },
    ) {
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

    let work = work_area();
    let screen = screen_size();
    // Size and position are fixed: everything is known before the window exists.
    let band = band_rect(work, screen);
    let size = band.size();
    let (icon_rgba, icon_w, icon_h) = armoury_core::icon::rgba_icon(32);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ArmouryDaemon Console")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(egui::WindowLevel::AlwaysOnTop)
            .with_inner_size([size.x, size.y])
            .with_position(band.min)
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
            Ok(Box::new(App::new(rx, Box::new(send_half), screen)) as Box<dyn eframe::App>)
        }),
    );
}

/// The band's height in logical points for a full-screen height.
fn band_height(screen_h: f32) -> f32 {
    screen_h * BAND_H_FRAC
}

/// Tile size in logical points: 60% of the band tall, keeping the 196:240 cover ratio.
fn tile_size(screen_h: f32) -> egui::Vec2 {
    let h = band_height(screen_h) * TILE_H_FRAC;
    egui::vec2(h * TILE_ASPECT, h)
}

/// The band inside a work area (logical points): a fixed 90% of the screen wide, centred
/// horizontally, 35% of the screen tall, its bottom edge `BOTTOM_LIFT` above the work area.
fn band_rect(work: egui::Rect, screen: egui::Vec2) -> egui::Rect {
    let half = screen.x * BAND_W_FRAC / 2.0;
    egui::Rect::from_min_max(
        egui::pos2(
            work.center().x - half,
            work.bottom() - BOTTOM_LIFT - band_height(screen.y),
        ),
        egui::pos2(work.center().x + half, work.bottom() - BOTTOM_LIFT),
    )
}

/// Whole tiles that fit the strip's inner width with the inter-tile gap: at least one, and
/// never one more than the width holds.
fn visible_tiles(inner_w: f32, tile_w: f32) -> usize {
    (((inner_w + GAP) / (tile_w + GAP)).floor() as usize).max(1)
}

/// The tile row's centre line. Tiles + hint gap + hint are one block, centred vertically.
fn row_center_y(band: egui::Rect) -> f32 {
    band.center().y - (HINT_GAP + HINT_H) / 2.0
}

/// DPI scale of the band: the primary monitor's. The band is primary-only and the primary
/// runs at 125% here, so the system DPI is the right scale. Per-monitor limitation: a band
/// on the 100% secondary would need that monitor's DPI, not the system one.
fn dpi_scale() -> f32 {
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;

    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }.max(0.5)
}

/// Physical pixels to logical points at `scale`. The process is Per-Monitor-V2 aware, so
/// Win32 hands out physical pixels; the band is drawn in points and egui multiplies them
/// by the window's `pixels_per_point`, which is the scale of the monitor it sits on.
fn to_points(px: f32, scale: f32) -> f32 {
    px / scale
}

/// Primary work area in logical points, before any window exists.
fn work_area() -> egui::Rect {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};

    let scale = dpi_scale();
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
        // No query (unlikely): a 1080p work area keeps the band on-screen.
        return egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1920.0, 1040.0));
    }
    egui::Rect::from_min_max(
        egui::pos2(
            to_points(work.left as f32, scale),
            to_points(work.top as f32, scale),
        ),
        egui::pos2(
            to_points(work.right as f32, scale),
            to_points(work.bottom as f32, scale),
        ),
    )
}

/// Full primary screen size in logical points (the band's height comes from this, not the work
/// area, so it stays 35% of the screen with or without a taskbar).
fn screen_size() -> egui::Vec2 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };

    let scale = dpi_scale();
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if w <= 0 || h <= 0 {
        return egui::vec2(1920.0, 1080.0);
    }
    egui::vec2(to_points(w as f32, scale), to_points(h as f32, scale))
}

/// One console at a time: a tray open racing a pad connect (or a manual run) must not stack
/// windows. The mutex is held for the process lifetime — the OS releases it on exit.
fn console_already_open() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;
    let name: Vec<u16> = "Local\\ArmouryDaemonConsole"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    match unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) } {
        Ok(_held) => unsafe { GetLastError() == ERROR_ALREADY_EXISTS },
        Err(_) => false,
    }
}

/// The Windows "Animation effects" setting (SPI_GETCLIENTAREAANIMATION). Defaults to on.
fn animations_enabled() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION,
    };
    let mut on: i32 = 1;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            &mut on as *mut i32 as *mut core::ffi::c_void,
            0,
        )
    };
    ok != 0 && on != 0
}

struct App {
    rx: Receiver<Ev>,
    out: Box<dyn Write>,
    status: Option<StatusSnapshot>,
    selected: usize,
    repeat: pad::Repeat,
    prev_a: bool,
    prev_b: bool,
    prev_x: bool,
    prev_y: bool,
    pad_slot: Option<u32>,
    entering: bool,
    leaving: bool,
    /// The launch the daemon is being asked about (and the failure it reported).
    launch: Launch,
    /// When the pending launch was asked for (the band gives up after `LAUNCH_TIMEOUT`).
    launch_since: Option<Instant>,
    first_frame: bool,
    add_dialog: Option<Receiver<PathBuf>>,
    /// Windows "reduce motion" honored: everything lands instantly.
    reduced: bool,
    /// Art/icon textures live on the UI thread; workers deliver decoded pixels.
    arts: HashMap<String, egui::TextureHandle>,
    /// Hero textures for the band background, keyed like `arts`.
    heros: HashMap<String, egui::TextureHandle>,
    icons: HashMap<String, egui::TextureHandle>,
    art_tx: Sender<ArtMsg>,
    art_rx: Receiver<ArtMsg>,
    hero_tx: Sender<ArtMsg>,
    hero_rx: Receiver<ArtMsg>,
    icon_tx: Sender<IconMsg>,
    icon_rx: Receiver<IconMsg>,
    art_want: HashSet<String>,
    hero_want: HashSet<String>,
    icon_want: HashSet<String>,
    /// Hero crossfade: the fading-out key, the incoming one, and its progress (0..=1).
    bg_prev: Option<String>,
    bg_key: Option<String>,
    bg_fade: f32,
    /// Which status we last ran the art/icon passes for (0 = none yet).
    ensured_for: u64,
    status_serial: u64,
    /// Full screen size in logical points, captured before the window existed (sizes the band).
    screen: egui::Vec2,
}

impl App {
    fn new(rx: Receiver<Ev>, out: Box<dyn Write>, screen: egui::Vec2) -> Self {
        let (art_tx, art_rx) = crossbeam_channel::unbounded();
        let (hero_tx, hero_rx) = crossbeam_channel::unbounded();
        let (icon_tx, icon_rx) = crossbeam_channel::unbounded();
        Self {
            rx,
            out,
            status: None,
            selected: 0,
            repeat: pad::Repeat::default(),
            prev_a: false,
            prev_b: false,
            prev_x: false,
            prev_y: false,
            pad_slot: None,
            entering: false,
            leaving: false,
            launch: Launch::Idle,
            launch_since: None,
            first_frame: true,
            add_dialog: None,
            reduced: !animations_enabled(),
            arts: HashMap::new(),
            heros: HashMap::new(),
            icons: HashMap::new(),
            art_tx,
            art_rx,
            hero_tx,
            hero_rx,
            icon_tx,
            icon_rx,
            art_want: HashSet::new(),
            hero_want: HashSet::new(),
            icon_want: HashSet::new(),
            bg_prev: None,
            bg_key: None,
            bg_fade: 1.0,
            ensured_for: 0,
            status_serial: 0,
            screen,
        }
    }

    fn send(&mut self, req: Request) {
        if let Err(e) = write_msg(&mut self.out, &req) {
            log::warn!("send failed: {e}");
        }
    }

    /// The launchable list: tracked apps first, then console-only additions (deduplicated by
    /// file name), ordered by recency — recently played first, never-played keep config order.
    /// Tracked apps the user hid sit the round out.
    fn entries(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(s) = &self.status {
            for a in &s.tracked_apps {
                if s.console_hidden.contains(&basename_lower(&a.exe)) {
                    continue;
                }
                out.push((a.name.clone(), a.exe.clone()));
            }
            for a in &s.console_apps {
                let base = basename_lower(&a.exe);
                if !out.iter().any(|(_, exe)| basename_lower(exe) == base) {
                    out.push((a.name.clone(), a.exe.clone()));
                }
            }
            let played = |exe: &str| s.last_played.get(&basename_lower(exe)).copied().unwrap_or(0);
            out.sort_by_key(|(_, exe)| std::cmp::Reverse(played(exe)));
        }
        out
    }

    /// Queue cover-art reads for entries not loaded yet. Cheap: keys nothing has on disk are
    /// skipped (the daemon's next status broadcast re-checks after a fetch).
    fn ensure_art(&mut self, entries: &[(String, String)]) {
        let mut missing: Vec<String> = Vec::new();
        for (_, exe) in entries {
            let key = art::key(exe);
            if self.arts.contains_key(&key) || self.art_want.contains(&key) {
                continue;
            }
            if art::cached(&key).is_none() {
                continue;
            }
            self.art_want.insert(key.clone());
            missing.push(key);
        }
        if missing.is_empty() {
            return;
        }
        let tx = self.art_tx.clone();
        thread::spawn(move || {
            for key in missing {
                let image = art::cached(&key).and_then(|path| {
                    let bytes = std::fs::read(&path).ok()?;
                    let img = image::load_from_memory(&bytes).ok()?;
                    // 2× of the biggest tile keeps it crisp and the texture small.
                    let img = img.resize(560, 560, image::imageops::FilterType::Triangle);
                    let rgba = img.to_rgba8();
                    Some((rgba.as_raw().clone(), rgba.width(), rgba.height()))
                });
                if tx.send(ArtMsg { key, image }).is_err() {
                    return;
                }
            }
        });
    }

    /// Queue hero reads (the band background) for entries not loaded yet.
    fn ensure_heros(&mut self, entries: &[(String, String)]) {
        let mut missing: Vec<String> = Vec::new();
        for (_, exe) in entries {
            let key = art::key(exe);
            if self.heros.contains_key(&key) || self.hero_want.contains(&key) {
                continue;
            }
            if art::cached_hero(&key).is_none() {
                continue;
            }
            self.hero_want.insert(key.clone());
            missing.push(key);
        }
        if missing.is_empty() {
            return;
        }
        let tx = self.hero_tx.clone();
        thread::spawn(move || {
            for key in missing {
                let image = art::cached_hero(&key).and_then(|path| {
                    let bytes = std::fs::read(&path).ok()?;
                    let img = image::load_from_memory(&bytes).ok()?;
                    // Hero art is wide (~3:1); 1600 across fits the band with room to spare.
                    let img = img.resize(1600, 1600, image::imageops::FilterType::Triangle);
                    let rgba = img.to_rgba8();
                    Some((rgba.as_raw().clone(), rgba.width(), rgba.height()))
                });
                if tx.send(ArtMsg { key, image }).is_err() {
                    return;
                }
            }
        });
    }

    /// Queue exe-icon extraction for any entry we haven't fetched yet.
    fn ensure_icons(&mut self, entries: &[(String, String)]) {
        let mut missing: Vec<(String, String)> = Vec::new();
        for (_, exe) in entries {
            let key = basename_lower(exe);
            if !self.icons.contains_key(&key) && !self.icon_want.contains(&key) {
                self.icon_want.insert(key.clone());
                missing.push((key, exe.clone()));
            }
        }
        if missing.is_empty() {
            return;
        }
        let tx = self.icon_tx.clone();
        thread::spawn(move || {
            for (key, exe) in missing {
                if let Some((rgba, w, h)) =
                    armoury_core::icon::exe_icon_rgba(std::path::Path::new(&exe))
                {
                    if tx.send(IconMsg { key, rgba, w, h }).is_err() {
                        return;
                    }
                }
            }
        });
    }

    /// A lands here: launch the selected app.
    ///
    /// The band waits for the daemon's answer: a game that did not start must not close
    /// the band silently, or the user only sees the band slide away and nothing happens.
    fn activate(&mut self, entries: &[(String, String)]) {
        if self.leaving || self.launch == Launch::Pending {
            return;
        }
        if let Some((_, exe)) = entries.get(self.selected) {
            log::info!("launching {exe}");
            self.launch = Launch::Pending;
            self.launch_since = Some(Instant::now());
            self.send(Request::LaunchApp { exe: exe.clone() });
        }
    }

    /// X lands here: pick an exe and pin it (the gamepad button instead of a strip tile).
    fn open_add_dialog(&mut self) {
        if self.add_dialog.is_some() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        thread::spawn(move || {
            if let Some(path) =
                crate::pick::open_dialog("Pick an app for the console", "Applications", "*.exe")
            {
                let _ = tx.send(path);
            }
        });
        self.add_dialog = Some(rx);
        log::info!("add-app picker opened");
    }

    /// Y lands here: take the selected entry off the band only. A tracked app is hidden (the
    /// daemon keeps tracking it); a console-only pin is unpinned for good.
    fn remove_selected(&mut self, entries: &[(String, String)]) {
        let Some((_, exe)) = entries.get(self.selected) else {
            return;
        };
        let tracked = self
            .status
            .as_ref()
            .map(|s| s.tracked_apps.as_slice())
            .unwrap_or(&[]);
        log::info!("removing {exe} from the band");
        self.send(remove_request(tracked, exe));
    }
}

/// Which request "remove" sends for an entry: a tracked app is only hidden from the band (the
/// daemon keeps tracking it), a console-only pin is unpinned for good.
fn remove_request(tracked: &[TrackedApp], exe: &str) -> Request {
    if is_tracked(tracked, exe) {
        Request::HideFromConsole { exe: exe.to_string() }
    } else {
        Request::RemoveConsoleApp { exe: exe.to_string() }
    }
}

/// Does the status's tracked list hold this exe, matched by base name like the band matches?
fn is_tracked(tracked: &[TrackedApp], exe: &str) -> bool {
    let base = basename_lower(exe);
    tracked.iter().any(|a| basename_lower(&a.exe) == base)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.first_frame {
            crate::ui::install_fonts(&ctx);
        }
        ctx.request_repaint(); // the pad is polled per frame

        // ---- daemon events ----
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Response(resp) => {
                    // The answer to a launch request rides the console-op variant; the
                    // marker tells it apart from the add/remove answers.
                    let ack = armoury_core::protocol::as_launch_ack(&resp)
                        .map(|(ok, text)| (ok, text.to_string()));
                    match ack {
                        Some((ok, message)) => {
                            log::info!("launch result: ok={ok} {message}");
                            if apply_launch_ack(&mut self.launch, ok, message) {
                                self.leaving = true; // the game started: slide away
                            }
                        }
                        None => match resp {
                            Response::HelloAck { status, .. } => {
                                self.status = Some(status);
                                self.status_serial += 1;
                                log::info!("got the app list");
                            }
                            Response::Status { status } => {
                                self.status = Some(status);
                                self.status_serial += 1;
                            }
                            Response::Focus => {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                            }
                            Response::PackOp { .. } | Response::ConsoleOp { .. } => {}
                        },
                    }
                }
                Ev::Eof => {
                    log::info!("daemon went away — exiting");
                    std::process::exit(0);
                }
            }
        }

        // A launch that gets no answer must not leave the band waiting forever.
        if self.launch == Launch::Pending
            && self
                .launch_since
                .map(|since| since.elapsed() > LAUNCH_TIMEOUT)
                .unwrap_or(false)
        {
            log::info!("the daemon did not answer the launch request");
            self.launch = Launch::Failed("the daemon did not answer the launch request".into());
        }

        // ---- cover art from the worker ----
        while let Ok(m) = self.art_rx.try_recv() {
            match m.image {
                Some((rgba, w, h)) => {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [w as usize, h as usize],
                        &rgba,
                    );
                    let handle = ctx.load_texture(
                        format!("console-art:{}", m.key),
                        img,
                        egui::TextureOptions::LINEAR,
                    );
                    self.arts.insert(m.key, handle);
                }
                None => {
                    // Unreadable file: forget the wish so the next status can re-try.
                    log::info!("cover art: {} unreadable; using the icon", m.key);
                    self.art_want.remove(&m.key);
                }
            }
        }

        // ---- hero art from the worker (the band background) ----
        while let Ok(m) = self.hero_rx.try_recv() {
            match m.image {
                Some((rgba, w, h)) => {
                    let img =
                        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    let handle = ctx.load_texture(
                        format!("console-hero:{}", m.key),
                        img,
                        egui::TextureOptions::LINEAR,
                    );
                    self.heros.insert(m.key, handle);
                }
                None => {
                    // Unreadable file: forget the wish so the next status can re-try.
                    log::info!("hero art: {} unreadable; using the flat band", m.key);
                    self.hero_want.remove(&m.key);
                }
            }
        }

        // ---- exe icons from the worker ----
        while let Ok(m) = self.icon_rx.try_recv() {
            let img = egui::ColorImage::from_rgba_unmultiplied([m.w as usize, m.h as usize], &m.rgba);
            let handle = ctx.load_texture(
                format!("console-icon:{}", m.key),
                img,
                egui::TextureOptions::LINEAR,
            );
            self.icons.insert(m.key, handle);
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
        let n = entries.len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
        if self.ensured_for != self.status_serial {
            self.ensured_for = self.status_serial;
            self.ensure_art(&entries);
            self.ensure_heros(&entries);
            self.ensure_icons(&entries);
        }

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

        let (key_left, key_right, key_up, key_down, key_enter, key_esc, key_delete) =
            ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::A),
                    i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::D),
                    i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::W),
                    i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::S),
                    i.key_pressed(egui::Key::Enter),
                    i.key_pressed(egui::Key::Escape),
                    // One removal per physical press, like pad Y, not one per key-repeat.
                    i.events.iter().any(|e| {
                        matches!(
                            e,
                            egui::Event::Key {
                                key: egui::Key::Delete,
                                pressed: true,
                                repeat: false,
                                ..
                            }
                        )
                    }),
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
            self.selected = wrap(self.selected, step.into(), n);
        }
        // One row: vertical keys still land somewhere sensible instead of doing nothing.
        if key_up {
            self.selected = 0;
        }
        if key_down {
            self.selected = n.saturating_sub(1);
        }

        let a = pad_state.map(|p| p.a).unwrap_or(false);
        let b = pad_state.map(|p| p.b).unwrap_or(false);
        let x = pad_state.map(|p| p.x).unwrap_or(false);
        let y = pad_state.map(|p| p.y).unwrap_or(false);
        if (a && !self.prev_a) || key_enter {
            self.activate(&entries);
        }
        if x && !self.prev_x {
            self.open_add_dialog();
        }
        if (y && !self.prev_y) || key_delete {
            self.remove_selected(&entries);
        }
        if (b && !self.prev_b) || key_esc {
            self.leaving = true;
        }
        self.prev_a = a;
        self.prev_b = b;
        self.prev_x = x;
        self.prev_y = y;

        if !self.entering && !self.leaving {
            self.entering = true;
        }
        let slow = if self.reduced { 0.001 } else { 0.28 };
        let enter = ctx.animate_bool_with_time(egui::Id::unique("console-enter"), self.entering && !self.leaving, slow);
        if self.leaving && enter < 0.03 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // ---- background: the selected entry's hero, crossfading on selection ----
        // A missing hero falls back to the flat band, so the outgoing one is dropped too.
        let hero_key = entries
            .get(self.selected)
            .map(|(_, exe)| art::key(exe))
            .filter(|key| self.heros.contains_key(key));
        if hero_key != self.bg_key {
            self.bg_prev = std::mem::replace(&mut self.bg_key, hero_key);
            self.bg_fade = if self.reduced { 1.0 } else { 0.0 };
        }
        if self.bg_key.is_none() {
            self.bg_prev = None;
        }
        if !self.reduced {
            self.bg_fade = (self.bg_fade + ctx.input(|i| i.stable_dt) / BG_FADE).min(1.0);
        }

        // ---- window: sized and positioned once at startup; just reveal it on the first frame ----
        let (hint, hint_font, hint_color) = hint_line(n, self.pad_slot.is_some(), &self.launch);
        let tile = tile_size(self.screen.y);
        if self.first_frame {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            log::info!("window shown");
            self.first_frame = false;
        }

        // ---- render: one horizontal strip, selection glides, the row wraps around ----
        // The band itself is solid (the OS window pops instantly); the content fades and
        // rises into place on entry and dips out again on the way to a launch or close.
        let bg = ui.painter().clone();
        let full = ui.max_rect();
        bg.rect_filled(full, 0.0, BG);
        if let Some(tex) = self.bg_prev.as_ref().and_then(|k| self.heros.get(k)) {
            paint_hero(&bg, full, tex, 1.0); // the outgoing hero stays put underneath
        }
        if let Some(tex) = self.bg_key.as_ref().and_then(|k| self.heros.get(k)) {
            paint_hero(&bg, full, tex, self.bg_fade);
            paint_band_dim(&bg, full);
        }
        ui.set_opacity(enter.max(0.01));
        let rise = if self.reduced { 0.0 } else { (1.0 - enter) * 40.0 };

        let view_w = (full.width() - 2.0 * SIDE).max(tile.x);
        let view_left = full.left() + SIDE;
        let pitch = tile.x + GAP;
        let total = n as f32 * pitch - GAP;
        let target_off = if n <= visible_tiles(view_w, tile.x) {
            -(view_w - total) / 2.0
        } else {
            (self.selected as f32 * pitch + tile.x / 2.0 - view_w / 2.0)
                .clamp(0.0, (total - view_w).max(0.0))
        };
        let scroll_time = if self.reduced { 0.001 } else { 0.22 };
        let off = ctx.animate_value_with_time(egui::Id::unique("console-scroll"), target_off, scroll_time);
        // Tiles + hint travel as one block, centered both ways in the band.
        let center_y = row_center_y(full) + rise;
        let hint_y = center_y + tile.y / 2.0 + HINT_GAP + HINT_H / 2.0;
        let sel_time = if self.reduced { 0.001 } else { 0.18 };

        // Hover sets the focus (only while the pointer actually moves, so the row scrolling
        // under a resting cursor never fights the pad/keyboard). A click launches that tile.
        let pointer_moved = ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let mut clicked = None;
        let mut visible: Vec<(usize, egui::Rect, bool, f32)> = Vec::with_capacity(n);
        for i in 0..n {
            let sel = ctx.animate_bool_with_time(egui::Id::unique(("console-sel", i)), i == self.selected, sel_time);
            let scale = 1.0 + 0.055 * sel;
            let rect = egui::Rect::from_center_size(
                egui::pos2(
                    view_left + i as f32 * pitch + tile.x / 2.0 - off,
                    center_y - 12.0 * sel,
                ),
                tile * scale,
            );
            if rect.right() < view_left - 60.0 || rect.left() > view_left + view_w + 60.0 {
                continue; // off the visible strip: don't paint or catch clicks
            }
            let response = ui.interact(rect, egui::Id::unique(("console-tile", i)), egui::Sense::click());
            if response.hovered() && pointer_moved {
                self.selected = i;
            }
            if response.clicked() {
                clicked = Some(i);
            }
            visible.push((i, rect, response.hovered(), sel));
        }
        if let Some(i) = clicked {
            self.selected = i;
            self.activate(&entries);
        }

        for (i, rect, hovered, sel) in visible {
            let (name, exe) = &entries[i];
            let scale = 1.0 + 0.055 * sel;
            let glow = (sel * 0.8 + if hovered { 0.5 } else { 0.0 }).min(1.0);

            ui.painter().rect_filled(rect, TILE_RADIUS, if i == self.selected { TILE_SEL } else { TILE });

            let icon_key = basename_lower(exe);
            let cover = self.arts.get(&art::key(exe));
            match cover {
                Some(tex) => {
                    // Center-crop the 2:3 art into the tile; scrim + name keep it readable.
                    let (uv_min, uv_max) = cover_uv(tex.size_vec2(), rect);
                    let uv = egui::Rect::from_min_max(uv_min, uv_max);
                    ui.painter().add(
                        RectShape::filled(rect, CornerRadius::from(TILE_RADIUS), egui::Color32::WHITE)
                            .with_texture(tex.id(), uv),
                    );
                    paint_scrim(ui.painter(), rect);
                    ui.painter().text(
                        egui::pos2(rect.center().x, rect.bottom() - 24.0),
                        egui::Align2::CENTER_CENTER,
                        short_name(name, 20),
                        egui::FontId::proportional(17.0),
                        TEXT.gamma_multiply(0.86 + 0.14 * sel),
                    );
                }
                None => {
                    let icon_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.center().x, rect.center().y - 24.0),
                        egui::vec2(ICON, ICON) * scale,
                    );
                    match self.icons.get(&icon_key) {
                        Some(tex) => {
                            let uv = egui::Rect::from_min_max(
                                egui::pos2(0.0, 0.0),
                                egui::pos2(1.0, 1.0),
                            );
                            ui.painter().add(
                                RectShape::filled(icon_rect, CornerRadius::from(12.0), egui::Color32::WHITE)
                                    .with_texture(tex.id(), uv),
                            );
                        }
                        None => {
                            ui.painter().text(
                                icon_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                first_char(name),
                                egui::FontId::proportional(64.0),
                                TEXT.gamma_multiply(0.55 + 0.45 * sel),
                            );
                        }
                    }
                    ui.painter().text(
                        egui::pos2(rect.center().x, rect.bottom() - 28.0),
                        egui::Align2::CENTER_CENTER,
                        short_name(name, 20),
                        egui::FontId::proportional(17.0),
                        TEXT.gamma_multiply(0.80 + 0.20 * sel),
                    );
                }
            }

            if glow > 0.01 {
                paint_focus_glow(ui.painter(), rect, glow);
                paint_focus_ring(ui.painter(), rect, glow);
            }
        }

        // ---- the hint line: centred under the tile block ----
        let hint_y = if n == 0 { full.center().y } else { hint_y };
        ui.painter().text(
            egui::pos2(full.center().x, hint_y),
            egui::Align2::CENTER_CENTER,
            hint,
            egui::FontId::proportional(hint_font),
            hint_color,
        );
    }
}

/// One hero texture across the whole band, center-cropped and faded by `alpha` (the crossfade).
/// Radius 0: the band's own rectangle.
fn paint_hero(painter: &egui::Painter, rect: egui::Rect, tex: &egui::TextureHandle, alpha: f32) {
    let (uv_min, uv_max) = cover_uv(tex.size_vec2(), rect);
    let uv = egui::Rect::from_min_max(uv_min, uv_max);
    let tint = egui::Color32::from_white_alpha((255.0 * alpha.clamp(0.0, 1.0)) as u8);
    painter.add(RectShape::filled(rect, CornerRadius::ZERO, tint).with_texture(tex.id(), uv));
}

/// Keep the strip readable over a hero: a flat dim over the whole band, then a stronger
/// gradient toward the hint line. Tiles and hints draw on top of both.
fn paint_band_dim(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(150));
    let mut m = Mesh::default();
    let top = rect.bottom() - BG_GRADIENT;
    m.colored_vertex(egui::pos2(rect.left(), top), egui::Color32::TRANSPARENT);
    m.colored_vertex(egui::pos2(rect.right(), top), egui::Color32::TRANSPARENT);
    m.colored_vertex(
        egui::pos2(rect.left(), rect.bottom()),
        egui::Color32::from_black_alpha(210),
    );
    m.colored_vertex(
        egui::pos2(rect.right(), rect.bottom()),
        egui::Color32::from_black_alpha(210),
    );
    m.add_triangle(0, 1, 2);
    m.add_triangle(2, 1, 3);
    painter.add(egui::Shape::mesh(m));
}

/// Move the selection by `step` with wrap-around (the endless scroll through the row).
fn wrap(selected: usize, step: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as i32 + step).rem_euclid(len as i32) as usize
}

/// Center-crop uv so cover art fills the tile without squashing.
fn cover_uv(image: egui::Vec2, rect: egui::Rect) -> (egui::Pos2, egui::Pos2) {
    let img_aspect = image.x / image.y.max(1.0);
    let tile_aspect = rect.width() / rect.height().max(1.0);
    if img_aspect > tile_aspect {
        let keep = tile_aspect / img_aspect;
        let dx = (1.0 - keep) / 2.0;
        (egui::pos2(dx, 0.0), egui::pos2(1.0 - dx, 1.0))
    } else {
        let keep = img_aspect / tile_aspect;
        let dy = (1.0 - keep) / 2.0;
        (egui::pos2(0.0, dy), egui::pos2(1.0, 1.0 - dy))
    }
}

/// Fade the cover into the tile's bottom so the name stays readable: one continuous vertical
/// gradient, transparent `H` above the bottom edge up to black_alpha(200) at it. The bottom
/// rows trace the tile's corner arcs, so the fill stays rounded instead of squaring off.
fn paint_scrim(painter: &egui::Painter, rect: egui::Rect) {
    let mut m = Mesh::default();
    for (y, left, right, a) in scrim_rows(rect) {
        let c = egui::Color32::from_black_alpha(a);
        m.colored_vertex(egui::pos2(left, y), c);
        m.colored_vertex(egui::pos2(right, y), c);
    }
    for q in 0..(m.vertices.len() as u32 / 2) - 1 {
        let a = q * 2;
        m.add_triangle(a, a + 1, a + 2);
        m.add_triangle(a + 2, a + 1, a + 3);
    }
    painter.add(egui::Shape::mesh(m));
}

/// The scrim's vertex rows, top to bottom as `(y, left_x, right_x, alpha)`. Straight sides above
/// the corners; the last rows inset by the corner arc so they never paint past the rounding.
fn scrim_rows(rect: egui::Rect) -> Vec<(f32, f32, f32, u8)> {
    const H: f32 = 120.0; // gradient height above the tile bottom
    const TOP_A: f32 = 200.0; // alpha at the bottom edge
    const ARC: usize = 8; // rows tracing a 16 px corner arc
    let (r, top, corner) = (TILE_RADIUS, rect.bottom() - H, rect.bottom() - TILE_RADIUS);
    let alpha = |y: f32| ((y - top) / H * TOP_A).clamp(0.0, 255.0) as u8;
    let mut rows = vec![
        (top, rect.left(), rect.right(), alpha(top)),
        (corner, rect.left(), rect.right(), alpha(corner)),
    ];
    for k in 1..=ARC {
        let y = corner + r * k as f32 / ARC as f32;
        let dy = y - corner;
        let inset = r - (r * r - dy * dy).max(0.0).sqrt();
        rows.push((y, rect.left() + inset, rect.right() - inset, alpha(y)));
    }
    rows
}

/// The focus ring's colour at `t` across the tile: violet on the left, magenta on the right.
fn focus_color(t: f32, alpha: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
    egui::Color32::from_rgba_unmultiplied(
        mix(FOCUS_FROM.r(), FOCUS_TO.r()),
        mix(FOCUS_FROM.g(), FOCUS_TO.g()),
        mix(FOCUS_FROM.b(), FOCUS_TO.b()),
        (255.0 * alpha.clamp(0.0, 1.0)) as u8,
    )
}

/// A rounded-rect outline sampled into points (8 steps per 16 px corner arc), clockwise from
/// the top edge. Used for both edges of the 2 px gradient ring.
fn rounded_outline(rect: egui::Rect, radius: f32) -> Vec<egui::Pos2> {
    const ARC: usize = 8;
    let r = radius
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0)
        .max(0.0);
    let (l, t, rgt, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let corners = [
        (egui::pos2(rgt - r, t + r), -90.0),
        (egui::pos2(rgt - r, b - r), 0.0),
        (egui::pos2(l + r, b - r), 90.0),
        (egui::pos2(l + r, t + r), 180.0),
    ];
    let mut out = Vec::with_capacity(4 * (ARC + 1));
    for (center, start) in corners {
        for k in 0..=ARC {
            let a = (start + 90.0 * k as f32 / ARC as f32).to_radians();
            out.push(egui::pos2(center.x + r * a.cos(), center.y + r * a.sin()));
        }
    }
    out
}

/// The focus border: a mesh strip around the rounded tile, violet → magenta left to right.
fn paint_focus_ring(painter: &egui::Painter, rect: egui::Rect, alpha: f32) {
    let outer = rounded_outline(rect.expand(1.0), TILE_RADIUS + 1.0);
    let inner = rounded_outline(rect.expand(-1.0), TILE_RADIUS - 1.0);
    let width = rect.width().max(1.0);
    let color = |p: egui::Pos2| focus_color((p.x - rect.left()) / width, alpha);
    let mut m = Mesh::default();
    for i in 0..outer.len() {
        let j = (i + 1) % outer.len();
        let base = m.vertices.len() as u32;
        m.colored_vertex(outer[i], color(outer[i]));
        m.colored_vertex(outer[j], color(outer[j]));
        m.colored_vertex(inner[i], color(inner[i]));
        m.colored_vertex(inner[j], color(inner[j]));
        m.add_triangle(base, base + 1, base + 2);
        m.add_triangle(base + 2, base + 1, base + 3);
    }
    painter.add(egui::Shape::mesh(m));
}

/// The soft halo outside the ring, in the gradient's mid purple.
fn paint_focus_glow(painter: &egui::Painter, rect: egui::Rect, alpha: f32) {
    for k in 0..3 {
        let pad = 2.0 + k as f32 * 2.5;
        let fade = alpha * (0.16 - k as f32 * 0.045);
        painter.rect_stroke(
            rect.expand(pad),
            TILE_RADIUS + pad,
            egui::Stroke::new(2.0, FOCUS_MID.gamma_multiply(fade)),
            egui::StrokeKind::Outside,
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

/// Trim a long message so it still fits the band.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let mut out: String = text.chars().take(max).collect();
        out.push('…');
        out
    } else {
        text.to_string()
    }
}

/// The monogram fallback: the first character, upper-cased.
fn first_char(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_trims_visibly() {
        assert_eq!(short_name("RainbowSix", 16), "RainbowSix");
        assert_eq!(short_name("Rainbow Six Extraction", 16), "Rainbow Six Extr…");
        // unicode-safe: never split a multi-byte character
        assert_eq!(short_name("Æther 全角テスト", 8), "Æther 全角…");
    }

    #[test]
    fn the_band_closes_only_when_the_game_really_started() {
        let mut launch = Launch::Idle;
        // a failed launch keeps the band open and shows the reason
        assert!(!apply_launch_ack(
            &mut launch,
            false,
            "RainbowSix.exe exited right away (exit code 0); the game did not start".into()
        ));
        assert_eq!(
            launch,
            Launch::Failed(
                "RainbowSix.exe exited right away (exit code 0); the game did not start".into()
            )
        );
        // a started game closes it
        assert!(apply_launch_ack(&mut launch, true, "launched game.exe (pid 42)".into()));
        assert_eq!(launch, Launch::Idle);
    }

    #[test]
    fn the_hint_line_shows_the_launch_state() {
        // the usual hints
        let (text, _, color) = hint_line(2, false, &Launch::Idle);
        assert_eq!(text, KEY_HINT);
        assert_eq!(color, MUTED);
        // a launch in flight
        let (text, _, _) = hint_line(2, false, &Launch::Pending);
        assert_eq!(text, LAUNCHING_HINT);
        // a failure: the reason, in the warning colour
        let (text, _, color) = hint_line(2, false, &Launch::Failed("it did not start".into()));
        assert_eq!(text, "it did not start");
        assert_eq!(color, WARN);
    }

    #[test]
    fn selection_wraps_both_ways() {
        assert_eq!(wrap(0, -1, 3), 2); // left from the first lands on the last
        assert_eq!(wrap(2, 1, 3), 0); // right from the last lands on the first
        assert_eq!(wrap(1, 1, 3), 2);
        assert_eq!(wrap(0, 1, 0), 0); // empty list stays put
        assert_eq!(wrap(5, 0, 3), 2); // stale index folds back into range
    }

    #[test]
    fn physical_pixels_become_points_at_the_primary_scale() {
        // The process is Per-Monitor-V2 aware: Win32 reports physical pixels, egui draws in
        // points. The primary is 2560x1440 at 125%, so 2048x1152 points.
        let scale = 1.25;
        assert!((to_points(2560.0, scale) - 2048.0).abs() < 1e-3);
        assert!((to_points(1440.0, scale) - 1152.0).abs() < 1e-3);
    }

    #[test]
    fn band_is_fixed_90_by_35_percent_and_docked() {
        // The primary at 125%: 2560x1440 physical is 2048x1152 points, and the work area is
        // the same width with the taskbar off the bottom.
        let work = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0));
        let screen = egui::vec2(2048.0, 1152.0);
        let band = band_rect(work, screen);
        assert!((band.width() - 1843.2).abs() < 1e-3);
        assert!((band.height() - 403.2).abs() < 1e-3);
        // in physical pixels: 90% of the 2560-wide primary, 35% of its 1440
        assert!((band.width() * 1.25 - 2304.0).abs() < 1e-3);
        assert!((band.height() * 1.25 - 504.0).abs() < 1e-3);
        // centered on the work area, docked 8 above its bottom edge
        assert!((band.center().x - work.center().x).abs() < 1e-3);
        assert!((band.bottom() - (work.bottom() - 8.0)).abs() < 1e-3);
        // a shorter work area moves the band, it does not resize it
        let shorter = band_rect(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1000.0)),
            screen,
        );
        assert!((shorter.height() - 403.2).abs() < 1e-3);
        assert!((shorter.bottom() - 992.0).abs() < 1e-3);
        // centered on a non-zero work-area origin too
        let shifted = band_rect(work.translate(egui::vec2(-1000.0, 40.0)), screen);
        assert!((shifted.center().x - (work.center().x - 1000.0)).abs() < 1e-3);
        assert!((shifted.height() - 403.2).abs() < 1e-3);
    }

    #[test]
    fn tiles_keep_the_196_by_240_ratio() {
        let tile = tile_size(1152.0);
        assert!((tile.y - 241.92).abs() < 1e-3);
        assert!((tile.x - 197.568).abs() < 1e-3);
        assert!((tile.x / tile.y - 196.0 / 240.0).abs() < 1e-6);
    }

    #[test]
    fn visible_tiles_never_exceeds_the_inner_width() {
        let tile_w = tile_size(1152.0).x;
        let pitch = tile_w + GAP;
        for inner in [tile_w, 400.0, 700.0, 1104.0, 1632.0, 2560.0] {
            let n = visible_tiles(inner, tile_w);
            assert!(n >= 1);
            assert!(n as f32 * pitch - GAP <= inner);
            assert!((n as f32 + 1.0) * pitch - GAP > inner);
        }
        // a narrow band still shows the focused tile rather than nothing
        assert_eq!(visible_tiles(10.0, tile_w), 1);
    }

    #[test]
    fn tracked_apps_match_by_base_name() {
        let tracked = vec![TrackedApp {
            name: "Rainbow Six".into(),
            exe: "C:\\Games\\RainbowSix.exe".into(),
        }];
        assert!(is_tracked(&tracked, "D:\\Elsewhere\\rainbowsix.EXE"));
        assert!(!is_tracked(&tracked, "C:\\Games\\Notepad.exe"));
        assert!(!is_tracked(&[], "C:\\Games\\RainbowSix.exe"));
    }

    #[test]
    fn remove_hides_tracked_apps_and_unpins_console_pins() {
        let tracked = vec![TrackedApp {
            name: "Notepad".into(),
            exe: "C:\\Windows\\notepad.exe".into(),
        }];
        assert!(matches!(
            remove_request(&tracked, "D:\\Portable\\Notepad.exe"),
            Request::HideFromConsole { .. }
        ));
        assert!(matches!(
            remove_request(&tracked, "C:\\Other\\Steam.exe"),
            Request::RemoveConsoleApp { .. }
        ));
    }

    #[test]
    fn scrim_rows_fade_and_round_the_corners() {
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(196.0, 240.0));
        let rows = scrim_rows(tile);
        // starts transparent 120 px up, ends at alpha 200 exactly on the bottom edge
        assert_eq!(rows[0], (120.0, 0.0, 196.0, 0));
        let last = *rows.last().unwrap();
        assert_eq!((last.0, last.3), (240.0, 200));
        // the bottom row is inset by the corner radius, so no square corners get painted
        assert_eq!((last.1, last.2), (16.0, 180.0));
        // monotonic gradient, every row inside the tile
        for w in rows.windows(2) {
            assert!(w[0].3 <= w[1].3);
            assert!(w[1].1 >= w[0].1 && w[1].2 <= w[0].2);
        }
    }

    #[test]
    fn cover_uv_crops_the_long_side() {
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(196.0, 240.0));
        // a 2:3 portrait is taller than the 0.82 tile: crop the top/bottom equally
        let (min, max) = cover_uv(egui::vec2(600.0, 900.0), tile);
        assert!((min.x - 0.0).abs() < 1e-6 && (max.x - 1.0).abs() < 1e-6);
        assert!(min.y > 0.0 && (min.y - (1.0 - max.y)).abs() < 1e-6);
        // a wide image is cropped left/right instead
        let (min, max) = cover_uv(egui::vec2(1920.0, 620.0), tile);
        assert!(min.x > 0.0 && (min.x - (1.0 - max.x)).abs() < 1e-6);
        assert!((min.y - 0.0).abs() < 1e-6 && (max.y - 1.0).abs() < 1e-6);
    }
}
