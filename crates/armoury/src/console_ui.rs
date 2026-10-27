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
use armoury_core::protocol::{
    ArtListing, ArtPhoto, ArtSlot, read_msg, write_msg, Request, Response,
};
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
/// The tile's corner radius, in points. The design is 16 px on a 100% display, and the
/// points grow with the display, so the corner stays in step with the tile it belongs
/// to: 16 px at 100%, 20 px at 125%, 24 px at 150%, 32 px at 200%.
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
/// The tile scrim: how far up the tile the cover art fades to black, in points. The
/// design is 120 px on a 100% display, and it grows with the display like everything
/// else: 120 px at 100%, 150 px at 125%, 180 px at 150%, 240 px at 200%.
const SCRIM_H: f32 = 120.0;

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
    "A  open      B  close      X  add app      Y  remove      Down  menu      Left / Right  scroll";
const KEY_HINT: &str =
    "Arrows / WASD move · Enter open · Esc close · Delete remove · Down opens the menu";
const EMPTY_HINT: &str = "No apps yet — press X to add one";
const LAUNCHING_HINT: &str = "starting…";
/// The hint line of the ≡ menu.
const MENU_HINT: &str = "A  choose      B  back      Up / Down  move";
/// The hint line of the artwork picker.
const PICKER_HINT: &str =
    "A  take it      B  back      Left / Right  scroll      Up / Down  slot";

/// The two entries of the ≡ menu.
const MENU_ENTRIES: [&str; 2] = ["Choose artwork…", "Use automatic again"];

/// The ≡ button's size.
const MENU_BTN: f32 = 44.0;
/// The menu panel's width and the height of one entry.
const MENU_W: f32 = 300.0;
const MENU_ROW: f32 = 40.0;
/// The picker's header: the app's name and the slot tabs.
const PICKER_HEADER: f32 = 52.0;
/// The picker's thumbnail size per slot: the 2:3 portrait and the wide hero.
const THUMB_ICON: (f32, f32) = (132.0, 198.0);
const THUMB_BG: (f32, f32) = (300.0, 97.0);
/// The room under a thumbnail for its dimensions and its style.
const CAPTION_H: f32 = 38.0;

/// How long the band waits for the daemon's answer before it gives up on a launch.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(8);

/// The band's launch state: nothing asked for, a launch in flight, or a failure to show.
#[derive(Debug, Clone, PartialEq)]
enum Launch {
    Idle,
    Pending,
    Failed(String),
}

/// What the band is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// The tile strip.
    Band,
    /// The ≡ menu.
    Menu,
    /// The artwork picker.
    Picker,
}

/// The artwork picker: the photos of the focused app, one list per slot.
struct Picker {
    /// The slot the user is on.
    slot: ArtSlot,
    /// The photos of each slot, once the daemon has answered.
    lists: HashMap<ArtSlot, ArtListing>,
    /// The highlighted photo of each slot.
    sel: HashMap<ArtSlot, usize>,
    /// A page the console asked for and has not got yet (no repeat requests).
    asked: Option<(ArtSlot, u32)>,
    /// The app the picker is for: its name and its exe.
    target: (String, String),
    /// A photo is being saved.
    busy: bool,
    /// The last thing that happened.
    note: String,
    /// The last thing that happened went wrong.
    note_bad: bool,
}

impl Picker {
    /// The photos of the slot the user is on.
    fn items(&self) -> &[ArtPhoto] {
        self.lists
            .get(&self.slot)
            .map(|l| l.items.as_slice())
            .unwrap_or(&[])
    }

