//! The daemon's hotkeys: a low-level keyboard hook, not `RegisterHotKey`.
//!
//! `RegisterHotKey` (what the `global-hotkey` crate wraps) *consumes* the key system-wide: it
//! never reaches the foreground application. That was harmless for Scroll Lock, which nobody
//! uses, but End and Home are used in every text field, every list, every IDE and every game —
//! they have to keep working. A `WH_KEYBOARD_LL` hook runs *before* the key is posted to the
//! application's queue, so the daemon sees the key and the application still gets it.
//!
//! Since 2026-10-27 the combinations are the user's: each action carries a [`Binding`], parsed
//! from the text the settings window stores (`ctrl+alt+k`, `home`). A binding matches only when
//! the modifier set is **exact**, so a bare `home` still types in a text field that sees
//! `shift+home`.

use crossbeam_channel::{Receiver, Sender};
use log::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_UP,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
};

/// End: the historical killswitch key (still the default).
pub const VK_END: u32 = 0x23;
/// Home: the historical idle key (still the default).
pub const VK_HOME: u32 = 0x24;

/// What a hotkey press means for the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hide and mute every running tracked app.
    Killswitch,
    /// Enter or leave idle mode.
    Idle,
    /// An installed mode pack: the combination comes from the pack's own trigger, and the
    /// press toggles it. The pack owns what the press does — the daemon only carries it out.
    Pack(String),
}

impl Action {
    pub fn label(&self) -> String {
        match self {
            Action::Killswitch => "killswitch".to_string(),
            Action::Idle => "idle mode".to_string(),
            Action::Pack(id) => format!("pack {id}"),
        }
    }
}

/// A combination: any subset of ctrl/alt/shift/win plus exactly one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// The virtual-key code of the key itself.
    pub vk: u32,
}

impl Binding {
    /// Parse the text form: `ctrl+alt+k`, `shift+f9`, `home`. Case and spaces are forgiven,
    /// and `control`/`meta`/`windows` are accepted for the four modifiers. A combination is
    /// one to four keys: four modifiers and a key would be five, and is refused.
    pub fn parse(text: &str) -> Option<Binding> {
        let mut binding = Binding {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            vk: 0,
        };
        let parts: Vec<String> = text
            .split('+')
            .map(|part| part.trim().to_ascii_lowercase())
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() || parts.len() > 4 {
            return None;
        }
        let mut parts = parts.iter().map(String::as_str).peekable();
        let mut key: Option<u32> = None;
        while let Some(part) = parts.next() {
            let is_last = parts.peek().is_none();
            if is_last {
                key = vk_of(part);
                if key.is_none() {
                    return None;
                }
                break;
            }
            match part {
                "ctrl" | "control" => binding.ctrl = true,
                "alt" => binding.alt = true,
                "shift" => binding.shift = true,
                "win" | "windows" | "meta" | "super" => binding.win = true,
                // a key name in the middle of a combination is a typo, never a modifier
                _ => return None,
            }
        }
        binding.vk = key?;
        Some(binding)
    }

    /// The text form the settings window shows: `Ctrl+Alt+K`, `Home`.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        parts.push(key_label(self.vk));
        parts.join("+")
    }

    /// The text form stored in config: lowercase, the one `parse` reads back.
    pub fn text(&self) -> String {
        self.label().to_ascii_lowercase()
    }

    /// Does this combination describe the key that was just pressed? The modifiers are an
    /// **exact** match: `home` must not fire on `shift+home`, or the dashboard would toggle
    /// every time a text field selects to the start of a line.
    fn matches(&self, vk: u32, mods: (bool, bool, bool, bool)) -> bool {
        self.vk == vk
            && self.ctrl == mods.0
            && self.alt == mods.1
            && self.shift == mods.2
            && self.win == mods.3
    }

    /// The `RegisterHotKey` flags for this combination — only used by the conflict probe.
    fn register_flags(&self) -> u32 {
        let mut flags = MOD_NOREPEAT;
        if self.ctrl {
            flags |= MOD_CONTROL;
        }
        if self.alt {
            flags |= MOD_ALT;
        }
        if self.shift {
            flags |= MOD_SHIFT;
        }
        if self.win {
            flags |= MOD_WIN;
        }
        flags
    }
}

