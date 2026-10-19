//! Idle overlay: black windows over every active monitor plus a mute of every audio
//! output device, with a ledger so exiting puts back exactly what we changed.

use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BLACK_BRUSH, EndPaint, EnumDisplayMonitors, FillRect, GetMonitorInfoW,
    GetStockObject, HBRUSH, HDC, HMONITOR, MONITORINFO, PAINTSTRUCT,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, HWND_TOPMOST, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, TranslateMessage,
    WM_ERASEBKGND, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE,
};
use windows::core::{BOOL, PCWSTR, w};

/// The ledger format this build writes and understands.
const LEDGER_SCHEMA: u32 = 1;

/// An output device we muted, with the stable endpoint id the ledger stores.
struct MutedDevice {
    device: IMMDevice,
    id: String,
}

/// A device as the ledger remembers it: the stable endpoint id and whether it was
/// muted before we touched it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LedgerEntry {
    id: String,
    was_muted: bool,
}

/// The crash-recovery ledger: the devices we changed, so a recovery puts back only
/// those. The daemon's marker only says "a run ended while idle"; this file says
/// what has to be unmuted.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    devices: Vec<LedgerEntry>,
}

/// What to do with one enumerated output device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutePlan {
    /// Already muted: nothing to change and nothing to restore.
    Leave,
    /// Mute it: it has to be unmuted again afterwards.
    Mute,
    /// The previous state could not be read: the device is left alone.
    Unknown,
}

#[derive(Default)]
pub struct Overlay {
    windows: Vec<HWND>,
    /// The devices we muted ourselves: exactly what has to be unmuted again.
    muted: Vec<MutedDevice>,
    active: bool,
}

impl Overlay {
    pub fn is_active(&self) -> bool {
        self.active
    }
    pub fn monitor_count(&self) -> usize {
        self.windows.len()
    }
    /// How many devices we actually muted (the ones that were already muted do not
    /// count: we did not change them).
    pub fn muted_devices(&self) -> usize {
        self.muted.len()
    }
}

