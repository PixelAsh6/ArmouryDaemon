//! Idle overlay: black windows over every active monitor plus a mute of every audio
//! output device, with a ledger so exiting puts back exactly what we changed.

use log::{info, warn};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
    MONITORINFO, PAINTSTRUCT,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    HWND_TOPMOST, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, TranslateMessage, WM_DESTROY,
    WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{BOOL, w};

#[derive(Default)]
pub struct Overlay {
    windows: Vec<HWND>,
    /// (device, was muted before us) for every endpoint we muted
    mutes: Vec<(IMMDevice, bool)>,
    active: bool,
}

impl Overlay {
    pub fn is_active(&self) -> bool {
        self.active
    }
    pub fn monitor_count(&self) -> usize {
        self.windows.len()
    }
    pub fn muted_devices(&self) -> usize {
        self.mutes.len()
    }
}

/// Register the overlay window class (once).
fn ensure_class() -> bool {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REGISTERED.get_or_init(|| unsafe {
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hbrBackground: CreateSolidBrush(COLORREF(0)),
            lpszClassName: w!("ArmouryIdleOverlay"),
            ..Default::default()
        };
        RegisterClassW(&class) != 0
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            // Black fill comes from the window class brush; the hint text is drawn here.
            // The update region MUST be validated (BeginPaint/EndPaint): returning without it
            // makes Windows re-send WM_PAINT forever — the daemon then spins at ~50% of a core
            // inside its message pump and stops draining pipe requests entirely (the "Wake up
            // button does nothing / screens stay black" bug of 2026-10-17).
            let mut ps = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            unsafe {
                draw_hint(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = hwnd;
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
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
    let band = |dy: i32| -> RECT {
        let top = (height / 2 + dy).clamp(0, height.saturating_sub(1));
        RECT {
            left: 0,
            right: width,
            top,
            bottom: (top + height / 4).min(height),
        }
    };

    unsafe {
        let _ = SetBkMode(hdc, BACKGROUND_MODE(1)); // TRANSPARENT: the black brush stays visible
        let old = SelectObject(hdc, HGDIOBJ(big.0));

        let mut title: Vec<u16> = "IDLE".encode_utf16().collect();
        let mut r = band(-90);
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
        let mut r2 = band(10);
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
        warn!("idle overlay: window class registration failed");
        return;
    }

    for (x, y, width, height) in monitors() {
        unsafe {
            match CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
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
                Err(e) => warn!("idle overlay: could not create a monitor window: {e}"),
            }
        }
    }

    overlay.mutes = mute_all(true).unwrap_or_default();
    overlay.active = true;
    info!(
        "idle overlay on: {} monitor(s) covered, {} output device(s) muted",
        overlay.windows.len(),
        overlay.mutes.len()
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
    for (device, was_muted) in overlay.mutes.drain(..) {
        if !was_muted {
            let _ = set_device_mute(&device, false);
        }
    }
    overlay.active = false;
    info!("idle overlay off: screens restored, audio restored");
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

/// Mute (or unmute) every active output device; returns what each one was before.
fn mute_all(mute: bool) -> Option<Vec<(IMMDevice, bool)>> {
    let mut states = Vec::new();
    for device in devices()? {
        let Ok(volume) = (unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) })
        else {
            continue;
        };
        let was = unsafe { volume.GetMute() }.map(|b| b.as_bool()).unwrap_or(false);
        if was != mute {
            let _ = set_device_mute(&device, mute);
        }
        states.push((device, was));
    }
    Some(states)
}

/// Unmute every active output device — used to recover if we died while idle.
pub fn unmute_all_devices() -> usize {
    match mute_all(false) {
        Some(states) => states.len(),
        None => 0,
    }
}

/// Marker file that exists while idle mode is active, so a crash can be recovered.
pub fn marker_path() -> std::path::PathBuf {
    armoury_core::config::config_dir().join("idle.marker")
}

/// Did a previous run die while idle? Then its mutes are still in effect.
pub fn recover_if_needed() {
    let marker = marker_path();
    if marker.exists() {
        let devices = unmute_all_devices();
        let _ = std::fs::remove_file(&marker);
        warn!("previous run ended while idle — restored mutes on {devices} output device(s)");
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