/// A key name to its virtual-key code. Covers what a person can type into a hotkey field:
/// letters, digits, F1-F24, and the navigation/editing keys, plus the numpad digits.
fn vk_of(name: &str) -> Option<u32> {
    if name.len() == 1 {
        let ch = name.chars().next()?;
        if ch.is_ascii_alphabetic() {
            return Some(ch.to_ascii_uppercase() as u32);
        }
        if ch.is_ascii_digit() {
            return Some(ch as u32);
        }
        return None;
    }
    let named = match name {
        "home" => VK_HOME,
        "end" => VK_END,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" | "pgdown" => 0x22,
        "pause" => 0x13,
        "scrolllock" | "scroll" => 0x91,
        "space" | "spacebar" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "escape" | "esc" => 0x1B,
        "backspace" | "back" => 0x08,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "capslock" | "caps" => 0x14,
        "printscreen" | "prtsc" => 0x2C,
        "numpad0" | "num0" => 0x60,
        "numpad1" | "num1" => 0x61,
        "numpad2" | "num2" => 0x62,
        "numpad3" | "num3" => 0x63,
        "numpad4" | "num4" => 0x64,
        "numpad5" | "num5" => 0x65,
        "numpad6" | "num6" => 0x66,
        "numpad7" | "num7" => 0x67,
        "numpad8" | "num8" => 0x68,
        "numpad9" | "num9" => 0x69,
        "multiply" => 0x6A,
        "add" | "plus" => 0x6B,
        "subtract" | "minus" => 0x6D,
        "decimal" => 0x6E,
        "divide" => 0x6F,
        _ => {
            // f1..f24
            if let Some(rest) = name.strip_prefix('f')
                && let Ok(n) = rest.parse::<u32>()
                && (1..=24).contains(&n)
            {
                return Some(0x70 + (n - 1));
            }
            return None;
        }
    };
    Some(named)
}

/// The display name of a virtual-key code (the inverse of `vk_of`, for the common keys).
fn key_label(vk: u32) -> String {
    if (0x41..=0x5A).contains(&vk) {
        return ((b'A' + (vk - 0x41) as u8) as char).to_string();
    }
    if (0x30..=0x39).contains(&vk) {
        return ((b'0' + (vk - 0x30) as u8) as char).to_string();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    // The numpad and its operator keys: the parser knows these names, so a recorded or
    // saved combination has to come back as the same key, not as `0x65`.
    if (0x60..=0x69).contains(&vk) {
        return format!("Numpad{}", vk - 0x60);
    }
    let named = match vk {
        VK_HOME => "Home",
        VK_END => "End",
        0x2D => "Insert",
        0x2E => "Delete",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x13 => "Pause",
        0x91 => "ScrollLock",
        0x20 => "Space",
        0x09 => "Tab",
        0x0D => "Enter",
        0x1B => "Esc",
        0x08 => "Backspace",
        0x26 => "Up",
        0x28 => "Down",
        0x25 => "Left",
        0x27 => "Right",
        0x14 => "CapsLock",
        0x2C => "PrintScreen",
        0x6A => "Multiply",
        0x6B => "Add",
        0x6D => "Subtract",
        0x6E => "Decimal",
        0x6F => "Divide",
        _ => return format!("0x{vk:02X}"),
    };
    named.to_string()
}

/// The combinations in force. The hook procedure is a plain function pointer, so the table has
/// to live in a global; it is small and written only on a settings change.
static BINDINGS: Mutex<Vec<(Binding, Action)>> = Mutex::new(Vec::new());

/// The channel the hook posts the actions to.
static SENDER: Mutex<Option<Sender<Action>>> = Mutex::new(None);

/// True while the settings window is recording a combination. The hook still tracks the
/// keys, but posts nothing: pressing End to record it must not also toggle the killswitch.
static SUSPENDED: AtomicBool = AtomicBool::new(false);

/// Stop or resume posting actions (the settings window is recording a combination). The
/// window sets this every frame from its own state, so a window that closes mid-record
/// cannot leave the hotkeys dead.
pub fn suspend(on: bool) {
    SUSPENDED.store(on, Ordering::Relaxed);
}

/// Keys that are currently down (one bit per key code, indexed by `vk >> 5`). A held key
/// repeats, and only the first press may toggle anything.
static DOWN: [AtomicU32; 8] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// Did a **real** key go down since the last call? `GetAsyncKeyState` cannot tell a
/// keystroke from a `SendInput` one, and the daemon injects keys itself (the G-Helper pack),
/// so an apply used to dismiss idle mode the moment a tracked app started.
static REAL_KEY: AtomicBool = AtomicBool::new(false);

pub fn take_real_key() -> bool {
    REAL_KEY.swap(false, Ordering::Relaxed)
}

fn down_word(vk: u32) -> usize {
    ((vk >> 5) & 7) as usize
}

fn down_bit(vk: u32) -> u32 {
    1 << (vk & 31)
}

/// Was this key event produced by an injector? The daemon injects keys itself (the G-Helper
/// hotkeys), and those must not trigger anything.
fn is_injected(flags: u32) -> bool {
    flags & LLKHF_INJECTED != 0
}

/// The modifier state at the moment of the event, as (ctrl, alt, shift, win).
fn modifiers_now() -> (bool, bool, bool, bool) {
    let down = |vk: u32| unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 };
    (
        down(0x11) || down(0xA2) || down(0xA3), // control, left/right control
        down(0x12) || down(0xA4) || down(0xA5), // menu(alt), left/right alt
        down(0x10) || down(0xA0) || down(0xA1), // shift, left/right shift
        down(0x5B) || down(0x5C),               // left/right windows
    )
}