    /// The page the slot shows.
    fn page(&self, slot: ArtSlot) -> u32 {
        self.lists.get(&slot).map(|l| l.page).unwrap_or(0)
    }
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
/// take the line over: the user has to see that the game did not start. The menu and the
/// picker have their own hints: they say what the keys do there.
fn hint_line(n: usize, pad: bool, launch: &Launch, view: View) -> (String, f32, egui::Color32) {
    match view {
        View::Menu => (MENU_HINT.to_string(), 14.0, MUTED),
        View::Picker => (PICKER_HINT.to_string(), 14.0, MUTED),
        View::Band => match launch {
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
        },
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
    if let Some(code) = crate::dpi::error() {
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
    // Size and position are fixed: everything is known before the window exists. The
    // band is centred on the work area and is 90% of the screen wide, and it wins over
    // the dashboard: the band's whole purpose is to launch a game from a controller
    // while the user is away, so it is never shrunk or pushed aside for the other
    // windows.
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

/// The band's rectangle inside a work area (logical points): a fixed 90% of the screen
/// wide, centred horizontally, 35% of the screen tall, its bottom edge `BOTTOM_LIFT`
/// above the work area.
///
/// The band wins over the dashboard: it is centred and 90% of the screen wide, so it
/// lies over the dashboard's lower half. That is the point of it — the band launches a
/// game from a controller while the user is away from the computer, and it must never be
/// shrunk or pushed aside for the other windows.
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

/// Logical points to physical pixels at the window's `pixels_per_point`: the ratio the
/// window itself draws at, so a point is worth this many pixels on this monitor.
fn to_pixels(points: f32, ppp: f32) -> f32 {
    points * ppp
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
/// The handle is never closed on purpose: that leak is what keeps a second launcher out,
/// so releasing it would break this guard unless the mutex got a lifetime of its own.
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

/// Why the band process leaves the screen, if it does.
///
/// `closing` is the user's own request (B, Esc, a lost controller); `minimized` is the OS
/// parking the window at (-32000, -32000). Either one ends the process: a band that is
/// gone but still alive leaves the daemon holding a launcher it can never use.
fn exit_reason(closing: bool, minimized: bool) -> Option<&'static str> {
    if closing {
        Some("the band was closed")
    } else if minimized {
        Some("the band was minimised")
    } else {
        None
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
    /// What the band is showing: the tiles, the menu or the picker.
    view: View,
    /// The highlighted entry of the ≡ menu.
    menu_sel: usize,
    /// The artwork picker.
    picker: Picker,
    /// The horizontal repeat (the band scrolls, the picker scrolls its list).
    repeat_x: pad::Repeat,
    /// The pad's vertical move last frame: the edge opens the menu, the picker switches
    /// slot.
    prev_move_y: i8,
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
    /// The picker's thumbnails, keyed by their url.
    thumbs: HashMap<String, egui::TextureHandle>,
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
    /// The thumbnails the console is waiting for.
    thumb_want: HashSet<String>,
    thumb_tx: Sender<ArtMsg>,
    thumb_rx: Receiver<ArtMsg>,
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
        let (thumb_tx, thumb_rx) = crossbeam_channel::unbounded();
        Self {
            rx,
            out,
            status: None,
            selected: 0,
            view: View::Band,
            menu_sel: 0,
            picker: Picker {
                slot: ArtSlot::Icon,
                lists: HashMap::new(),
                sel: HashMap::new(),
                asked: None,
                target: (String::new(), String::new()),
                busy: false,
                note: String::new(),
                note_bad: false,
            },
            repeat_x: pad::Repeat::default(),
            prev_move_y: 0,
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
            thumbs: HashMap::new(),
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
            thumb_want: HashSet::new(),
            thumb_tx,
            thumb_rx,
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

    /// The ≡ button: open the menu, or close it when it is already open.
    fn toggle_menu(&mut self, entries: &[(String, String)]) {
        match menu_button_action(self.view) {
            View::Band => self.close_menu(),
            _ => self.open_menu(entries),
        }
    }

    /// Open the ≡ menu. Its target is the focused app.
    fn open_menu(&mut self, entries: &[(String, String)]) {
        let Some((name, exe)) = entries.get(self.selected) else {
            return;
        };
        if self.picker.target.1 != *exe {
            // Another app: the photos on screen were that one's.
            self.picker.lists.clear();
            self.picker.sel.clear();
            self.picker.asked = None;
        }
        self.picker.target = (name.clone(), exe.clone());
        self.view = View::Menu;
        self.menu_sel = 0;
        self.repeat_x = pad::Repeat::default();
        log::info!("menu opened for {exe}");
    }

    /// Back to the band.
    fn close_menu(&mut self) {
        self.view = View::Band;
        self.repeat_x = pad::Repeat::default();
    }

    /// The picker's own state starts fresh, but the slot is remembered: the menu's
    /// "use automatic again" touches the slot the user last looked at.
    fn open_picker(&mut self) {
        self.view = View::Picker;
        self.picker.busy = false;
        self.picker.note.clear();
        self.picker.note_bad = false;
        self.picker.asked = None;
        self.repeat_x = pad::Repeat::default();
        let slot = self.picker.slot;
        self.request_listing(slot);
    }

    /// A: the highlighted entry of the menu.
    fn menu_activate(&mut self) {
        match self.menu_sel {
            0 => self.open_picker(),
            _ => self.reset_art(),
        }
    }

    /// "Use automatic again": the slot goes back to what the automatic picker chooses.
    fn reset_art(&mut self) {
        let (_, exe) = self.picker.target.clone();
        if exe.is_empty() {
            return;
        }
        self.picker.busy = true;
        self.picker.note.clear();
        self.picker.note_bad = false;
        self.send(Request::SetArt {
            exe,
            slot: self.picker.slot,
            url: String::new(),
        });
    }

    /// Show the other slot's list.
    fn switch_slot(&mut self, slot: ArtSlot) {
        if self.picker.slot == slot {
            return;
        }
        self.picker.slot = slot;
        self.repeat_x = pad::Repeat::default();
        self.request_listing(slot);
    }

    /// Ask the daemon for one page of photos. The console has no API key: the daemon does
    /// the work and answers with the listing.
    fn request_page(&mut self, slot: ArtSlot, page: u32) {
        if self.picker.asked == Some((slot, page)) {
            return; // already asked: the answer is on its way
        }
        let (_, exe) = self.picker.target.clone();
        if exe.is_empty() {
            return;
        }
        self.picker.asked = Some((slot, page));
        self.send(Request::ListArt { exe, slot, page });
    }

    /// Ask for a slot's first page, unless it is already here.
    fn request_listing(&mut self, slot: ArtSlot) {
        if self.picker.lists.contains_key(&slot) {
            return;
        }
        self.request_page(slot, 0);
    }

    /// The daemon answered with the photos of a page.
    fn on_listing(&mut self, listing: ArtListing) {
        // The slot is answered: a new page may be asked for.
        if self.picker.asked.map(|(s, _)| s == listing.slot).unwrap_or(false) {
            self.picker.asked = None;
        }
        let slot = listing.slot;
        let page = listing.page;
        let n = listing.items.len();
        // A new page starts at its first photo.
        let sel = if self.picker.page(slot) == page {
            *self.picker.sel.get(&slot).unwrap_or(&0)
        } else {
            0
        };
        self.picker
            .sel
            .insert(slot, sel.min(n.saturating_sub(1)));
        self.picker.lists.insert(slot, listing);
    }

    /// The daemon answered a pick: the file on disk changed, so the band has to read it
    /// again.
    fn on_set(&mut self, ok: bool, message: String) {
        self.picker.busy = false;
        self.picker.note = message;
        self.picker.note_bad = !ok;
        if !ok {
            return;
        }
        let (_, exe) = self.picker.target.clone();
        if exe.is_empty() {
            return;
        }
        // The texture the band holds is the old file: forget it and read the new one.
        let key = art::key(&exe);
        invalidate_art(
            &mut self.arts,
            &mut self.heros,
            &mut self.art_want,
            &mut self.hero_want,
            &key,
        );
        let entries = self.entries();
        self.ensure_art(&entries);
        self.ensure_heros(&entries);
    }

    /// A: take the highlighted photo for the slot the user is on.
    fn take_photo(&mut self, index: usize) {
        let Some(photo) = self.picker.items().get(index) else {
            return;
        };
        let url = photo.url.clone();
        let slot = self.picker.slot;
        let (_, exe) = self.picker.target.clone();
        if exe.is_empty() {
            return;
        }
        self.picker.busy = true;
        self.picker.note.clear();
        self.picker.note_bad = false;
        self.send(Request::SetArt { exe, slot, url });
    }

    /// Move the highlight along the list. At the end of a page the next page is asked for.
    fn picker_scroll(&mut self, step: i32) {
        let n = self.picker.items().len();
        if n == 0 {
            return;
        }
        let slot = self.picker.slot;
        let sel = *self.picker.sel.get(&slot).unwrap_or(&0) as i32;
        let next = sel + step;
        if next < 0 {
            // Before the first photo: the page before it.
            let page = self.picker.page(slot);
            if page > 0 {
                self.request_page(slot, page - 1);
            }
            return;
        }
        if next as usize >= n {
            // After the last photo: the page after it.
            if let Some(listing) = self.picker.lists.get(&slot) {
                let next_page = listing.page + 1;
                if (next_page as usize) * art::PAGE_LIMIT < listing.total as usize {
                    self.request_page(slot, next_page);
                }
            }
            return;
        }
        self.picker.sel.insert(slot, next as usize);
    }

    /// Queue the thumbnails of the photos on screen. The thumbnails live on a public CDN,
    /// so the console fetches them itself: it has no API key, and does not need one.
    fn ensure_thumbs(&mut self, urls: &[String]) {
        let mut missing: Vec<String> = Vec::new();
        for url in urls {
            if url.is_empty() || self.thumbs.contains_key(url) || self.thumb_want.contains(url) {
                continue;
            }
            self.thumb_want.insert(url.clone());
            missing.push(url.clone());
        }
        if missing.is_empty() {
            return;
        }
        let tx = self.thumb_tx.clone();
        thread::spawn(move || {
            for url in missing {
                let image = fetch_thumb(&url);
                if tx.send(ArtMsg { key: url, image }).is_err() {
                    return;
                }
            }
        });
    }

    /// The ≡ menu: two entries under the button. The mouse picks one, the pad moves
    /// between them and A takes the highlighted one.
    fn paint_menu(&mut self, ui: &mut egui::Ui, full: egui::Rect) {
        let panel = menu_rect(full);
        let painter = ui.painter().clone();
        painter.rect_filled(panel, 10.0, egui::Color32::from_rgb(20, 22, 28));
        painter.rect_stroke(
            panel,
            10.0,
            egui::Stroke::new(1.0, FOCUS_MID.gamma_multiply(0.5)),
            egui::StrokeKind::Inside,
        );
        let pointer_moved = ui.ctx().input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        for (i, label) in MENU_ENTRIES.iter().enumerate() {
            let row = egui::Rect::from_min_size(
                egui::pos2(panel.left() + 8.0, panel.top() + 8.0 + i as f32 * MENU_ROW),
                egui::vec2(panel.width() - 16.0, MENU_ROW),
            );
            let response = ui.interact(
                row,
                egui::Id::unique(("console-menu", i)),
                egui::Sense::click(),
            );
            if response.hovered() && pointer_moved {
                self.menu_sel = i;
            }
            if response.clicked() {
                self.menu_sel = i;
                self.menu_activate();
            }
            let on = i == self.menu_sel;
            if on {
                painter.rect_filled(row, 8.0, TILE_SEL);
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(row.left() + 4.0, row.center().y - 8.0),
                        egui::vec2(3.0, 16.0),
                    ),
                    1.5,
                    FOCUS_MID,
                );
            }
            painter.text(
                egui::pos2(row.left() + 16.0, row.center().y),
                egui::Align2::LEFT_CENTER,
                *label,
                egui::FontId::proportional(16.0),
                if on { TEXT } else { MUTED },
            );
            if i == 1 {
                // Which slot the reset touches.
                painter.text(
                    egui::pos2(row.right() - 16.0, row.center().y),
                    egui::Align2::RIGHT_CENTER,
                    slot_label(self.picker.slot),
                    egui::FontId::proportional(13.0),
                    MUTED,
                );
            }
        }
    }

    /// The artwork picker: the photos of the focused app, one list per slot.
    ///
    /// The list of the slot the user is on is a horizontal strip like the band's: the
    /// highlighted photo sits in the middle, the mouse picks one, and the pad scrolls.
    fn paint_picker(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, full: egui::Rect) {
        let painter = ui.painter().clone();
        painter.rect_filled(full, 0.0, BG);
        let slot = self.picker.slot;
        let (name, _) = self.picker.target.clone();

        // ---- the header: the app's name and the two slots ----
        painter.text(
            egui::pos2(full.left() + SIDE, full.top() + PICKER_HEADER / 2.0),
            egui::Align2::LEFT_CENTER,
            short_name(&name, 40),
            egui::FontId::proportional(24.0),
            TEXT,
        );
        let tab_w = 120.0;
        for (i, tab) in [ArtSlot::Icon, ArtSlot::Background].iter().enumerate() {
            let r = egui::Rect::from_center_size(
                egui::pos2(
                    full.right() - SIDE - tab_w / 2.0 - i as f32 * (tab_w + 8.0),
                    full.top() + PICKER_HEADER / 2.0,
                ),
                egui::vec2(tab_w, 30.0),
            );
            let on = *tab == slot;
            // The mouse can pick the other list too.
            if ui.interact(
                r,
                egui::Id::unique(("console-pick-tab", i)),
                egui::Sense::click(),
            )
            .clicked()
            {
                self.switch_slot(*tab);
            }
            painter.rect_filled(
                r,
                8.0,
                if on {
                    TILE_SEL
                } else {
                    egui::Color32::from_rgb(18, 20, 26)
                },
            );
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                slot_label(*tab),
                egui::FontId::proportional(14.0),
                if on { TEXT } else { MUTED },
            );
            if on {
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(r.left() + 12.0, r.bottom() - 3.0),
                        egui::vec2(r.width() - 24.0, 2.0),
                    ),
                    1.0,
                    FOCUS_MID,
                );
            }
        }

        // ---- the list of photos ----
        let hint_y = full.bottom() - 24.0;
        // The listing is only borrowed for what the paint needs: the loop below changes
        // the picker's own state.
        let (n, listing_ok, listing_error) = match self.picker.lists.get(&slot) {
            Some(l) => (l.items.len(), l.ok, l.error.clone()),
            None => (0, false, String::new()),
        };
        let sel = *self.picker.sel.get(&slot).unwrap_or(&0);
        let thumb = thumb_size(slot);
        let pitch = thumb.x + GAP;
        let view_w = full.width() - 2.0 * SIDE;
        let view_left = full.left() + SIDE;
        let total = n as f32 * pitch - GAP;
        let target_off = if n == 0 {
            0.0
        } else if n <= visible_tiles(view_w, thumb.x) {
            -(view_w - total) / 2.0
        } else {
            (sel as f32 * pitch + thumb.x / 2.0 - view_w / 2.0)
                .clamp(0.0, (total - view_w).max(0.0))
        };
        let scroll_time = if self.reduced { 0.001 } else { 0.22 };
        let off = ctx.animate_value_with_time(
            egui::Id::unique("console-pick-scroll"),
            target_off,
            scroll_time,
        );
        let strip_top = full.top() + PICKER_HEADER + 16.0;
        let strip_bottom = hint_y - 44.0;
        let block_h = thumb.y + CAPTION_H;
        let thumb_top = strip_top + ((strip_bottom - strip_top - block_h) / 2.0).max(0.0);

        // Only the photos on screen are loaded and painted.
        let first = (((off - 60.0) / pitch).floor().max(0.0) as usize).min(n);
        let last = (((off + view_w + 60.0) / pitch).ceil().max(0.0) as usize).min(n);
        let shown: Vec<(usize, ArtPhoto)> = match self.picker.lists.get(&slot) {
            Some(l) => l.items[first..last]
                .iter()
                .enumerate()
                .map(|(k, p)| (first + k, p.clone()))
                .collect(),
            None => Vec::new(),
        };
        let urls: Vec<String> = shown.iter().map(|(_, p)| p.thumb.clone()).collect();
        self.ensure_thumbs(&urls);

        let pointer_moved = ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        for (i, photo) in shown {
            let rect = egui::Rect::from_min_size(
                egui::pos2(view_left + i as f32 * pitch - off, thumb_top),
                thumb,
            );
            let response = ui.interact(
                rect,
                egui::Id::unique(("console-pick", i)),
                egui::Sense::click(),
            );
            if response.hovered() && pointer_moved {
                self.picker.sel.insert(slot, i);
            }
            if response.clicked() {
                self.picker.sel.insert(slot, i);
                self.take_photo(i);
            }
            let on = *self.picker.sel.get(&slot).unwrap_or(&0) == i;
            match self.thumbs.get(&photo.thumb) {
                Some(tex) => {
                    let (uv_min, uv_max) = cover_uv(tex.size_vec2(), rect);
                    painter.add(
                        RectShape::filled(rect, CornerRadius::from(8.0), egui::Color32::WHITE)
                            .with_texture(tex.id(), egui::Rect::from_min_max(uv_min, uv_max)),
                    );
                }
                None => {
                    // The thumbnail is not here yet: the box says what the photo is.
                    painter.rect_filled(rect, 8.0, TILE);
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("{}x{}", photo.width, photo.height),
                        egui::FontId::proportional(14.0),
                        MUTED,
                    );
                }
            }
            if on {
                paint_focus_glow(&painter, rect, 0.9);
                paint_focus_ring(&painter, rect, 0.9);
            }
            // The caption: the dimensions and the style, under every photo.
            let cap_y = rect.bottom() + 13.0;
            let color = if on { TEXT } else { MUTED };
            painter.text(
                egui::pos2(rect.center().x, cap_y),
                egui::Align2::CENTER_CENTER,
                format!("{}x{}", photo.width, photo.height),
                egui::FontId::proportional(13.0),
                color,
            );
            let style = if photo.style.is_empty() {
                "—".to_string()
            } else {
                photo.style.clone()
            };
            painter.text(
                egui::pos2(rect.center().x, cap_y + 16.0),
                egui::Align2::CENTER_CENTER,
                style,
                egui::FontId::proportional(13.0),
                color.gamma_multiply(0.85),
            );
        }

        // ---- the state of the list ----
        let center = egui::pos2(full.center().x, thumb_top + thumb.y / 2.0);
        if n == 0 {
            if listing_ok {
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    "no photos for this game",
                    egui::FontId::proportional(18.0),
                    MUTED,
                );
            } else if listing_error.is_empty() {
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    "loading…",
                    egui::FontId::proportional(18.0),
                    MUTED,
                );
            } else {
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    shorten(&listing_error, 80),
                    egui::FontId::proportional(16.0),
                    WARN,
                );
            }
        }

        // ---- the note and the hint line ----
        let note = if self.picker.busy {
            Some("saving…".to_string())
        } else if self.picker.note.is_empty() {
            None
        } else {
            Some(self.picker.note.clone())
        };
        if let Some(note) = note {
            painter.text(
                egui::pos2(full.center().x, hint_y - 26.0),
                egui::Align2::CENTER_CENTER,
                shorten(&note, 120),
                egui::FontId::proportional(15.0),
                if self.picker.note_bad { WARN } else { TEXT },
            );
        }
        painter.text(
            egui::pos2(full.center().x, hint_y),
            egui::Align2::CENTER_CENTER,
            PICKER_HINT,
            egui::FontId::proportional(14.0),
            MUTED,
        );
    }
}

