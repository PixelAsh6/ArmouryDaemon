//! The daemon's hotkeys: a low-level keyboard hook, not `RegisterHotKey`.
//!
//! `RegisterHotKey` (what the `global-hotkey` crate wraps) *consumes* the key system-wide: it
//! never reaches the foreground application. That was harmless for Scroll Lock, which nobody
//! uses, but End and Home are used in every text field, every list, every IDE and every game —
//! they have to keep working. A `WH_KEYBOARD_LL` hook runs *before* the key is posted to the
//! application's queue, so the daemon sees the key and the application still gets it.

use crossbeam_channel::{Receiver, Sender};
use log::{info, warn};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_UP,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
};

/// End: toggle the killswitch.
pub const VK_END: u32 = 0x23;
/// Home: toggle idle mode.
pub const VK_HOME: u32 = 0x24;

/// What a hotkey press means for the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// End: hide and mute every running tracked app.
    Killswitch,
    /// Home: enter or leave idle mode.
    Idle,
}

/// The key -> action map. Both keys are momentary, so there is no toggle state to restore.
pub fn action_for(vk: u32) -> Option<Action> {
    match vk {
        VK_END => Some(Action::Killswitch),
        VK_HOME => Some(Action::Idle),
        _ => None,
    }
}

/// The channel the hook posts the keys to. The hook procedure is a plain function pointer, so
/// the sender has to live in a global.
static SENDER: Mutex<Option<Sender<u32>>> = Mutex::new(None);

/// The hotkey keys that are currently down (one bit per key). A held key repeats, and only the
/// first press may toggle anything.
static DOWN: AtomicU32 = AtomicU32::new(0);

/// The bit of a hotkey key in `DOWN` (0 = not a hotkey).
fn bit_of(vk: u32) -> u32 {
    match vk {
        VK_END => 1 << 0,
        VK_HOME => 1 << 1,
        _ => 0,
    }
}

/// Was this key event produced by an injector? The daemon injects keys itself (the G-Helper
/// hotkeys), and those must not trigger anything.
fn is_injected(flags: u32) -> bool {
    flags & LLKHF_INJECTED != 0
}

/// Post the key to the daemon's event loop. The channel is unbounded, so the hook never waits.
fn post(vk: u32) {
    let guard = SENDER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(vk);
    }
}

/// The hook procedure. It has to return fast: Windows silently removes a low-level hook whose
/// callback takes longer than `LowLevelHooksTimeout` (1 s at most).
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION the system hands over a valid KBDLLHOOKSTRUCT.
        let kbd = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if !is_injected(kbd.flags) {
            let bit = bit_of(kbd.vkCode);
            if bit != 0 {
                if kbd.flags & LLKHF_UP != 0 {
                    DOWN.fetch_and(!bit, Ordering::Relaxed);
                } else if DOWN.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                    // the first press of this key: the auto-repeat must not toggle twice
                    post(kbd.vkCode);
                }
            }
        }
    }
    // The key goes on to the foreground application: End and Home keep working everywhere.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// The installed hook, so it can be taken down again when the daemon exits.
pub struct Hook {
    handle: HHOOK,
}

/// Install the low-level keyboard hook. It has to be installed on the thread that pumps the
/// messages: Windows calls the procedure in that thread's context.
/// Returns the hook (to uninstall it) and the receiver the key presses arrive on.
pub fn install() -> Option<(Hook, Receiver<u32>)> {
    let (tx, rx) = crossbeam_channel::unbounded::<u32>();
    *SENDER.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    // SAFETY: a plain function pointer of this process, and a null module handle is what the
    // Win32 documentation asks for when the hook procedure lives in the calling process.
    let handle = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hook_proc),
            std::ptr::null_mut(),
            0, // every thread in the system
        )
    };
    if handle.is_null() {
        warn!(
            "keyboard hook failed: {} (the hotkeys stay dead)",
            std::io::Error::last_os_error()
        );
        *SENDER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return None;
    }
    info!("keyboard hook installed (End = killswitch, Home = idle mode)");
    Some((Hook { handle }, rx))
}

impl Hook {
    /// Take the hook down again (the daemon is exiting).
    pub fn uninstall(self) {
        // SAFETY: the handle came from SetWindowsHookExW and is uninstalled exactly once.
        unsafe { UnhookWindowsHookEx(self.handle) };
        *SENDER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // A key that was down when the daemon left must not block the next press.
        DOWN.store(0, Ordering::Relaxed);
        info!("keyboard hook removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_is_the_killswitch_and_home_is_the_idle_mode() {
        assert_eq!(action_for(VK_END), Some(Action::Killswitch));
        assert_eq!(action_for(VK_HOME), Some(Action::Idle));
        assert_eq!(action_for(0x41), None); // A
        assert_eq!(action_for(0x91), None); // Scroll Lock: no longer a hotkey
    }

    #[test]
    fn injected_keys_are_ignored() {
        assert!(is_injected(LLKHF_INJECTED));
        assert!(is_injected(LLKHF_INJECTED | LLKHF_UP));
        assert!(!is_injected(0));
        assert!(!is_injected(LLKHF_UP));
    }

    #[test]
    fn only_the_two_hotkeys_have_a_bit() {
        assert_ne!(bit_of(VK_END), 0);
        assert_ne!(bit_of(VK_HOME), 0);
        assert_ne!(bit_of(VK_END), bit_of(VK_HOME));
        assert_eq!(bit_of(0x41), 0);
    }

    #[test]
    fn the_hook_installs_and_comes_away_again() {
        let (hook, _rx) = install().expect("the hook installs");
        hook.uninstall();
    }
}