/// Register the overlay window class (once).
fn ensure_class() -> bool {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REGISTERED.get_or_init(|| unsafe {
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            // No background brush: Windows keeps that handle for the whole life of
            // the class, so the black is painted with the stock black brush instead
            // (see WM_ERASEBKGND) — no GDI object of ours to leak.
            lpszClassName: w!("ArmouryIdleOverlay"),
            ..Default::default()
        };
        RegisterClassW(&class) != 0
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            // The black background and the hint text are drawn here.
            // The update region MUST be validated (BeginPaint/EndPaint): returning without it
            // makes Windows re-send WM_PAINT forever — the daemon then spins at ~50% of a core
            // inside its message pump and stops draining pipe requests entirely (the "Wake up
            // button does nothing / screens stay black" bug of 2026-10-17).
            let mut ps = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            unsafe {
                fill_black(hwnd, hdc);
                draw_hint(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // The class has no background brush, so the black is filled here.
            unsafe { fill_black(hwnd, HDC(wparam.0 as *mut core::ffi::c_void)) };
            LRESULT(1)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Fill the window black with the stock brush (a system object: never deleted).
unsafe fn fill_black(hwnd: HWND, hdc: HDC) {
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    unsafe {
        let _ = FillRect(hdc, &rect, HBRUSH(GetStockObject(BLACK_BRUSH).0));
    }
}

/// "IDLE — press anything to wake up", centred on every covered screen, in muted greys.
unsafe fn draw_hint(hwnd: HWND, hdc: HDC) {
    use windows::Win32::Foundation::{COLORREF, RECT};
    use windows::Win32::Graphics::Gdi::{
        BACKGROUND_MODE, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DrawTextW, HGDIOBJ,
        SelectObject, SetBkMode, SetTextColor,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    let (big, small) = hint_fonts();
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    // "IDLE" sits a little above the middle, the hint just under it — both centred horizontally.
    let band = |centre_y: i32, half: i32| -> RECT {
        RECT {
            left: 0,
            right: width,
            top: centre_y - half,
            bottom: centre_y + half,
        }
    };

    unsafe {
        let _ = SetBkMode(hdc, BACKGROUND_MODE(1)); // TRANSPARENT: the black brush stays visible
        let old = SelectObject(hdc, HGDIOBJ(big.0));

        let mut title: Vec<u16> = "IDLE".encode_utf16().collect();
        let mut r = band(height / 2 - 70, 80);
        let _ = SetTextColor(hdc, COLORREF(0x00969696)); // RGB(150,150,150)
        let _ = DrawTextW(
            hdc,
            &mut title,
            &mut r,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );

        SelectObject(hdc, HGDIOBJ(small.0));
        let mut line: Vec<u16> = "press any key to wake up"
            .encode_utf16()
            .collect();
        let mut r2 = band(height / 2 + 40, 45);
        let _ = SetTextColor(hdc, COLORREF(0x00787878)); // RGB(120,120,120)
        let _ = DrawTextW(
            hdc,
            &mut line,
            &mut r2,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
    }
}

/// Two Segoe UI fonts, created once (falling back to the stock GUI font).
/// They are kept for the whole process on purpose: the windows come and go, and a
/// deleted font would leave them painting with a dead handle.
fn hint_fonts() -> (
    windows::Win32::Graphics::Gdi::HFONT,
    windows::Win32::Graphics::Gdi::HFONT,
) {
    use std::sync::OnceLock;
    use windows::Win32::Graphics::Gdi::{
        CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, CreateFontW, DEFAULT_CHARSET, DEFAULT_GUI_FONT,
        DEFAULT_PITCH, FF_DONTCARE, GetStockObject, HFONT, OUT_TT_PRECIS,
    };
    use windows::core::PCWSTR;

    static FONTS: OnceLock<(usize, usize)> = OnceLock::new();
    let (big, small) = *FONTS.get_or_init(|| unsafe {
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        let make = |height: i32, weight: i32| {
            CreateFontW(
                height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                PCWSTR(face.as_ptr()),
            )
        };
        let big = make(-64, 600);
        let small = make(-28, 400);
        let stock = || HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
        let big = if big.0.is_null() { stock() } else { big };
        let small = if small.0.is_null() { stock() } else { small };
        (big.0 as usize, small.0 as usize)
    });
    (
        HFONT(big as *mut core::ffi::c_void),
        HFONT(small as *mut core::ffi::c_void),
    )
}

/// Cover every monitor in black and mute every output device.
pub fn show(overlay: &mut Overlay) {
    if overlay.active {
        return;
    }
    if !ensure_class() {
        error!("idle overlay: the window class could not be registered - idle mode stays off");
        return;
    }

    for (x, y, width, height) in monitors() {
        unsafe {
            match CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("ArmouryIdleOverlay"),
                w!("ArmouryDaemonIdle"),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                width,
                height,
                None,
                None,
                None,
                None,
            ) {
                Ok(hwnd) => {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    // Only the Z-order changes: SWP_NOMOVE|SWP_NOSIZE are essential here, or
                    // this call resizes the window to 0x0 (that is exactly what happened for
                    // months: the overlay "covered" monitors but was invisible).
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    overlay.windows.push(hwnd);
                    info!("idle overlay window at {x},{y} {width}x{height}");
                }
                Err(e) => error!("idle overlay: could not create a monitor window: {e}"),
            }
        }
    }

    if overlay.windows.is_empty() {
        // Not one black screen: idle mode is not up. The audio is left alone as well,
        // the daemon takes the marker back and stays out of idle mode.
        error!(
            "idle overlay: not one monitor window could be created - idle mode stays off and \
             the audio was not touched"
        );
        return;
    }

    let outcome = match mute_all() {
        Ok(outcome) => {
            if outcome.enumerated == 0 {
                error!("idle overlay: no active output device was found - idle mode does not silence the machine");
            }
            outcome
        }
        Err(e) => {
            error!("idle overlay: {e} - the output devices were not muted");
            MuteOutcome {
                muted: Vec::new(),
                enumerated: 0,
                changed: 0,
            }
        }
    };

    // The ledger is the authoritative record of what we changed: the daemon's marker
    // only says "a run ended while idle".
    let entries: Vec<LedgerEntry> = outcome
        .muted
        .iter()
        .map(|muted| LedgerEntry {
            id: muted.id.clone(),
            was_muted: false,
        })
        .collect();
    if let Err(e) = write_ledger(&ledger_path(), &entries) {
        error!("idle overlay: the mute ledger could not be written ({e}) - a crash would not be recoverable");
    }

    overlay.muted = outcome.muted;
    overlay.active = true;
    info!(
        "idle overlay on: {} monitor(s) covered, {} output device(s) muted ({} enumerated)",
        overlay.windows.len(),
        outcome.changed,
        outcome.enumerated
    );
}

/// Tear down the overlay and restore the previous mute states.
pub fn hide(overlay: &mut Overlay) {
    if !overlay.active {
        return;
    }
    for hwnd in overlay.windows.drain(..) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
    let mut failed: Vec<LedgerEntry> = Vec::new();
    for muted in overlay.muted.drain(..) {
        if let Err(e) = set_device_mute(&muted.device, false) {
            warn!("idle: {} could not be unmuted: {e}", muted.id);
            failed.push(LedgerEntry {
                id: muted.id,
                was_muted: false,
            });
        }
    }
    overlay.active = false;
    let ledger_file = ledger_path();
    if failed.is_empty() {
        let _ = fs::remove_file(&ledger_file);
        info!("idle overlay off: screens restored, audio restored");
    } else {
        // What is still muted stays in the ledger: the next start retries it.
        if let Err(e) = write_ledger(&ledger_file, &failed) {
            error!("idle: the leftover ledger could not be written: {e}");
        }
        error!(
            "idle overlay off: screens restored, {} device(s) could not be unmuted - the ledger \
             was kept for the next start",
            failed.len()
        );
    }
}

/// Pump our overlay's window messages (called from the daemon loop while active).
pub fn pump() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        // Bounded on purpose: a misbehaving window must never keep this loop from returning
        // (see the WM_PAINT note in `wndproc`).
        for _ in 0..256 {
            if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                break;
            }
        }
    }
}

/// (x, y, width, height) of every monitor in virtual-screen coordinates.
fn monitors() -> Vec<(i32, i32, i32, i32)> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        unsafe {
            let out = &mut *(lparam.0 as *mut Vec<(i32, i32, i32, i32)>);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let r = info.rcMonitor;
                out.push((r.left, r.top, r.right - r.left, r.bottom - r.top));
            }
        }
        BOOL(1)
    }

    let mut out: Vec<(i32, i32, i32, i32)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut out as *mut _ as isize));
    }
    if out.is_empty() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
        };
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        out.push((0, 0, w, h));
    }
    out
}

