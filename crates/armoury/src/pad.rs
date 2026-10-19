//! XInput polling: pad presence (daemon) and pad state (console launcher).
//!
//! `XInputGetState` on the four fixed slots — the standard windowless approach, no window or
//! message pump involved. Pads that only speak DInput are deliberately out of scope for now.

use std::time::{Duration, Instant};

use windows::Win32::UI::Input::XboxController::{
    XInputGetState, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_DPAD_DOWN,
    XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT, XINPUT_GAMEPAD_DPAD_UP,
    XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE,
};

pub const SLOTS: u32 = 4;

/// Left-stick deadzone for menu navigation. Firm on purpose: stick drift must never scroll.
const DEADZONE: i16 = 16_000;

/// The console launcher's view of one pad.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pad {
    pub a: bool,
    pub b: bool,
    /// X / Y face buttons, carried for the next round of pad actions (no ui use yet).
    pub x: bool,
    pub y: bool,
    /// -1 / 0 / +1: d-pad or left stick horizontal intent.
    pub move_x: i8,
    /// -1 / 0 / +1: d-pad or left stick vertical intent. Screen coordinates: up is -1.
    pub move_y: i8,
}

/// Read slot `slot` (0..SLOTS). `None` when no pad is connected there.
pub fn read(slot: u32) -> Option<Pad> {
    if slot >= SLOTS {
        return None;
    }
    let mut state = XINPUT_STATE::default();
    // XInputGetState returns 0 (ERROR_SUCCESS) while a pad sits in the slot.
    if unsafe { XInputGetState(slot, &mut state) } != 0 {
        return None;
    }
    let g = state.Gamepad;
    Some(Pad {
        a: g.wButtons.0 & XINPUT_GAMEPAD_A.0 != 0,
        b: g.wButtons.0 & XINPUT_GAMEPAD_B.0 != 0,
        x: g.wButtons.0 & XINPUT_GAMEPAD_X.0 != 0,
        y: g.wButtons.0 & XINPUT_GAMEPAD_Y.0 != 0,
        move_x: nav_x(
            g.wButtons.0 & XINPUT_GAMEPAD_DPAD_LEFT.0 != 0,
            g.wButtons.0 & XINPUT_GAMEPAD_DPAD_RIGHT.0 != 0,
            g.sThumbLX,
        ),
        // The same helper, fed up as the negative key and down as the positive one.
        move_y: nav_x(
            g.wButtons.0 & XINPUT_GAMEPAD_DPAD_UP.0 != 0,
            g.wButtons.0 & XINPUT_GAMEPAD_DPAD_DOWN.0 != 0,
            g.sThumbLY,
        ),
    })
}

/// First connected slot, for the launcher's per-frame read.
pub fn first_connected() -> Option<u32> {
    (0..SLOTS).find(|slot| read(*slot).is_some())
}

/// Which slots currently hold a pad, for the daemon's hotplug watch.
pub fn connected_slots() -> [bool; SLOTS as usize] {
    let mut out = [false; SLOTS as usize];
    for (slot, on) in out.iter_mut().enumerate() {
        *on = read(slot as u32).is_some();
    }
    out
}

fn nav_x(dpad_left: bool, dpad_right: bool, stick_x: i16) -> i8 {
    if dpad_left || stick_x < -DEADZONE {
        -1
    } else if dpad_right || stick_x > DEADZONE {
        1
    } else {
        0
    }
}

/// Hold-to-repeat for a held direction: fires at once, then after `DELAY`, then every `EVERY`.
#[derive(Debug, Default)]
pub struct Repeat {
    dir: i8,
    next: Option<Instant>,
}

impl Repeat {
    pub const DELAY: Duration = Duration::from_millis(320);
    pub const EVERY: Duration = Duration::from_millis(110);

    /// Feed the direction that is held right now plus the clock; returns a step (-1/0/+1).
    pub fn tick(&mut self, dir: i8, now: Instant) -> i8 {
        if dir == 0 {
            self.dir = 0;
            self.next = None;
            return 0;
        }
        if dir != self.dir {
            self.dir = dir;
            self.next = Some(now + Self::DELAY);
            return dir;
        }
        match self.next {
            Some(t) if now >= t => {
                self.next = Some(now + Self::EVERY);
                dir
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_prefers_the_dpad_and_respects_the_deadzone() {
        assert_eq!(nav_x(true, false, 0), -1);
        assert_eq!(nav_x(false, true, 0), 1);
        assert_eq!(nav_x(false, false, 0), 0);
        assert_eq!(nav_x(false, false, DEADZONE), 0); // at the edge: still quiet
        assert_eq!(nav_x(false, false, DEADZONE + 1), 1);
        assert_eq!(nav_x(false, false, -DEADZONE - 1), -1);
        assert_eq!(nav_x(true, true, 30_000), -1); // both held: the d-pad wins
    }

    #[test]
    fn repeat_fires_immediately_then_on_schedule() {
        let t0 = Instant::now();
        let mut r = Repeat::default();
        assert_eq!(r.tick(1, t0), 1); // first press is instant
        assert_eq!(r.tick(1, t0 + Duration::from_millis(100)), 0); // quiet during the delay
        assert_eq!(r.tick(1, t0 + Repeat::DELAY), 1); // hold fires
        assert_eq!(r.tick(1, t0 + Repeat::DELAY + Duration::from_millis(50)), 0);
        assert_eq!(r.tick(1, t0 + Repeat::DELAY + Repeat::EVERY), 1);
    }

    #[test]
    fn repeat_resets_on_release_and_on_direction_change() {
        let t0 = Instant::now();
        let mut r = Repeat::default();
        assert_eq!(r.tick(1, t0), 1);
        assert_eq!(r.tick(0, t0 + Duration::from_millis(10)), 0); // released
        assert_eq!(r.tick(1, t0 + Duration::from_millis(20)), 1); // new press: instant again
        assert_eq!(r.tick(-1, t0 + Duration::from_millis(30)), -1); // flip: instant
        assert_eq!(r.tick(-1, t0 + Duration::from_millis(40)), 0); // then the delay applies
    }
}
