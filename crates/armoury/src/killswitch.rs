//! Killswitch: hide + mute the running tracked apps, then put back exactly what changed.
//!
//! Everything is recorded in a ledger so the restore is exact and only touches what we
//! altered (windows we hid, audio sessions we muted and their previous mute state).

use log::{info, warn};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::core::{BOOL, Interface};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioSessionControl, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GW_OWNER, GetWindow, GetWindowLongPtrW, GetWindowThreadProcessId,
    IsWindowVisible, SW_HIDE, SW_SHOW, ShowWindow, WS_EX_TOOLWINDOW,
};

#[derive(Default)]
pub struct Ledger {
    /// (window, process) for every window we hid
    hidden: Vec<(isize, u32)>,
    /// (pid, was muted before us) for every session we muted
    muted: Vec<(u32, bool)>,
    active: bool,
}

impl Ledger {
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// How many apps the killswitch is suppressing right now: the distinct processes it has
    /// hidden a window for or muted a session for. A released killswitch holds nothing, so
    /// this is 0 — which is what the dashboard shows.
    pub fn affected_apps(&self) -> usize {
        let mut pids: Vec<u32> = self.hidden.iter().map(|(_, pid)| *pid).collect();
        pids.extend(self.muted.iter().map(|(pid, _)| *pid));
        pids.sort_unstable();
        pids.dedup();
        pids.len()
    }
}

/// COM for the audio interfaces, once per thread.
pub fn init_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Hide every visible top-level window of these processes and mute their audio sessions.
pub fn engage(pids: &[u32], ledger: &mut Ledger) -> (usize, usize) {
    let mut hidden_now = 0;
    let mut muted_now = 0;

    for pid in pids {
        for hwnd in windows_of(*pid) {
            if !ledger.hidden.iter().any(|(w, _)| *w == hwnd) {
                unsafe {
                    let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
                }
                ledger.hidden.push((hwnd, *pid));
                hidden_now += 1;
            }
        }
        if let Some(previous) = set_mute(*pid, true) {
            if !previous {
                // only remember sessions we actually changed from unmuted
                ledger.muted.push((*pid, previous));
                muted_now += 1;
            } else {
                ledger.muted.push((*pid, true));
            }
        }
    }

    ledger.active = true;
    info!(
        "killswitch engaged: {hidden_now} window(s) hidden, {muted_now} session(s) muted (ledger: {} windows, {} sessions)",
        ledger.hidden.len(),
        ledger.muted.len()
    );
    (ledger.hidden.len(), ledger.muted.len())
}

/// Put back exactly what we changed.
pub fn disengage(ledger: &mut Ledger) {
    for (hwnd, _) in ledger.hidden.drain(..) {
        unsafe {
            let _ = ShowWindow(HWND(hwnd as *mut _), SW_SHOW);
        }
    }
    for (pid, was_muted) in ledger.muted.drain(..) {
        // only unmute what we muted (leave sessions that were already muted alone)
        if !was_muted {
            set_mute(pid, false);
        }
    }
    ledger.active = false;
    info!("killswitch released: windows shown, audio restored");
}

/// Visible, unowned, non-tool top-level windows belonging to a process.
fn windows_of(pid: u32) -> Vec<isize> {
    struct Ctx {
        pid: u32,
        out: Vec<isize>,
    }
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            if IsWindowVisible(hwnd).as_bool() {
                let mut owner_pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
                if owner_pid == ctx.pid
                    && GetWindow(hwnd, GW_OWNER).is_err()
                    && (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & (WS_EX_TOOLWINDOW.0 as isize)) == 0
                {
                    ctx.out.push(hwnd.0 as isize);
                }
            }
        }
        BOOL(1)
    }

    let mut ctx = Ctx {
        pid,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

/// Every audio session on every active output device (an app can play to a device
/// that is not the default one).
fn sessions() -> Vec<IAudioSessionControl> {
    let mut out = Vec::new();
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return out;
        };
        let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        let device_count = collection.GetCount().unwrap_or(0);
        for index in 0..device_count {
            let Ok(device) = collection.Item(index) else {
                continue;
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(list) = manager.GetSessionEnumerator() else {
                continue;
            };
            let session_count = list.GetCount().unwrap_or(0);
            for session_index in 0..session_count {
                if let Ok(control) = list.GetSession(session_index) {
                    out.push(control);
                }
            }
        }
    }
    out
}

/// Mute/unmute every audio session of a process across **all** output devices.
/// Returns the previous mute state of the last session it touched.
fn set_mute(pid: u32, mute: bool) -> Option<bool> {
    let mut previous = None;
    for control in sessions() {
        unsafe {
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            if control2.GetProcessId().unwrap_or(0) != pid {
                continue;
            }
            let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            if let Ok(was) = volume.GetMute() {
                let was = was.as_bool();
                if was != mute {
                    if let Err(e) = volume.SetMute(mute, std::ptr::null()) {
                        warn!("SetMute failed for pid {pid}: {e}");
                    } else {
                        info!("killswitch: pid {pid} session {}", if mute { "muted" } else { "unmuted" });
                    }
                }
                previous = Some(previous.unwrap_or(was));
            }
        }
    }
    if previous.is_none() {
        info!("killswitch: no audio session for pid {pid} (nothing to mute)");
    }
    previous
}

/// Keep the compiler honest about the interface imports we rely on.
#[allow(dead_code)]
fn _assert_types(_: Option<IAudioSessionControl>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ledger_counts_the_apps_it_suppresses() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.affected_apps(), 0, "an unused ledger suppresses nothing");
        assert!(!ledger.is_active());

        // two windows of one app and one session of another: two apps
        ledger.hidden.push((10, 42));
        ledger.hidden.push((11, 42));
        ledger.muted.push((42, false));
        ledger.muted.push((43, false));
        assert_eq!(ledger.affected_apps(), 2);
        assert_eq!(ledger.hidden.len(), 2, "the windows are counted on their own");
        assert_eq!(ledger.muted.len(), 2, "and so are the muted sessions");
    }

    #[test]
    fn a_released_killswitch_suppresses_nothing() {
        let mut ledger = Ledger::default();
        ledger.hidden.push((10, 42));
        ledger.muted.push((42, false));
        ledger.active = true;
        assert!(ledger.is_active());

        // what `disengage` does: the windows are shown and the sessions unmuted again
        ledger.hidden.clear();
        ledger.muted.clear();
        ledger.active = false;
        assert_eq!(ledger.affected_apps(), 0);
        assert!(!ledger.is_active());
    }
}