fn devices() -> Option<Vec<IMMDevice>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).ok()?;
        let count = collection.GetCount().ok()?;
        let mut out = Vec::new();
        for index in 0..count {
            if let Ok(device) = collection.Item(index) {
                out.push(device);
            }
        }
        Some(out)
    }
}

fn set_device_mute(device: &IMMDevice, mute: bool) -> windows::core::Result<()> {
    unsafe {
        let volume = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)?;
        volume.SetMute(mute, std::ptr::null())
    }
}

/// The stable endpoint id of a device (the one Windows uses in the registry).
fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const core::ffi::c_void));
        text
    }
}

/// The plan for a device whose previous mute state is `was`: `Some(true)` is a
/// device that was already muted, `Some(false)` one we mute, `None` one whose state
/// we could not read (and which we then leave alone).
fn plan_for(was: Option<bool>) -> MutePlan {
    match was {
        Some(true) => MutePlan::Leave,
        Some(false) => MutePlan::Mute,
        None => MutePlan::Unknown,
    }
}

/// How many of these devices we really mute (the others were already muted or
/// could not be read at all).
fn changed_count(plans: &[MutePlan]) -> usize {
    plans.iter().filter(|plan| **plan == MutePlan::Mute).count()
}

/// What the mute pass did.
struct MuteOutcome {
    /// The devices we muted: exactly what has to be put back.
    muted: Vec<MutedDevice>,
    /// How many devices were enumerated.
    enumerated: usize,
    /// How many of them we really changed.
    changed: usize,
}

/// Mute every active output device. Only the devices we really change are returned:
/// those are the ones a restore has to put back.
fn mute_all() -> Result<MuteOutcome, String> {
    let devices = devices().ok_or_else(|| "the output devices could not be enumerated".to_string())?;
    let mut plans: Vec<MutePlan> = Vec::with_capacity(devices.len());
    let mut muted: Vec<MutedDevice> = Vec::new();
    for (index, device) in devices.iter().enumerate() {
        let id = device_id(device).unwrap_or_else(|| format!("output device #{index}"));
        let Some(volume) =
            (unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }).ok()
        else {
            warn!("idle: the volume of {id} could not be opened - the device was left alone");
            plans.push(MutePlan::Unknown);
            continue;
        };
        let was = match unsafe { volume.GetMute() } {
            Ok(was) => Some(was.as_bool()),
            Err(e) => {
                warn!("idle: the mute state of {id} could not be read ({e}) - the device was left alone");
                None
            }
        };
        let mut plan = plan_for(was);
        if plan == MutePlan::Mute {
            match unsafe { volume.SetMute(true, std::ptr::null()) } {
                Ok(()) => muted.push(MutedDevice {
                    device: device.clone(),
                    id,
                }),
                Err(e) => {
                    warn!("idle: SetMute failed for {id}: {e}");
                    plan = MutePlan::Leave; // we did not change it after all
                }
            }
        }
        plans.push(plan);
    }
    Ok(MuteOutcome {
        changed: changed_count(&plans),
        muted,
        enumerated: devices.len(),
    })
}

