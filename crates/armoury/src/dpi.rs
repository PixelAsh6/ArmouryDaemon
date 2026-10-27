//! Per-Monitor-V2 DPI awareness, set before any window, COM object or font exists.
//!
//! A DPI-unaware process gets virtualised screen metrics and 96-DPI GDI text, so the
//! daemon, the dashboard and the band all have to say the same thing early. The switch
//! happens in `main`, before the modes run; a mode reports the failure once it has a
//! logger (`error`). Failing is not fatal: the process may already be aware
//! (ERROR_ACCESS_DENIED), or the system may not know the context — the mode still runs
//! either way.

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

/// The Win32 error of the DPI switch (0 = it worked).
static DPI_ERROR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Switch the process to Per-Monitor-V2, before any window exists.
///
/// The error is kept instead of logged: at this point there is no logger, so the message
/// would be lost. `error()` lets the mode report it once `logging::init` has run.
pub fn set() {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            DPI_ERROR.store(GetLastError(), std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The error of the DPI switch, if it failed. Read it after the logger exists.
pub fn error() -> Option<u32> {
    match DPI_ERROR.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        code => Some(code),
    }
}