/// The action a key press means, if any.
fn action_for_press(vk: u32, mods: (bool, bool, bool, bool)) -> Option<Action> {
    let table = BINDINGS.lock().unwrap_or_else(|e| e.into_inner());
    table
        .iter()
        .find(|(binding, _)| binding.matches(vk, mods))
        .map(|(_, action)| action.clone())
}

/// Post the action to the daemon's event loop. The channel is unbounded, so the hook never
/// waits.
fn post(action: Action) {
    let guard = SENDER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(action);
    }
}

/// The hook procedure. It has to return fast: Windows silently removes a low-level hook whose
/// callback takes longer than `LowLevelHooksTimeout` (1 s at most).
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION the system hands over a valid KBDLLHOOKSTRUCT.
        let kbd = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if !is_injected(kbd.flags) {
            let word = down_word(kbd.vkCode);
            let bit = down_bit(kbd.vkCode);
            if kbd.flags & LLKHF_UP != 0 {
                DOWN[word].fetch_and(!bit, Ordering::Relaxed);
            } else if DOWN[word].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                // A real key, not one of ours: this is what may wake an idle session. The
                // pack hotkeys are injected, so they must never count as the user arriving.
                REAL_KEY.store(true, Ordering::Relaxed);
                // the first press of this key: the auto-repeat must not toggle twice
                if !SUSPENDED.load(Ordering::Relaxed)
                    && let Some(action) = action_for_press(kbd.vkCode, modifiers_now())
                {
                    post(action);
                }
            }
        }
    }
    // The key goes on to the foreground application: the combination keeps working everywhere.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// The installed hook, so it can be taken down again when the daemon exits.
pub struct Hook {
    handle: HHOOK,
}

/// Install the low-level keyboard hook with these combinations.
///
/// It has to be installed on the thread that pumps the messages: Windows calls the procedure in
/// that thread's context. Returns the hook (to uninstall it) and the receiver the actions
/// arrive on.
pub fn install(bindings: Vec<(Binding, Action)>) -> Option<(Hook, Receiver<Action>)> {
    set_bindings(bindings);
    let (tx, rx) = crossbeam_channel::unbounded::<Action>();
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
    info!("keyboard hook installed ({})", describe());
    Some((Hook { handle }, rx))
}

/// Replace the combination table (the settings window changed a hotkey). The hook reads it on
/// the next press, so nothing has to be reinstalled.
pub fn set_bindings(bindings: Vec<(Binding, Action)>) {
    *BINDINGS.lock().unwrap_or_else(|e| e.into_inner()) = bindings;
}