/// Unmute the output device with this endpoint id.
fn restore_device(entry: &LedgerEntry) -> Result<(), String> {
    let wide: Vec<u16> = entry.id.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator
            .GetDevice(PCWSTR(wide.as_ptr()))
            .map_err(|e| e.to_string())?;
        set_device_mute(&device, false).map_err(|e| e.to_string())
    }
}

/// Marker file that exists while idle mode is active, so a crash can be recovered.
pub fn marker_path() -> std::path::PathBuf {
    armoury_core::config::config_dir().join("idle.marker")
}

/// The ledger of the devices we muted: the authoritative record of what a crashed
/// run has to put back. It sits next to the marker and outlives it.
pub fn ledger_path() -> PathBuf {
    armoury_core::config::config_dir().join("idle.ledger")
}

/// Write the ledger (atomically: a crash mid-write cannot leave a corrupt file).
fn write_ledger(path: &Path, entries: &[LedgerEntry]) -> std::io::Result<()> {
    let ledger = Ledger {
        schema: LEDGER_SCHEMA,
        devices: entries.to_vec(),
    };
    let body = serde_json::to_vec_pretty(&ledger).map_err(std::io::Error::other)?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, &body)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_ledger(path: &Path) -> std::io::Result<Ledger> {
    let body = fs::read(path)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

/// Unmute the devices the ledger lists. `restore` fails when a device could not be
/// unmuted. Returns how many were restored and the entries that failed.
fn restore_entries<F>(entries: &[LedgerEntry], mut restore: F) -> (usize, Vec<LedgerEntry>)
where
    F: FnMut(&LedgerEntry) -> Result<(), String>,
{
    let mut restored = 0;
    let mut failed = Vec::new();
    for entry in entries {
        if entry.was_muted {
            // We never changed it: it stays as it is.
            continue;
        }
        match restore(entry) {
            Ok(()) => restored += 1,
            Err(e) => {
                warn!("idle recovery: {} could not be unmuted: {e}", entry.id);
                failed.push(entry.clone());
            }
        }
    }
    (restored, failed)
}

/// Restore the devices of the ledger and clean up the files. The ledger and the
/// marker are only removed when every device could be unmuted; what is left stays
/// in the ledger, so the next start can retry it.
fn recover_from_ledger<F>(
    ledger_file: &Path,
    marker: &Path,
    entries: &[LedgerEntry],
    restore: F,
) -> (usize, usize)
where
    F: FnMut(&LedgerEntry) -> Result<(), String>,
{
    let (restored, failed) = restore_entries(entries, restore);
    if failed.is_empty() {
        let _ = fs::remove_file(ledger_file);
        let _ = fs::remove_file(marker);
    } else {
        if let Err(e) = write_ledger(ledger_file, &failed) {
            error!("idle recovery: the leftover ledger could not be written: {e}");
        }
        error!(
            "idle recovery: {restored} device(s) unmuted, {} still muted - the ledger was kept \
             for the next start",
            failed.len()
        );
    }
    (restored, failed.len())
}

/// Did a previous run die while idle? Then its mutes are still in effect.
pub fn recover_if_needed() {
    let marker = marker_path();
    let ledger_file = ledger_path();
    if !marker.exists() && !ledger_file.exists() {
        return;
    }
    let ledger = match read_ledger(&ledger_file) {
        Ok(ledger) => ledger,
        Err(e) => {
            // Without a ledger we do not know what the previous run muted, and
            // unmuting everything would unmute devices the user muted themselves.
            error!(
                "idle recovery: the mute ledger could not be read ({e}) - no device was unmuted; \
                 if a device is still muted it has to be unmuted by hand"
            );
            let _ = fs::remove_file(&ledger_file);
            let _ = fs::remove_file(&marker);
            return;
        }
    };
    if ledger.schema > LEDGER_SCHEMA {
        // A newer build wrote this: leave the files alone, it knows what to do with them.
        error!(
            "idle recovery: the mute ledger uses schema {} and this build understands {LEDGER_SCHEMA} - no device was unmuted, the files were kept",
            ledger.schema
        );
        return;
    }
    let (restored, failed) =
        recover_from_ledger(&ledger_file, &marker, &ledger.devices, restore_device);
    if failed == 0 {
        warn!(
            "the previous run ended while idle - {restored} output device(s) unmuted ({} were \
             listed)",
            ledger.devices.len()
        );
    }
}

/// Mute state of the default output device (used by the drill).
#[allow(dead_code)]
pub fn default_device_muted() -> Option<bool> {    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        let volume = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()?;
        volume.GetMute().ok().map(|b| b.as_bool())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("armoury-overlay-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(id: &str, was_muted: bool) -> LedgerEntry {
        LedgerEntry {
            id: id.to_string(),
            was_muted,
        }
    }

    #[test]
    fn the_ledger_round_trips() {
        let dir = temp_dir("ledger");
        let file = dir.join("idle.ledger");
        let entries = vec![entry("{0.0.0.00000000}.{aaa}", false), entry("{0.0.0.00000000}.{bbb}", true)];
        write_ledger(&file, &entries).unwrap();
        let back = read_ledger(&file).unwrap();
        assert_eq!(back.schema, 1);
        assert_eq!(back.devices, entries);
    }

    #[test]
    fn a_corrupt_ledger_is_reported_not_panicked_on() {
        let dir = temp_dir("corrupt");
        let file = dir.join("idle.ledger");
        fs::write(&file, b"{not json").unwrap();
        assert!(read_ledger(&file).is_err());
    }

    #[test]
    fn only_the_devices_we_changed_are_restored() {
        let entries = vec![entry("a", false), entry("b", true), entry("c", false)];
        let mut asked: Vec<String> = Vec::new();
        let (restored, failed) = restore_entries(&entries, |entry| {
            asked.push(entry.id.clone());
            Ok(())
        });
        assert_eq!(restored, 2);
        assert!(failed.is_empty());
        assert_eq!(asked, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn a_failed_restore_keeps_the_ledger() {
        let dir = temp_dir("keep");
        let ledger_file = dir.join("idle.ledger");
        let marker = dir.join("idle.marker");
        fs::write(&marker, b"idle").unwrap();
        let entries = vec![entry("a", false), entry("b", false)];
        write_ledger(&ledger_file, &entries).unwrap();
        let (restored, failed) = recover_from_ledger(&ledger_file, &marker, &entries, |entry| {
            if entry.id == "b" {
                Err("the audio service is not up yet".to_string())
            } else {
                Ok(())
            }
        });
        assert_eq!(restored, 1);
        assert_eq!(failed, 1);
        // the ledger and the marker stay: the next start retries the failed device
        assert!(ledger_file.exists());
        assert!(marker.exists());
        let left = read_ledger(&ledger_file).unwrap();
        assert_eq!(left.devices.len(), 1);
        assert_eq!(left.devices[0].id, "b");
    }

    #[test]
    fn a_complete_restore_drops_the_ledger_and_the_marker() {
        let dir = temp_dir("done");
        let ledger_file = dir.join("idle.ledger");
        let marker = dir.join("idle.marker");
        fs::write(&marker, b"idle").unwrap();
        let entries = vec![entry("a", false)];
        write_ledger(&ledger_file, &entries).unwrap();
        let (restored, failed) = recover_from_ledger(&ledger_file, &marker, &entries, |_| Ok(()));
        assert_eq!(restored, 1);
        assert_eq!(failed, 0);
        assert!(!ledger_file.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn the_changed_count_is_not_the_enumerated_count() {
        let plans = vec![
            MutePlan::Leave,
            MutePlan::Mute,
            MutePlan::Unknown,
            MutePlan::Mute,
        ];
        assert_eq!(changed_count(&plans), 2);
        assert_eq!(plans.len(), 4);
    }

    #[test]
    fn a_device_with_an_unknown_state_is_never_muted() {
        assert_eq!(plan_for(None), MutePlan::Unknown);
        assert_eq!(plan_for(Some(true)), MutePlan::Leave);
        assert_eq!(plan_for(Some(false)), MutePlan::Mute);
    }
}
