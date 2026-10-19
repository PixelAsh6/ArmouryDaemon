//! Controller watch for console mode: a thread polls the XInput slots and reports connects and
//! disconnects. The daemon decides what to do with them (open the launcher, log).
//!
//! Polling on purpose: XInput has no notification API, and the queries are microseconds when no
//! pad answers them.

use crossbeam_channel::Sender;
use std::time::Duration;

use crate::pad;

/// Pad hotplug events, as the daemon's loop sees them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PadEvent {
    Connected { slot: u32 },
    Disconnected { slot: u32 },
}

/// Poll interval — the launcher feels immediate on a fresh plug, and the daemon's own loop adds
/// up to a second of wait on top, so this poll is the term that has to stay small.
const EVERY: Duration = Duration::from_millis(500);

pub fn start(tx: Sender<PadEvent>, wake: usize) {
    std::thread::spawn(move || {
        // Seed with the current state: a pad that was already plugged when the daemon started
        // is not a "connect" (no launcher pops at logon).
        let mut seen = pad::connected_slots();
        loop {
            std::thread::sleep(EVERY);
            let now = pad::connected_slots();
            for slot in 0..pad::SLOTS {
                let s = slot as usize;
                if now[s] != seen[s] {
                    seen[s] = now[s];
                    let ev = if now[s] {
                        PadEvent::Connected { slot }
                    } else {
                        PadEvent::Disconnected { slot }
                    };
                    let _ = tx.send(ev);
                    if wake != 0 {
                        unsafe { windows_sys::Win32::System::Threading::SetEvent(wake as _) };
                    }
                }
            }
        }
    });
}