/// The combinations from the config. One that does not parse falls back to the default for its
/// action (and says so in the log) instead of leaving the action dead with no way to reach it
/// from the settings window that wrote the bad value.
pub fn bindings_from(hotkeys: &armoury_core::config::Hotkeys) -> Vec<(Binding, Action)> {
    let mut out: Vec<(Binding, Action)> = Vec::with_capacity(2);
    let mut add = |text: &str, fallback: &str, action: Action| {
        let binding = Binding::parse(text).or_else(|| {
            warn!(
                "hotkey '{text}' for {} does not parse — using {fallback}",
                action.label()
            );
            Binding::parse(fallback)
        });
        if let Some(binding) = binding {
            out.push((binding, action));
        }
    };
    add(&hotkeys.idle, "home", Action::Idle);
    add(&hotkeys.killswitch, "end", Action::Killswitch);
    out
}

/// A one-line description of what is bound, for the log.
fn describe() -> String {
    let table = BINDINGS.lock().unwrap_or_else(|e| e.into_inner());
    let mut parts: Vec<String> = table
        .iter()
        .map(|(binding, action)| format!("{} = {}", binding.label(), action.label()))
        .collect();
    parts.sort();
    parts.join(", ")
}

/// Is this combination already taken by something that registers hotkeys (Windows itself, or
/// another app)? The daemon uses a hook, so a taken combination still *reaches* it — the
/// problem is that the owner acts too. `Some(reason)` means "warn the user".
///
/// The probe registers the combination for a moment and gives it straight back: success means
/// nobody owns it.
pub fn conflict(text: &str) -> Option<String> {
    let binding = Binding::parse(text)?;
    if binding.vk == 0 {
        return None;
    }
    let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), 0x7FF0, binding.register_flags(), binding.vk) };
    if ok == 0 {
        return Some(format!(
            "{} is already owned by another app or by Windows",
            binding.label()
        ));
    }
    unsafe { UnregisterHotKey(std::ptr::null_mut(), 0x7FF0) };
    None
}

