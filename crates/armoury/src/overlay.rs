//! Idle overlay: black windows over every active monitor plus a mute of every audio
//! output device, with a ledger so exiting puts back exactly what we changed.

use log::{info, warn};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, EnumDisplayMonitors, GetMonitorInfoW, HBRUSH, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    HWND_TOPMOST, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW, SW_SHOW,
    SWP_NOACTIVATE, SetWindowPos, ShowWindow, TranslateMessage, WM_DESTROY, WM_PAINT, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{BOOL, PCWSTR, w};

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
        WM_PAINT => LRESULT(0), // the class brush paints it black
        WM_DESTROY => {
            let _ = hwnd;
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
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
                    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE);
                    overlay.windows.push(hwnd);
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
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
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
pub fn default_device_muted() -> Option<bool> {    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        let volume = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()?;
        volume.GetMute().ok().map(|b| b.as_bool())
    }
}