/// Forget a key's textures and wishes, so the next read picks up the file on disk.
///
/// The band keeps the art it has loaded: without this a new file is invisible until the
/// band restarts.
fn invalidate_art<T>(
    arts: &mut HashMap<String, T>,
    heros: &mut HashMap<String, T>,
    art_want: &mut HashSet<String>,
    hero_want: &mut HashSet<String>,
    key: &str,
) {
    arts.remove(key);
    heros.remove(key);
    art_want.remove(key);
    hero_want.remove(key);
}

/// The client the thumbnails are fetched with: the process-wide one (see `net::agent`).
///
/// The band is a process of its own, so this is an agent of its own too — but the same one
/// the catalogue and the cover art use: the Windows certificate store is the root store, and
/// the connections are pooled.
fn client() -> &'static ureq::Agent {
    crate::net::agent()
}

/// Download one thumbnail and decode it.
fn fetch_thumb(url: &str) -> Option<(Vec<u8>, u32, u32)> {
    let bytes = client()
        .get(url)
        .call()
        .ok()?
        .body_mut()
        .read_to_vec()
        .ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    // A thumbnail is small; twice the biggest box keeps it crisp.
    let img = img.resize(360, 360, image::imageops::FilterType::Triangle);
    let rgba = img.to_rgba8();
    Some((rgba.as_raw().clone(), rgba.width(), rgba.height()))
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
    /// The band is gone when the OS parks the window at (-32000, -32000): the ui stops
    /// running, so nothing would ever close it and the process would stay alive with a
    /// dead viewport. A minimised band is a closed band, and it goes the same way.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let minimized = ctx
            .input(|i| {
                i.raw
                    .viewports
                    .get(&egui::ViewportId::ROOT)
                    .and_then(|v| v.minimized)
            })
            .unwrap_or(false);
        if let Some(reason) = exit_reason(false, minimized) {
            log::info!("{reason} — exiting");
            std::process::exit(0);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.first_frame {
            crate::fonts::install_fonts(&ctx);
        }
        ctx.request_repaint(); // the pad is polled per frame

        // ---- daemon events ----
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Response(resp) => {
                    // The answers to the launch and to the artwork requests ride the
                    // console-op variant; the markers tell them apart from the add/remove
                    // answers.
                    let ack = armoury_core::protocol::as_launch_ack(&resp)
                        .map(|(ok, text)| (ok, text.to_string()));
                    let listing = armoury_core::protocol::as_art_listing(&resp);
                    let set = armoury_core::protocol::as_art_set(&resp)
                        .map(|(ok, text)| (ok, text.to_string()));
                    match ack {
                        Some((ok, message)) => {
                            log::info!("launch result: ok={ok} {message}");
                            if apply_launch_ack(&mut self.launch, ok, message) {
                                self.leaving = true; // the game started: slide away
                            }
                        }
                        None => {
                            if let Some(listing) = listing {
                                self.on_listing(listing);
                            } else if let Some((ok, message)) = set {
                                self.on_set(ok, message);
                            } else {
                                match resp {
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
                                }
                            }
                        }
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

        // ---- the picker's thumbnails from the worker ----
        while let Ok(m) = self.thumb_rx.try_recv() {
            match m.image {
                Some((rgba, w, h)) => {
                    let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    let handle = ctx.load_texture(
                        format!("console-thumb:{}", m.key),
                        img,
                        egui::TextureOptions::LINEAR,
                    );
                    self.thumbs.insert(m.key, handle);
                }
                None => {
                    // The thumbnail is not there: forget the wish so it can be tried again.
                    self.thumb_want.remove(&m.key);
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
            self.pad_slot = None;
            if self.view == View::Band {
                log::info!("controller disconnected — closing");
                self.leaving = true;
            } else {
                // The menu and the picker stay open: the mouse still works, and a pad that
                // comes back takes over.
                log::info!("controller disconnected — the menu stays open");
            }
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
        let step = self.repeat_x.tick(dir, Instant::now());
        // The pad's vertical move: up jumps to the first app, down opens the ≡ menu.
        let move_y = pad_state.map(|p| p.move_y).unwrap_or(0);
        let down_edge = move_y > 0 && self.prev_move_y <= 0;
        let up_edge = move_y < 0 && self.prev_move_y >= 0;

        let a = pad_state.map(|p| p.a).unwrap_or(false);
        let b = pad_state.map(|p| p.b).unwrap_or(false);
        let x = pad_state.map(|p| p.x).unwrap_or(false);
        let y = pad_state.map(|p| p.y).unwrap_or(false);

        // Each view has its own keys: while the menu or the picker is open the band must
        // not launch a game or take the app away.
        match self.view {
            View::Band => {
                if step != 0 {
                    self.selected = wrap(self.selected, step.into(), n);
                }
                // One row: up jumps to the first app, down opens the menu.
                if key_up || up_edge {
                    self.selected = 0;
                }
                if key_down || down_edge {
                    self.open_menu(&entries);
                }
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
            }
            View::Menu => {
                // Two entries: the move is an edge, not a held direction.
                if up_edge || key_up {
                    self.menu_sel = wrap(self.menu_sel, -1, MENU_ENTRIES.len());
                }
                if down_edge || key_down {
                    self.menu_sel = wrap(self.menu_sel, 1, MENU_ENTRIES.len());
                }
                if (a && !self.prev_a) || key_enter {
                    self.menu_activate();
                }
                if (b && !self.prev_b) || key_esc {
                    self.close_menu();
                }
            }
            View::Picker => {
                if step != 0 {
                    self.picker_scroll(step.into());
                }
                // Up and down walk through the two lists: the icon and the background.
                if up_edge || key_up {
                    self.switch_slot(ArtSlot::Icon);
                }
                if down_edge || key_down {
                    self.switch_slot(ArtSlot::Background);
                }
                if (a && !self.prev_a) || key_enter {
                    let sel = *self.picker.sel.get(&self.picker.slot).unwrap_or(&0);
                    self.take_photo(sel);
                }
                if (b && !self.prev_b) || key_esc {
                    self.view = View::Menu;
                }
            }
        }
        self.prev_a = a;
        self.prev_b = b;
        self.prev_x = x;
        self.prev_y = y;
        self.prev_move_y = move_y;

        if !self.entering && !self.leaving {
            self.entering = true;
        }
        let slow = if self.reduced { 0.001 } else { 0.28 };
        let enter = ctx.animate_bool_with_time(egui::Id::unique("console-enter"), self.entering && !self.leaving, slow);
        // The band is on its way out: the process goes with it. A viewport close only asks
        // the event loop to wind down, and a window the OS took away never gets that far —
        // the daemon would keep a launcher it can never use again.
        if self.leaving && enter < 0.03 {
            if let Some(reason) = exit_reason(true, false) {
                log::info!("{reason} — exiting");
                std::process::exit(0);
            }
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
        let (hint, hint_font, hint_color) =
            hint_line(n, self.pad_slot.is_some(), &self.launch, self.view);
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
            // The tiles only take clicks on the band: the menu and the picker float above
            // them, and a click there must not launch a game.
            let response = if self.view == View::Band {
                Some(ui.interact(rect, egui::Id::unique(("console-tile", i)), egui::Sense::click()))
            } else {
                None
            };
            let hovered = response.as_ref().is_some_and(|r| r.hovered());
            if hovered && pointer_moved {
                self.selected = i;
            }
            if response.as_ref().is_some_and(|r| r.clicked()) {
                clicked = Some(i);
            }
            visible.push((i, rect, hovered, sel));
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
        if self.view != View::Picker {
            ui.painter().text(
                egui::pos2(full.center().x, hint_y),
                egui::Align2::CENTER_CENTER,
                hint,
                egui::FontId::proportional(hint_font),
                hint_color,
            );
        }

        // ---- the ≡ button, the menu and the picker ----
        if self.view != View::Picker {
            let btn = menu_button_rect(full);
            let response = ui.interact(
                btn,
                egui::Id::unique("console-menu-btn"),
                egui::Sense::click(),
            );
            if response.clicked() {
                self.toggle_menu(&entries);
            }
            paint_menu_button(
                ui.painter(),
                btn,
                response.hovered(),
                response.is_pointer_button_down_on(),
            );
        }
        if self.view == View::Menu {
            self.paint_menu(ui, full);
        }
        if self.view == View::Picker {
            self.paint_picker(ui, &ctx, full);
        }
    }
}

/// What the ≡ button does: it opens the menu, or closes it when it is already open.
fn menu_button_action(view: View) -> View {
    match view {
        View::Menu => View::Band,
        _ => View::Menu,
    }
}

/// The ≡ button's place on the band: the top-left corner, above the tiles.
fn menu_button_rect(full: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(full.left() + SIDE, full.top() + 8.0),
        egui::vec2(MENU_BTN, MENU_BTN),
    )
}

/// The menu panel's place: right under the ≡ button.
fn menu_rect(full: egui::Rect) -> egui::Rect {
    let btn = menu_button_rect(full);
    egui::Rect::from_min_size(
        egui::pos2(btn.left(), btn.bottom() + 8.0),
        egui::vec2(MENU_W, MENU_ROW * MENU_ENTRIES.len() as f32 + 16.0),
    )
}

/// The ≡ button: three bars in a rounded box, lit when the pointer is on it.
fn paint_menu_button(painter: &egui::Painter, rect: egui::Rect, hovered: bool, pressed: bool) {
    let fill = if pressed {
        TILE_SEL
    } else if hovered {
        egui::Color32::from_rgb(32, 35, 44)
    } else {
        egui::Color32::from_rgb(20, 22, 28)
    };
    painter.rect_filled(rect, 12.0, fill);
    painter.rect_stroke(
        rect,
        12.0,
        egui::Stroke::new(
            1.0,
            if hovered {
                FOCUS_MID.gamma_multiply(0.8)
            } else {
                MUTED.gamma_multiply(0.45)
            },
        ),
        egui::StrokeKind::Inside,
    );
    let bar = rect.width() * 0.48;
    for i in 0..3 {
        let y = rect.center().y + (i as f32 - 1.0) * 7.0;
        painter.rect_filled(
            egui::Rect::from_center_size(
                egui::pos2(rect.center().x, y),
                egui::vec2(bar, 2.0),
            ),
            1.0,
            TEXT.gamma_multiply(if hovered { 0.95 } else { 0.7 }),
        );
    }
}

/// The slot's name on screen.
fn slot_label(slot: ArtSlot) -> &'static str {
    match slot {
        ArtSlot::Icon => "Icon",
        ArtSlot::Background => "Background",
    }
}

/// The picker's thumbnail size for a slot: the 2:3 portrait and the wide hero.
fn thumb_size(slot: ArtSlot) -> egui::Vec2 {
    match slot {
        ArtSlot::Icon => egui::vec2(THUMB_ICON.0, THUMB_ICON.1),
        ArtSlot::Background => egui::vec2(THUMB_BG.0, THUMB_BG.1),
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
/// gradient, transparent `SCRIM_H` above the bottom edge up to black_alpha(200) at it. The
/// bottom rows trace the tile's corner arcs, so the fill stays rounded instead of squaring off.
fn paint_scrim(painter: &egui::Painter, rect: egui::Rect) {
    let mut m = Mesh::default();
    for (y, left, right, a) in scrim_rows(rect, painter.pixels_per_point()) {
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

/// Steps used to trace a corner arc of `radius` points.
///
/// The radius comes in points, so it is turned into pixels first: the arc is sampled in
/// physical pixels, so the step count follows the pixel radius — eight steps per 16 px of
/// radius, and never fewer than eight. At 200% the corner is twice as wide in pixels, and
/// eight steps would show up as a chunky edge.
fn arc_steps(radius: f32, ppp: f32) -> usize {
    ((to_pixels(radius, ppp) / 16.0) * 8.0).round().max(8.0) as usize
}

/// The scrim's vertex rows, top to bottom as `(y, left_x, right_x, alpha)`. Straight sides above
/// the corners; the last rows inset by the corner arc so they never paint past the rounding.
/// `ppp` is the window's pixel ratio: the corner is sampled in pixels, so it takes more rows
/// on a bigger display.
fn scrim_rows(rect: egui::Rect, ppp: f32) -> Vec<(f32, f32, f32, u8)> {
    const TOP_A: f32 = 200.0; // alpha at the bottom edge
    let (r, top, corner) = (
        TILE_RADIUS,
        rect.bottom() - SCRIM_H,
        rect.bottom() - TILE_RADIUS,
    );
    let steps = arc_steps(r, ppp);
    let alpha = |y: f32| ((y - top) / SCRIM_H * TOP_A).clamp(0.0, 255.0) as u8;
    let mut rows = vec![
        (top, rect.left(), rect.right(), alpha(top)),
        (corner, rect.left(), rect.right(), alpha(corner)),
    ];
    for k in 1..=steps {
        let y = corner + r * k as f32 / steps as f32;
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

/// The outline's points: `steps` per corner arc, clockwise from the top edge.
///
/// The step count is the caller's: the focus ring joins its two edges point by point, so
/// both of them have to be sampled with the same number of steps.
fn outline_points(rect: egui::Rect, r: f32, steps: usize) -> Vec<egui::Pos2> {
    let (l, t, rgt, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let corners = [
        (egui::pos2(rgt - r, t + r), -90.0),
        (egui::pos2(rgt - r, b - r), 0.0),
        (egui::pos2(l + r, b - r), 90.0),
        (egui::pos2(l + r, t + r), 180.0),
    ];
    let mut out = Vec::with_capacity(4 * (steps + 1));
    for (center, start) in corners {
        for k in 0..=steps {
            let a = (start + 90.0 * k as f32 / steps as f32).to_radians();
            out.push(egui::pos2(center.x + r * a.cos(), center.y + r * a.sin()));
        }
    }
    out
}

/// The corner radius an outline gets: the radius asked for, or the largest the rectangle
/// can carry.
fn corner_radius(rect: egui::Rect, radius: f32) -> f32 {
    radius
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0)
        .max(0.0)
}

/// The focus border: a mesh strip around the rounded tile, violet → magenta left to right.
fn paint_focus_ring(painter: &egui::Painter, rect: egui::Rect, alpha: f32) {
    let ppp = painter.pixels_per_point();
    let outer_rect = rect.expand(1.0);
    let inner_rect = rect.expand(-1.0);
    let outer_r = corner_radius(outer_rect, TILE_RADIUS + 1.0);
    let inner_r = corner_radius(inner_rect, TILE_RADIUS - 1.0);
    // The two edges are joined point by point, so they are sampled with the same number
    // of steps: the outer edge has the longer arc and sets the count. Each edge with a
    // count of its own would leave the shorter one short of points, and the mesh would
    // index past its end.
    let steps = arc_steps(outer_r, ppp);
    let outer = outline_points(outer_rect, outer_r, steps);
    let inner = outline_points(inner_rect, inner_r, steps);
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
    use ureq::tls::RootCerts;

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
    fn a_minimised_band_is_a_closed_band() {
        // the band is up: it stays
        assert_eq!(exit_reason(false, false), None);
        // the user closed it: the process goes
        assert_eq!(exit_reason(true, false), Some("the band was closed"));
        // the OS parked it at (-32000, -32000): that is a closed band as well
        assert_eq!(exit_reason(false, true), Some("the band was minimised"));
    }

    #[test]
    fn the_hint_line_shows_the_launch_state() {
        // the usual hints
        let (text, _, color) = hint_line(2, false, &Launch::Idle, View::Band);
        assert_eq!(text, KEY_HINT);
        assert_eq!(color, MUTED);
        // a launch in flight
        let (text, _, _) = hint_line(2, false, &Launch::Pending, View::Band);
        assert_eq!(text, LAUNCHING_HINT);
        // a failure: the reason, in the warning colour
        let (text, _, color) =
            hint_line(2, false, &Launch::Failed("it did not start".into()), View::Band);
        assert_eq!(text, "it did not start");
        assert_eq!(color, WARN);
    }

    #[test]
    fn the_menu_and_the_picker_have_their_own_hints() {
        // the band is not behind them: they say what the keys do there
        let (text, _, color) = hint_line(2, true, &Launch::Idle, View::Menu);
        assert_eq!(text, MENU_HINT);
        assert_eq!(color, MUTED);
        let (text, _, _) = hint_line(2, true, &Launch::Idle, View::Picker);
        assert_eq!(text, PICKER_HINT);
    }

    #[test]
    fn the_menu_button_opens_and_closes_the_menu() {
        // the button toggles: it opens the menu, and closes it again
        assert_eq!(menu_button_action(View::Band), View::Menu);
        assert_eq!(menu_button_action(View::Menu), View::Band);
    }

    #[test]
    fn the_menu_button_sits_above_the_tiles() {
        let band = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1843.0, 403.0));
        let btn = menu_button_rect(band);
        assert!(btn.top() >= band.top());
        assert!(btn.right() < band.right());
        // the panel hangs under the button
        let panel = menu_rect(band);
        assert!(panel.top() >= btn.bottom());
        assert!(panel.left() == btn.left());
        // one row per entry
        assert_eq!(panel.height(), MENU_ROW * 2.0 + 16.0);
    }

    #[test]
    fn a_forgotten_key_is_read_again() {
        let mut arts = HashMap::new();
        let mut heros = HashMap::new();
        let mut art_want = HashSet::new();
        let mut hero_want = HashSet::new();
        arts.insert("aa".to_string(), 1u32);
        heros.insert("aa".to_string(), 1u32);
        art_want.insert("aa".to_string());
        hero_want.insert("aa".to_string());

        invalidate_art(&mut arts, &mut heros, &mut art_want, &mut hero_want, "aa");
        assert!(arts.is_empty() && heros.is_empty());
        assert!(art_want.is_empty() && hero_want.is_empty());

        // another key is left alone
        arts.insert("bb".to_string(), 1u32);
        invalidate_art(&mut arts, &mut heros, &mut art_want, &mut hero_want, "aa");
        assert_eq!(arts.len(), 1);
    }

    #[test]
    fn the_picker_scrolls_within_a_page_and_asks_for_the_next_one() {
        // a page of three photos, and a second page behind it
        let photos: Vec<ArtPhoto> = (0..3)
            .map(|i| ArtPhoto {
                id: i,
                url: format!("https://cdn.example/{i}.png"),
                thumb: String::new(),
                width: 600,
                height: 900,
                style: "Alternate".into(),
            })
            .collect();
        let listing = ArtListing {
            ok: true,
            slot: ArtSlot::Icon,
            page: 0,
            total: 3 * art::PAGE_LIMIT as u32,
            items: photos,
            error: String::new(),
        };
        let mut picker = Picker {
            slot: ArtSlot::Icon,
            lists: HashMap::from([(ArtSlot::Icon, listing)]),
            sel: HashMap::from([(ArtSlot::Icon, 0usize)]),
            asked: None,
            target: (String::new(), String::new()),
            busy: false,
            note: String::new(),
            note_bad: false,
        };
        // the highlight walks the page
        picker.sel.insert(ArtSlot::Icon, 1);
        let list = picker.lists.get(&ArtSlot::Icon).expect("the listing");
        let sel = *picker.sel.get(&ArtSlot::Icon).unwrap_or(&0);
        assert_eq!(list.items[sel].id, 1);
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

    /// The primary monitor at 125%: 2560x1440 physical is 2048x1152 points, and the work
    /// area is the same width with the taskbar off the bottom.
    fn primary_work() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1104.0))
    }

    /// The primary screen in logical points.
    fn primary_screen() -> egui::Vec2 {
        egui::vec2(2048.0, 1152.0)
    }

    #[test]
    fn the_band_is_90_percent_of_the_screen_and_centred() {
        let work = primary_work();
        let screen = primary_screen();
        let band = band_rect(work, screen);
        // 90% of the screen's width, 35% of the screen's height
        assert!((band.width() - screen.x * BAND_W_FRAC).abs() < 1e-3);
        assert!((band.height() - screen.y * BAND_H_FRAC).abs() < 1e-3);
        assert!((band.width() - 1843.2).abs() < 1e-3);
        assert!((band.height() - 403.2).abs() < 1e-3);
        // in physical pixels: 90% of the 2560-wide primary, 35% of its 1440
        assert!((band.width() * 1.25 - 2304.0).abs() < 1e-3);
        assert!((band.height() * 1.25 - 504.0).abs() < 1e-3);
        // centred: the work area is the screen's width here, so the band sits on the
        // screen's centre (1024 points, 1280 physical pixels)
        assert!((band.center().x - work.center().x).abs() < 1e-3);
        assert!((band.center().x - screen.x / 2.0).abs() < 1e-3);
        assert!((band.center().x * 1.25 - 1280.0).abs() < 1e-3);
        // docked 8 points above the work area's bottom edge
        assert!((band.bottom() - (work.bottom() - BOTTOM_LIFT)).abs() < 1e-3);
        // the whole rectangle in physical pixels: L=128, T=866, R=2432, B=1370
        assert!((band.left() * 1.25 - 128.0).abs() < 1e-3);
        assert!((band.top() * 1.25 - 866.0).abs() < 1e-3);
        assert!((band.right() * 1.25 - 2432.0).abs() < 1e-3);
        assert!((band.bottom() * 1.25 - 1370.0).abs() < 1e-3);
        // a shorter work area moves the band, it does not resize it
        let shorter_work =
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2048.0, 1000.0));
        let shorter = band_rect(shorter_work, screen);
        assert!((shorter.height() - 403.2).abs() < 1e-3);
        assert!((shorter.width() - band.width()).abs() < 1e-3);
        assert!((shorter.bottom() - 992.0).abs() < 1e-3);
        // the same on a work area that does not start at the screen's left edge: the
        // band keeps its size and its centring on the work area
        let shifted_work = work.translate(egui::vec2(-1000.0, 40.0));
        let shifted = band_rect(shifted_work, screen);
        assert!((shifted.center().x - shifted_work.center().x).abs() < 1e-3);
        assert!((shifted.height() - 403.2).abs() < 1e-3);
        assert!((shifted.width() - band.width()).abs() < 1e-3);
    }

    #[test]
    fn the_band_wins_over_the_dashboard() {
        // The dashboard in its corner: 380-420 points wide (475-523 physical pixels at
        // 125%), flush to the right and the bottom of the work area.
        let work = primary_work();
        let screen = primary_screen();
        let band = band_rect(work, screen);
        for width in [380.0, 418.0, 460.0] {
            let dashboard = egui::Rect::from_min_max(
                egui::pos2(work.right() - width, work.bottom() - 500.0),
                egui::pos2(work.right(), work.bottom()),
            );
            // The band is not moved aside for the dashboard: it is centred and 90% of
            // the screen wide, so it lies over the dashboard's lower half.
            assert!(
                band.intersects(dashboard),
                "the band lies over the dashboard: {band:?} / {dashboard:?}"
            );
            assert!(
                band.right() > dashboard.left(),
                "the band is not narrowed for the dashboard: {band:?} / {dashboard:?}"
            );
        }
        // A dashboard that was dragged into the middle of the screen is covered too.
        let dragged = egui::Rect::from_min_max(
            egui::pos2(800.0, work.bottom() - 500.0),
            egui::pos2(1218.0, work.bottom()),
        );
        assert!(
            band.intersects(dragged),
            "a dragged dashboard is covered too: {band:?} / {dragged:?}"
        );
        // The dashboard's title row stays clear of the band: the gear, Pause and ✕
        // are still clickable. The band's top edge is at 692.8 points, and the title
        // row of a dashboard this tall ends well above it.
        let title_row = egui::Rect::from_min_max(
            egui::pos2(work.right() - 418.0, work.bottom() - 500.0),
            egui::pos2(work.right(), work.bottom() - 460.0),
        );
        assert!(
            title_row.bottom() <= band.top(),
            "the dashboard's title row stays clear: {title_row:?} / {band:?}"
        );
    }

    #[test]
    fn the_band_still_holds_its_tiles() {
        let work = primary_work();
        let screen = primary_screen();
        let band = band_rect(work, screen);
        let tile = tile_size(screen.y);
        let inner = band.width() - 2.0 * SIDE;
        let n = visible_tiles(inner, tile.x);
        assert!(n >= 5, "the band must still show its tiles: {n}");
        // and the row sits inside the band, not beside it.
        let row = egui::Rect::from_center_size(
            egui::pos2(band.center().x, row_center_y(band)),
            egui::vec2(n as f32 * (tile.x + GAP) - GAP, tile.y),
        );
        assert!(
            band.contains_rect(row),
            "the tiles stay inside the band: {row:?} / {band:?}"
        );
        // One tile always fits, whatever the screen is: the band is useless without it.
        let small_screen = egui::vec2(640.0, 480.0);
        let small = band_rect(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 440.0)),
            small_screen,
        );
        assert!(
            small.width() >= tile_size(small_screen.y).x,
            "the band keeps one tile: {small:?}"
        );
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
        let rows = scrim_rows(tile, 1.25);
        // starts transparent 120 pt up (150 px at 125%), ends at alpha 200 exactly on the
        // bottom edge
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
    fn the_corner_radius_grows_with_the_display() {
        // The corner is drawn in points, so the same 16 points is worth more pixels on a
        // bigger display: the corner grows with the tile it belongs to.
        assert!((to_pixels(TILE_RADIUS, 1.0) - 16.0).abs() < 1e-3);
        assert!((to_pixels(TILE_RADIUS, 1.25) - 20.0).abs() < 1e-3);
        assert!((to_pixels(TILE_RADIUS, 1.5) - 24.0).abs() < 1e-3);
        assert!((to_pixels(TILE_RADIUS, 2.0) - 32.0).abs() < 1e-3);
        // the scrim insets by that very radius in points, so the fill never paints past
        // the tile's own rounding
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(196.0, 240.0));
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let rows = scrim_rows(tile, ppp);
            let last = *rows.last().unwrap();
            assert!((last.1 - TILE_RADIUS).abs() < 1e-3);
            assert!((last.2 - (196.0 - TILE_RADIUS)).abs() < 1e-3);
        }
    }

    #[test]
    fn the_scrim_fade_grows_with_the_display() {
        // The fade's height is points too: 120 points is 120 px at 100% and 240 px at 200%.
        assert!((to_pixels(SCRIM_H, 1.0) - 120.0).abs() < 1e-3);
        assert!((to_pixels(SCRIM_H, 1.25) - 150.0).abs() < 1e-3);
        assert!((to_pixels(SCRIM_H, 1.5) - 180.0).abs() < 1e-3);
        assert!((to_pixels(SCRIM_H, 2.0) - 240.0).abs() < 1e-3);
        // and the fade always starts the same distance above the tile's bottom edge
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(196.0, 240.0));
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let rows = scrim_rows(tile, ppp);
            assert!((rows[0].0 - (tile.bottom() - SCRIM_H)).abs() < 1e-3);
        }
    }

    #[test]
    fn the_corner_is_sampled_finer_on_a_bigger_display() {
        // Eight steps per 16 px of radius, and never fewer than eight.
        assert_eq!(arc_steps(TILE_RADIUS, 1.0), 8);
        assert_eq!(arc_steps(TILE_RADIUS, 1.25), 10);
        assert_eq!(arc_steps(TILE_RADIUS, 1.5), 12);
        assert_eq!(arc_steps(TILE_RADIUS, 2.0), 16);
        // a radius of nothing still gets a smooth-enough arc
        assert_eq!(arc_steps(0.0, 2.0), 8);
        // the arc is stepped every two physical pixels, whatever the display's scale
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let steps = arc_steps(TILE_RADIUS, ppp);
            assert!((to_pixels(TILE_RADIUS, ppp) / steps as f32 - 2.0).abs() < 1e-3);
        }
        // the scrim takes one row per step, the focus ring one point per step
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(196.0, 240.0));
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let steps = arc_steps(TILE_RADIUS, ppp);
            assert_eq!(scrim_rows(tile, ppp).len(), 2 + steps);
            assert_eq!(
                outline_points(tile, TILE_RADIUS, steps).len(),
                4 * (steps + 1)
            );
        }
    }

    #[test]
    fn the_focus_ring_draws_at_every_scale() {
        // The ring is a strip between two outlines, and the mesh joins them point by
        // point: both have to be sampled with the same number of steps. Each edge with a
        // count of its own left the shorter one short of points, and the mesh indexed
        // past its end — the band died on its first frame with the focus ring on it.
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(197.568, 241.92));
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(ppp);
            let out = ctx.run_pass(egui::RawInput::default(), |ctx| {
                let painter = ctx.layer_painter(egui::LayerId::background());
                paint_focus_ring(&painter, tile, 1.0);
            });
            assert!(!out.shapes.is_empty(), "the ring must draw at {ppp}");
            out.drop_without_applying_deltas();
        }
    }

    #[test]
    fn the_tile_geometry_did_not_move() {
        // This is about the sampling, not about the shapes: the tiles and the scrim's
        // place on the tile are exactly where they were. The band itself did move: it
        // is now 90% of the screen wide, so its numbers below are the new ones.
        let work = primary_work();
        let band = band_rect(work, primary_screen());
        assert!((band.width() - 1843.2).abs() < 1e-3);
        assert!((band.height() - 403.2).abs() < 1e-3);
        assert!((band.bottom() - (work.bottom() - BOTTOM_LIFT)).abs() < 1e-3);
        let tile = tile_size(primary_screen().y);
        assert!((tile.x - 197.568).abs() < 1e-3);
        assert!((tile.y - 241.92).abs() < 1e-3);
        assert!((TILE_RADIUS - 16.0).abs() < 1e-6);
        assert!((SCRIM_H - 120.0).abs() < 1e-6);
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

    #[test]
    fn the_thumbnails_are_fetched_through_the_windows_certificate_store() {
        // The bug this guards: the thumbnails had a client of their own, validated against
        // Mozilla's bundled roots, so a machine with a corporate CA could fetch the catalogue
        // but not the band.
        let roots = client().config().tls_config().root_certs();
        assert!(
            matches!(roots, RootCerts::PlatformVerifier),
            "the thumbnails must be validated against the Windows certificate store, not {roots:?}"
        );
    }

    #[test]
    fn the_thumbnails_share_the_client_with_the_catalogue() {
        // One client, one connection pool: a client of its own would rebuild the rustls config
        // and the platform verifier for every single thumbnail.
        assert!(
            std::ptr::eq::<ureq::Agent>(client(), crate::net::agent()),
            "the thumbnails must share the client's connection pool with the catalogue"
        );
    }
}