impl Hook {
    /// Take the hook down again (the daemon is exiting).
    pub fn uninstall(self) {
        // SAFETY: the handle came from SetWindowsHookExW and is uninstalled exactly once.
        unsafe { UnhookWindowsHookEx(self.handle) };
        *SENDER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // A key that was down when the daemon left must not block the next press.
        for word in DOWN.iter() {
            word.store(0, Ordering::Relaxed);
        }
        info!("keyboard hook removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_home_and_end() {
        let home = Binding::parse("home").expect("home parses");
        let end = Binding::parse("end").expect("end parses");
        assert_eq!(home.vk, VK_HOME);
        assert_eq!(end.vk, VK_END);
        assert_eq!(home.label(), "Home");
        assert_eq!(Binding::parse("HOME").unwrap().vk, VK_HOME);
    }

    #[test]
    fn a_combination_parses_the_way_it_is_written() {
        let binding = Binding::parse("ctrl+alt+k").expect("ctrl+alt+k parses");
        assert!(binding.ctrl && binding.alt && !binding.shift && !binding.win);
        assert_eq!(binding.vk, 'K' as u32);
        assert_eq!(binding.label(), "Ctrl+Alt+K");
        assert_eq!(binding.text(), "ctrl+alt+k");
        // and it survives a round trip
        assert_eq!(Binding::parse(&binding.text()), Some(binding));
        assert_eq!(Binding::parse(" shift + f9 ").unwrap().label(), "Shift+F9");
        assert_eq!(Binding::parse("win+space").unwrap().vk, 0x20);
        assert_eq!(Binding::parse("control+windows+k").unwrap().label(), "Ctrl+Win+K");
    }

    #[test]
    fn nonsense_is_refused_instead_of_guessed() {
        assert!(Binding::parse("").is_none());
        assert!(Binding::parse("ctrl+").is_none());
        assert!(Binding::parse("ctrl+alt").is_none(), "a modifier is not a key");
        assert!(Binding::parse("k+ctrl").is_none(), "the key goes last");
        assert!(Binding::parse("banana").is_none());
        assert!(Binding::parse("ctrl+banana").is_none());
        assert!(Binding::parse("f25").is_none());
    }

    #[test]
    fn a_combination_is_one_to_four_keys() {
        // up to four keys: three modifiers and a key is the longest chord
        assert!(Binding::parse("ctrl+alt+shift+k").is_some());
        assert!(
            Binding::parse("ctrl+alt+shift+win+k").is_none(),
            "five keys is one too many"
        );
        assert!(Binding::parse("home").is_some(), "one key is a combination too");
    }

    #[test]
    fn a_binding_needs_the_exact_modifier_set() {
        let home = Binding::parse("home").unwrap();
        assert!(home.matches(VK_HOME, (false, false, false, false)));
        assert!(
            !home.matches(VK_HOME, (false, false, true, false)),
            "shift+home is a text selection, not the idle toggle"
        );
        assert!(!home.matches(VK_HOME, (true, false, false, false)));
        assert!(!home.matches(VK_END, (false, false, false, false)));

        let combo = Binding::parse("ctrl+alt+k").unwrap();
        assert!(combo.matches('K' as u32, (true, true, false, false)));
        assert!(!combo.matches('K' as u32, (true, false, false, false)));
        assert!(!combo.matches('K' as u32, (true, true, true, false)));
    }

    #[test]
    fn the_table_is_what_decides() {
        set_bindings(vec![
            (Binding::parse("f9").unwrap(), Action::Idle),
            (Binding::parse("ctrl+f10").unwrap(), Action::Killswitch),
        ]);
        assert_eq!(action_for_press(0x78, (false, false, false, false)), Some(Action::Idle));
        assert_eq!(action_for_press(0x79, (true, false, false, false)), Some(Action::Killswitch));
        assert_eq!(action_for_press(0x79, (false, false, false, false)), None);
        set_bindings(Vec::new());
    }

    #[test]
    fn injected_keys_are_ignored() {
        assert!(is_injected(LLKHF_INJECTED));
        assert!(is_injected(LLKHF_INJECTED | LLKHF_UP));
        assert!(!is_injected(0));
    }

    #[test]
    fn every_key_label_round_trips_through_the_parser() {
        // A label the user sees must be a text the parser reads back, or a saved binding
        // would stop working the moment it is shown.
        for text in [
            "home", "end", "f1", "f12", "f24", "a", "z", "0", "9", "space", "tab", "enter",
            "esc", "backspace", "up", "down", "left", "right", "insert", "delete", "pageup",
            "pagedown", "pause", "scrolllock", "capslock", "printscreen", "numpad5", "divide",
        ] {
            let binding = Binding::parse(text).unwrap_or_else(|| panic!("{text} must parse"));
            let again = Binding::parse(&binding.text())
                .unwrap_or_else(|| panic!("{} must parse back ({text})", binding.text()));
            assert_eq!(again, binding, "{text} → {} → {:?}", binding.label(), again);
        }
    }

    #[test]
    fn a_broken_binding_falls_back_instead_of_killing_the_action() {
        let hotkeys = armoury_core::config::Hotkeys {
            idle: "ctrl+banana".to_string(),
            killswitch: "ctrl+alt+k".to_string(),
        };
        let bindings = bindings_from(&hotkeys);
        assert_eq!(bindings.len(), 2);
        let idle = bindings
            .iter()
            .find(|(_, action)| *action == Action::Idle)
            .map(|(binding, _)| *binding)
            .expect("idle is still bound");
        assert_eq!(idle.vk, VK_HOME, "the default takes over");
        let kill = bindings
            .iter()
            .find(|(_, action)| *action == Action::Killswitch)
            .map(|(binding, _)| *binding)
            .expect("the killswitch is bound");
        assert_eq!(kill.label(), "Ctrl+Alt+K");
    }

    #[test]
    fn the_hook_installs_and_comes_away_again() {
        let (hook, _rx) = install(vec![
            (Binding::parse("home").unwrap(), Action::Idle),
            (Binding::parse("end").unwrap(), Action::Killswitch),
        ])
        .expect("the hook installs");
        hook.uninstall();
    }
}
