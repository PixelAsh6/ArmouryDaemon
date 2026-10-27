//! Trigger engine: pure state machine driven by tracked-app start/stop events.
//!
//! Semantics (user-specified, revised 2026-10-27):
//! - A tracked-app event states the **intent**: the packs belong to the applied state while a
//!   tracked app runs, and back to their own state once the last one is gone. `desired` is that
//!   intent, `applied` is what the machine does — the two are allowed to differ, and that gap is
//!   exactly a deferred change.
//! - A change lands when **no window is running**, and the cooldown **starts when the mode
//!   changed** — that is, after the packs were switched, not when the app opened or closed.
//! - While a window runs, nothing is checked and nothing changes: a start or a stop only moves
//!   the **intent**. When the window ends, the tick compares the intent with the machine and
//!   lands one change if the two differ.
//! - The **apply** therefore lands at once when the machine is free — the first launch of a
//!   session — and a launch inside a running window waits for it: the packs stay where they are
//!   until that window is over, and go to the applied state only if the app is still running by
//!   then (2026-10-22).
//! - The **revert** is just as immediate: a close that arrives after the cooldown has run out
//!   puts the packs back **at once** and starts a cooldown of its own — it does not wait a
//!   second cooldown for the privilege of going back (2026-10-27). A close inside a window waits
//!   for that window, and a fast restart inside it cancels the change: the packs stay applied.
//! - A window that expires while the intent still holds changes **nothing** and starts **no new**
//!   window: only a change that actually happened starts one.
//! - A change of the cooldown **re-times the window that is running**: the window keeps its
//!   start and takes the new length, so it ends at `start + cooldown`. The setting is in force
//!   at once, and a window is never a mix of the old and the new length (2026-10-22). A window
//!   that has already ended is dropped with it, never re-timed (2026-10-23).
//! - The capture of the "previous" state therefore always happens on a settled machine: a fast
//!   app restart can no longer capture the daemon's own value back (the "stuck on Turbo" bug),
//!   and the carry no longer has to paper over it.
//! - Idle mode cancels switching entirely (entering reverts, leaving re-applies immediately).
//! - Manual actions — removing a tracked app, pausing, entering or leaving idle — bypass the
//!   lockout: they change the packs at once, drop a running lockout and start none. Unpausing is
//!   the one exception: with nothing running it schedules the revert, so a lockout is started to
//!   time that revert.
//! - The engine only believes a change when the daemon really performed it: a transition the
//!   daemon drops (the packs are paused) is handed back to the engine, which goes back to the
//!   state the machine is in. The intent is kept, so the change happens later, not never.
//!
//! Deliberately free of Windows/clock dependencies so it can be unit-tested.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Triggered,
    Cooldown,
}

/// What the daemon must do to the packs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Tracked app(s) is/are running: apply the packs.
    Applied,
    /// The intent is gone while the packs are applied: revert them.
    Reverted,
}

/// The settling window: it began at `start` and runs for `len`.
///
/// The window is kept as its **start** and its **length**, not as a deadline, so that a
/// change of the cooldown can re-time the window that is running: the window keeps its
/// start and takes the new length.
#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    len: Duration,
}

impl Window {
    fn new(start: Instant, len: Duration) -> Self {
        Self { start, len }
    }

    /// When the window ends.
    fn end(&self) -> Instant {
        self.start + self.len
    }
}

pub struct Engine {
    cooldown: Duration,
    /// Are the packs applied right now?
    applied: bool,
    /// Should they be applied? (true = a tracked app is running)
    desired: bool,
    idle: bool,
    paused: bool,
    /// The settling window. While it is running, no change may touch the packs.
    lock: Option<Window>,
    /// exe file name (lowercase, the tracked identity) -> (display name, live pids)
    running: BTreeMap<String, (String, Vec<u32>)>,
    last_source: Option<String>,
}

impl Engine {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            applied: false,
            desired: false,
            idle: false,
            paused: false,
            lock: None,
            running: BTreeMap::new(),
            last_source: None,
        }
    }

    /// Set the length of the settling window.
    ///
    /// A window that is running is **re-timed**: it keeps its start and takes the new
    /// length, so it ends at `start + cooldown`. The setting is in force at once, and a
    /// window is never a mix of the old and the new length.
    ///
    /// A new length that is shorter than the time the window has already run ends the
    /// window at once — a window cannot end before it began — and the change it was
    /// holding lands with the next tick.
    ///
    /// A window that has **already ended** is dropped, not re-timed: the engine keeps it
    /// until a tick reaps it, and the daemon drains a pipe request *before* that tick runs,
    /// so a new length used to bring a dead window back to life — a countdown on the
    /// dashboard and a held launch for a cooldown that was never really running.
    pub fn set_cooldown(&mut self, cooldown: Duration, now: Instant) {
        self.cooldown = cooldown;
        self.lock = match self.lock {
            Some(w) if w.end() > now => Some(Window { len: cooldown, ..w }),
            _ => None,
        };
    }

    /// What the user should see: the lockout first, then the state of the packs.
    pub fn state(&self, now: Instant) -> EngineState {
        if self.lock_running(now) {
            EngineState::Cooldown
        } else if self.applied {
            EngineState::Triggered
        } else {
            EngineState::Idle
        }
    }

    pub fn source(&self) -> Option<String> {
        self.last_source.clone()
    }

    /// How much of the lockout is left.
    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.lock
            .map(|w| w.end())
            .filter(|end| *end > now)
            .map(|end| end.saturating_duration_since(now))
    }

    /// Display names of the tracked apps that are currently running.
    pub fn running_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.running.values().map(|(name, _)| name.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Is any instance of this tracked app (keyed by exe file name, lowercase) still up?
    pub fn is_running(&self, key: &str) -> bool {
        self.running.contains_key(&key.to_ascii_lowercase())
    }

    /// Are the packs applied right now?
    pub fn is_applied(&self) -> bool {
        self.applied
    }

    /// The change that is waiting for the window, if there is one: the packs are not where
    /// the intent says they should be, and a window is holding the change back. `None` when
    /// nothing is waiting — the machine is where the intent says it is. A difference can only
    /// be held by a running window: without one, the next tick lands it.
    pub fn pending_change(&self) -> Option<Transition> {
        if self.idle || self.paused || self.desired == self.applied {
            return None;
        }
        Some(if self.desired {
            Transition::Applied
        } else {
            Transition::Reverted
        })
    }

    pub fn is_idle_mode(&self) -> bool {
        self.idle
    }

    /// Freeze/thaw *automatic* switching. Pausing drops the window (no pack change may happen)
    /// and makes the intent match the current state, so nothing is held back. Unpausing catches
    /// up at once — the window is gone, so there is nothing to wait for — and it is the user's
    /// own action, so it is performed like every other manual one: it changes the packs and
    /// starts **no** window. Manual actions never go through here (the daemon forces them).
    pub fn set_paused(&mut self, paused: bool, now: Instant) -> Option<Transition> {
        if self.paused == paused {
            return None;
        }
        // A resume is the user's own action: the clock plays no part in it.
        let _ = now;
        self.paused = paused;
        if paused {
            // Freeze: drop the window. The intent matches the machine, so nothing is held.
            self.lock = None;
            self.desired = self.applied;
            return None;
        }
        // Unpause: catch up at once, with no window in the way.
        if !self.running.is_empty() {
            self.desired = true;
            self.lock = None;
            if self.applied {
                return None;
            }
            self.applied = true;
            self.last_source = Some(self.source_label());
            return Some(Transition::Applied);
        }
        // Nothing is running: the revert lands now. A manual action starts no window.
        self.desired = false;
        self.lock = None;
        if self.applied {
            self.applied = false;
            self.last_source = None;
            return Some(Transition::Reverted);
        }
        None
    }

    /// Enter or leave idle mode (black screens + muted audio). Entering reverts whatever is
    /// applied; leaving re-applies when a tracked app is running right now. A mode change is
    /// deliberate: it drops a running lockout and starts none.
    pub fn set_idle(&mut self, idle: bool) -> Option<Transition> {
        if self.idle == idle {
            return None;
        }
        self.idle = idle;
        self.lock = None;
        if idle {
            if self.applied {
                self.applied = false;
                self.last_source = None;
                return Some(Transition::Reverted);
            }
            None
        } else if !self.running.is_empty() && !self.applied {
            // Back to normal with a tracked app still running: apply again (no lockout: this is
            // a mode change, not an app start).
            self.applied = true;
            self.desired = true;
            self.last_source = Some(self.source_label());
            Some(Transition::Applied)
        } else {
            None
        }
    }

    /// The daemon could not perform the transition it asked for (the packs are paused and the
    /// transition was not one of the user's own actions). The machine is still in the state it
    /// was in, so the engine goes back to it: `applied` may only change when the daemon really
    /// did the change. The intent is kept — the change is deferred, not dropped — and the
    /// lockout the change started is dropped with it, because nothing happened.
    pub fn transition_not_performed(&mut self, t: Transition) {
        match t {
            Transition::Applied => self.applied = false,
            Transition::Reverted => self.applied = true,
        }
        self.lock = None;
    }

    /// A tracked app was removed from the user's list: forget it entirely. This is a manual
    /// action, so it bypasses the lockout: with nothing tracked left the packs revert at once
    /// and no lockout is left behind (the next tracked app may apply right away). While paused
    /// the manual action still takes effect (manual actions bypass pause, G12).
    pub fn on_untracked(&mut self, key: &str, now: Instant) -> Option<Transition> {
        self.running.remove(&key.to_ascii_lowercase());
        self.last_source = if self.running.is_empty() {
            None
        } else {
            Some(self.source_label())
        };
        if self.running.is_empty() {
            self.desired = false;
        }
        if self.running.is_empty() && self.applied {
            // Manual: revert at once, drop the lockout, start none.
            self.applied = false;
            self.last_source = None;
            self.lock = None;
            return Some(Transition::Reverted);
        }
        self.try_change(now)
    }

    pub fn on_started(
        &mut self,
        key: &str,
        name: &str,
        pid: u32,
        now: Instant,
    ) -> Option<Transition> {
        let entry = self
            .running
            .entry(key.to_ascii_lowercase())
            .or_insert_with(|| (name.to_string(), Vec::new()));
        if !entry.1.contains(&pid) {
            entry.1.push(pid);
        }
        self.last_source = Some(self.source_label());
        // The intent: a tracked app runs, so the packs should be applied.
        self.desired = true;
        self.try_change(now)
    }

    pub fn on_stopped(&mut self, key: &str, pid: u32, now: Instant) -> Option<Transition> {
        let lookup = key.to_ascii_lowercase();
        if let Some(entry) = self.running.get_mut(&lookup) {
            entry.1.retain(|p| *p != pid);
            if entry.1.is_empty() {
                self.running.remove(&lookup);
            }
        }
        self.last_source = if self.running.is_empty() {
            None
        } else {
            Some(self.source_label())
        };
        if self.running.is_empty() {
            // The intent is gone — but the change itself may have to wait for the lockout.
            self.desired = false;
        }
        self.try_change(now)
    }

    /// Advance time (called ~1/s). When the window that holds a change expires, the change
    /// lands — unless the intent holds again, in which case nothing happens (and no new
    /// lockout starts).
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        self.try_change(now)
    }

    /// Make the packs match the intent.
    ///
    /// One rule, both directions: the change lands when **no window is running**, and it
    /// starts a window of its own — the cooldown begins the moment the packs are switched.
    ///
    /// While a window is running nothing is decided: a start or a stop has already moved the
    /// intent, and the change waits. It is the end of the window that looks at the intent
    /// again, so a close that lands in the middle of a cooldown is applied when the cooldown
    /// ends, and a launch inside one is applied when it ends if the app still runs. A change
    /// that the intent has taken back in the meantime never happens at all.
    ///
    /// The revert is immediate for the same reason: once the cooldown of the apply has run
    /// out, a close has no window in the way, so the packs go back at once — and the revert
    /// starts a cooldown of its own (2026-10-27). Waiting for a window first made a close cost
    /// two cooldowns.
    ///
    /// Idle and pause freeze the switching entirely. A change that does not happen starts no
    /// window.
    fn try_change(&mut self, now: Instant) -> Option<Transition> {
        if self.idle || self.paused {
            return None;
        }
        if self.desired == self.applied {
            // The intent holds and the machine is where it should be: a change the window was
            // holding is cancelled, and an expired window is gone — it starts no new one.
            if self.lock.is_some_and(|w| w.end() <= now) {
                self.lock = None;
            }
            return None;
        }
        // The intent and the machine differ, and a window is still running: nothing is done
        // during the window. The tick at its end looks at the intent again.
        if self.lock_running(now) {
            return None;
        }
        // The machine is free: the change lands, and the cooldown starts with it.
        self.applied = self.desired;
        self.lock = Some(Window::new(now, self.cooldown));
        self.last_source = if self.applied {
            Some(self.source_label())
        } else {
            None
        };
        Some(if self.applied {
            Transition::Applied
        } else {
            Transition::Reverted
        })
    }

    /// Is the lockout still running?
    fn lock_running(&self, now: Instant) -> bool {
        self.lock.is_some_and(|w| w.end() > now)
    }

    fn source_label(&self) -> String {
        let names = self.running_names();
        match names.len() {
            0 => "—".to_string(),
            1 => names[0].clone(),
            n => format!("{} +{}", names[0], n - 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: u64 = 60;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_start_with_no_window_running_applies_at_once_and_starts_a_window() {
        // The machine is free: no window is running, so the change lands at once and starts
        // a window of its own. This is the first launch of a session.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));

        assert_eq!(
            e.on_started("game.exe", "game.exe", 10, now),
            Some(Transition::Applied)
        );
        assert!(e.is_applied());
        assert_eq!(e.state(now), EngineState::Cooldown);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
    }

    #[test]
    fn the_lockout_expires_without_a_change_when_the_intent_still_holds() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 10, now);

        // the lockout runs out while the game is still up: nothing changes, no new lockout
        let expiry = now + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), None);
        assert!(e.is_applied());
        assert_eq!(e.state(expiry), EngineState::Triggered);
        assert_eq!(e.cooldown_remaining(expiry), None);
    }

    #[test]
    fn closing_an_hour_later_still_reverts_at_once() {
        // Nothing about a long session changes the rule: with the cooldown long over, the
        // close has no window in the way and the packs go back with it.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 10, now);
        // the lockout ends with the game still running: nothing happens
        assert_eq!(e.tick(now + Duration::from_secs(C)), None);

        // an hour later the game ends: the revert lands with the close
        let closed = now + Duration::from_secs(3600);
        assert_eq!(
            e.on_stopped("game.exe", 10, closed),
            Some(Transition::Reverted)
        );
        assert!(!e.is_applied());
        assert_eq!(e.state(closed), EngineState::Cooldown);
        assert_eq!(e.cooldown_remaining(closed), Some(Duration::from_secs(C)));

        // the window it started runs out quietly: nothing is performed twice
        assert_eq!(e.tick(closed + Duration::from_secs(C)), None);
        assert!(!e.is_applied());
        assert_eq!(e.state(closed + Duration::from_secs(C)), EngineState::Idle);
    }

    #[test]
    fn a_fast_restart_after_the_lockout_reverts_and_reapplies_later() {
        // The trade-off the 2026-10-27 rule accepts, recorded here so nobody re-learns it the
        // hard way: with the cooldown already over, a close reverts at once, so a restart that
        // lands in the revert's own window is applied when that window ends. A restart inside
        // the cooldown of the apply still never reverts — `a_fast_restart_never_reverts`.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        assert_eq!(
            e.on_started("game.exe", "game.exe", 1, now),
            Some(Transition::Applied)
        );
        assert!(e.is_applied());

        // the lockout runs out with the game still up: nothing changes, no new lockout
        let expiry = now + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), None);
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(expiry), None);

        // the game is closed: the revert lands with it, and starts a window
        let closed = now + Duration::from_secs(C + 5);
        assert_eq!(
            e.on_stopped("game.exe", 1, closed),
            Some(Transition::Reverted)
        );
        assert!(!e.is_applied());

        // the game is opened again inside that window: the apply waits for it
        let reopened = closed + Duration::from_secs(5);
        assert_eq!(e.on_started("game.exe", "game.exe", 2, reopened), None);
        assert!(!e.is_applied(), "the apply waits for the window");
        assert_eq!(e.pending_change(), Some(Transition::Applied));

        // the window ends with the game running: the packs come back
        let held = closed + Duration::from_secs(C);
        assert_eq!(e.tick(held), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.state(held), EngineState::Cooldown);
    }

    #[test]
    fn a_start_while_the_packs_are_reverted_waits_for_the_window() {
        // The lockout holds a change in both directions (2026-10-22): a tracked app that
        // starts while a window is running does not get the packs at once — the apply waits
        // for the window and lands when it is over, if the app is still running by then.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        // the game closes: the window that holds the revert starts
        assert_eq!(
            e.on_stopped("game.exe", 1, now + Duration::from_secs(5)),
            None
        );
        // the window runs out with the game gone: the revert lands and starts a window
        let reverted = now + Duration::from_secs(C);
        assert_eq!(e.tick(reverted), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(
            e.cooldown_remaining(reverted),
            Some(Duration::from_secs(C)),
            "the revert starts a window of its own"
        );

        // the game starts again while that window runs: the apply is held
        let started = reverted + Duration::from_secs(1);
        assert_eq!(
            e.on_started("game.exe", "game.exe", 2, started),
            None,
            "the apply waits for the window"
        );
        assert!(!e.is_applied(), "the packs stay where they are");
        assert_eq!(
            e.cooldown_remaining(started),
            Some(Duration::from_secs(C - 1)),
            "the window is not restarted"
        );

        // the window is over and the game still runs: the apply lands
        let expiry = reverted + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), Some(Transition::Applied));
        assert!(e.is_applied());
        // and the apply starts a window of its own
        assert_eq!(e.cooldown_remaining(expiry), Some(Duration::from_secs(C)));
    }

    #[test]
    fn a_change_inside_a_window_is_held_in_both_directions() {
        // While a window runs, neither the apply nor the revert may touch the packs: a change
        // that arrives inside it is held, not dropped (2026-10-22).
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));

        // the machine is free: the apply lands at once and starts a window
        assert_eq!(
            e.on_started("game.exe", "game.exe", 1, now),
            Some(Transition::Applied)
        );
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));

        // the revert inside that window is held
        let closed = now + Duration::from_secs(10);
        assert_eq!(e.on_stopped("game.exe", 1, closed), None);
        assert!(e.is_applied(), "the packs stay applied while the window runs");

        // the window is over: the revert lands and starts a window of its own
        let expiry = now + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.cooldown_remaining(expiry), Some(Duration::from_secs(C)));

        // the apply inside that window is held too
        let started = expiry + Duration::from_secs(5);
        assert_eq!(
            e.on_started("game.exe", "game.exe", 2, started),
            None,
            "the apply waits for the window"
        );
        assert!(!e.is_applied(), "the packs stay where they are");
        assert_eq!(
            e.cooldown_remaining(started),
            Some(Duration::from_secs(C - 5)),
            "the window is not restarted"
        );

        // the window is over and the app still runs: the apply lands
        let applied = expiry + Duration::from_secs(C);
        assert_eq!(e.tick(applied), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(applied), Some(Duration::from_secs(C)));
    }

    #[test]
    fn a_held_apply_is_cancelled_when_the_app_closes_again() {
        // The apply was held by a window; the app closes before the window is over, so the
        // intent takes the held change back and it never lands.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        // the close inside the window is held
        assert_eq!(e.on_stopped("game.exe", 1, now), None);
        let reverted = now + Duration::from_secs(C);
        assert_eq!(e.tick(reverted), Some(Transition::Reverted));

        // the app starts inside the window the revert started: the apply is held
        let started = reverted + Duration::from_secs(5);
        assert_eq!(e.on_started("game.exe", "game.exe", 2, started), None);
        assert!(!e.is_applied());

        // the app closes again inside the window: the held apply is cancelled
        let closed = started + Duration::from_secs(5);
        assert_eq!(e.on_stopped("game.exe", 2, closed), None);
        assert!(!e.is_applied());

        // the window is over: nothing lands, the packs stay where they are
        let end = reverted + Duration::from_secs(C);
        assert_eq!(e.tick(end), None);
        assert!(!e.is_applied(), "the cancelled apply does not land");
    }

    #[test]
    fn a_held_apply_lands_exactly_once() {
        // The apply waits for the window, lands when it is over — and only once.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert_eq!(e.on_stopped("game.exe", 1, now), None);
        let reverted = now + Duration::from_secs(C);
        assert_eq!(e.tick(reverted), Some(Transition::Reverted));

        // the app starts inside the window: the apply is held
        let started = reverted + Duration::from_secs(1);
        assert_eq!(e.on_started("game.exe", "game.exe", 2, started), None);
        assert!(!e.is_applied());

        // the window is over: the apply lands and starts a window of its own
        let expiry = reverted + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(expiry), Some(Duration::from_secs(C)));

        // nothing more happens: the apply is not performed twice
        assert_eq!(e.tick(expiry + Duration::from_secs(1)), None);
        assert_eq!(e.tick(expiry + Duration::from_secs(C + 1)), None);
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(expiry + Duration::from_secs(C + 1)), None);
        assert_eq!(
            e.state(expiry + Duration::from_secs(C + 1)),
            EngineState::Triggered
        );
    }

    #[test]
    fn a_close_after_the_window_reverts_at_once() {
        // The cooldown of the apply has run out and the game closes: there is no window in the
        // way, so the packs go back now — and the revert starts a cooldown of its own
        // (2026-10-27: a close used to wait a whole cooldown before it was performed).
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        assert_eq!(
            e.on_started("game.exe", "game.exe", 1, now),
            Some(Transition::Applied)
        );
        // the window runs out with the game still up: nothing changes and no window is left
        let expiry = now + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), None);
        assert_eq!(e.cooldown_remaining(expiry), None);

        // the game closes after the window: the revert lands at once
        let closed = expiry + Duration::from_secs(1);
        assert_eq!(
            e.on_stopped("game.exe", 1, closed),
            Some(Transition::Reverted),
            "a close with no window running reverts at once"
        );
        assert!(!e.is_applied());
        // and it starts a window of its own
        assert_eq!(e.cooldown_remaining(closed), Some(Duration::from_secs(C)));
        assert_eq!(e.state(closed), EngineState::Cooldown);
    }

    #[test]
    fn a_restart_after_the_revert_waits_for_the_new_window() {
        // The close reverts at once and starts a cooldown; a restart inside that cooldown waits
        // for it, and the packs come back when it ends (2026-10-27).
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        // the machine settles: the lockout of the start runs out
        assert_eq!(e.tick(now + Duration::from_secs(C)), None);

        // the game closes: the revert lands at once, and starts a window
        let closed = now + Duration::from_secs(C + 5);
        assert_eq!(
            e.on_stopped("game.exe", 1, closed),
            Some(Transition::Reverted)
        );
        assert!(!e.is_applied());
        assert_eq!(e.cooldown_remaining(closed), Some(Duration::from_secs(C)));

        // the game starts again inside that window: the apply waits
        let reopened = closed + Duration::from_secs(5);
        assert_eq!(e.on_started("game.exe", "game.exe", 2, reopened), None);
        assert!(!e.is_applied(), "the apply waits for the window");
        assert_eq!(e.pending_change(), Some(Transition::Applied));
        assert_eq!(
            e.cooldown_remaining(reopened),
            Some(Duration::from_secs(C - 5)),
            "the window is not restarted"
        );

        // the window ends with the game still running: the apply lands and starts one of its own
        let held = closed + Duration::from_secs(C);
        assert_eq!(e.tick(held), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(held), Some(Duration::from_secs(C)));
        assert_eq!(e.pending_change(), None);
    }

    #[test]
    fn a_revert_lands_exactly_once() {
        // The revert lands at once and starts its own window; nothing is performed twice, and
        // the window it started runs out quietly.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert_eq!(e.tick(now + Duration::from_secs(C)), None);

        // the game closes after the window: the revert lands at once
        let closed = now + Duration::from_secs(C + 5);
        assert_eq!(
            e.on_stopped("game.exe", 1, closed),
            Some(Transition::Reverted)
        );
        assert!(!e.is_applied());
        assert_eq!(e.cooldown_remaining(closed), Some(Duration::from_secs(C)));

        // nothing more happens: the revert is not performed twice
        assert_eq!(e.tick(closed + Duration::from_secs(1)), None);
        assert_eq!(e.tick(closed + Duration::from_secs(C + 1)), None);
        assert!(!e.is_applied());
        assert_eq!(e.cooldown_remaining(closed + Duration::from_secs(C + 1)), None);
        assert_eq!(e.state(closed + Duration::from_secs(C + 1)), EngineState::Idle);
    }

    #[test]
    fn a_manual_action_bypasses_a_running_lockout() {
        // A manual action happens at once, even while the lockout runs: removing the tracked
        // app reverts the packs right away and leaves no lockout behind.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));

        assert_eq!(e.on_untracked("game.exe", now), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.state(now), EngineState::Idle);
        assert_eq!(
            e.cooldown_remaining(now),
            None,
            "a manual action leaves no lockout behind"
        );
    }

    #[test]
    fn a_close_inside_the_lockout_is_deferred_and_applied_at_the_expiry() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);

        // closed while the lockout runs: deferred, not dropped
        let closed = now + Duration::from_secs(30);
        assert_eq!(e.on_stopped("game.exe", 1, closed), None);
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(closed), Some(Duration::from_secs(30)));

        // the lockout ends: the deferred revert happens now and starts a new lockout
        let expiry = now + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.cooldown_remaining(expiry), Some(Duration::from_secs(C)));
    }

    #[test]
    fn a_fast_restart_never_reverts() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);

        // closed 5 s in, reopened 2 s later: both are deferred by the lockout
        assert_eq!(e.on_stopped("game.exe", 1, now + Duration::from_secs(5)), None);
        assert_eq!(
            e.on_started("game.exe", "game.exe", 2, now + Duration::from_secs(7)),
            None
        );
        assert!(e.is_applied());

        // the lockout ends with the game running again: the intent holds, so nothing happens
        let expiry = now + Duration::from_secs(C);
        assert_eq!(e.tick(expiry), None);
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(expiry), None);
        assert_eq!(e.state(expiry), EngineState::Triggered);
    }

    #[test]
    fn a_second_instance_and_a_second_app_keep_it_applied() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        assert_eq!(e.on_started("a.exe", "a.exe", 1, now), Some(Transition::Applied));
        assert_eq!(e.on_started("a.exe", "a.exe", 2, now), None);
        assert_eq!(e.on_started("b.exe", "b.exe", 3, now), None);
        assert_eq!(e.source().as_deref(), Some("a.exe +1"));
        // the intent already holds, so no new lockout was started
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));

        // a.exe closes, b.exe is still up: the intent holds, so nothing changes
        e.on_stopped("a.exe", 1, now);
        e.on_stopped("a.exe", 2, now);
        assert!(e.is_applied());
        assert_eq!(e.source().as_deref(), Some("b.exe"));

        // the last one closes while the lockout runs: deferred
        e.on_stopped("b.exe", 3, now);
        assert!(e.is_applied());
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Reverted));
    }

    #[test]
    fn stop_without_start_is_ignored() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        assert_eq!(e.on_stopped("ghost.exe", 5, now), None);
        assert_eq!(e.state(now), EngineState::Idle);
        assert!(!e.is_applied());
    }

    #[test]
    fn keys_are_case_insensitive() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("App.EXE", "App.EXE", 3, now);
        assert!(e.is_running("app.exe"));
        e.on_stopped("app.exe", 3, now);
        // the close is deferred by the lockout the start began
        assert!(e.is_applied());
        assert_eq!(e.state(now), EngineState::Cooldown);
    }

    #[test]
    fn entering_idle_reverts_the_applied_packs() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);

        assert_eq!(e.set_idle(true), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert!(e.is_idle_mode());
        assert_eq!(e.set_idle(true), None); // a second press changes nothing
        assert_eq!(e.cooldown_remaining(now), None); // the lockout is dropped
    }

    #[test]
    fn while_idle_a_tracked_app_does_not_apply() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.set_idle(true);
        assert_eq!(e.on_started("game.exe", "game.exe", 1, now), None);
        assert!(!e.is_applied());
        e.on_stopped("game.exe", 1, now);
        assert_eq!(e.cooldown_remaining(now), None);
        assert_eq!(e.state(now), EngineState::Idle);
    }

    #[test]
    fn removing_the_last_tracked_app_reverts_at_once() {
        // A manual action bypasses the lockout: the packs go back right away and no lockout is
        // left behind.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));

        assert_eq!(e.on_untracked("game.exe", now), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.state(now), EngineState::Idle);
        assert_eq!(e.cooldown_remaining(now), None);
    }

    #[test]
    fn removing_the_last_app_while_paused_reverts_now() {
        // A manual action still takes effect while the automatic switching is frozen (G12).
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.set_paused(true, now);

        assert_eq!(e.on_untracked("game.exe", now), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.state(now), EngineState::Idle);
        assert_eq!(e.cooldown_remaining(now), None);
    }

    #[test]
    fn a_transition_the_daemon_could_not_perform_is_undone() {
        // The daemon asks for the change, then drops it (the packs are paused): the machine
        // did not change, so the engine must not believe it did.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert!(e.is_applied());

        // the revert the daemon could not perform
        e.transition_not_performed(Transition::Reverted);
        assert!(e.is_applied(), "the packs are still applied");
        assert_eq!(e.cooldown_remaining(now), None, "no change, no lockout");

        // the apply the daemon could not perform
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert_eq!(e.set_idle(true), Some(Transition::Reverted));
        assert_eq!(e.set_idle(false), Some(Transition::Applied));
        assert!(e.is_applied());
        e.transition_not_performed(Transition::Applied);
        assert!(!e.is_applied(), "the packs were never applied");
    }

    #[test]
    fn removing_an_app_that_already_closed_reverts_at_once() {
        // The close was deferred by the lockout; the manual removal is not.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.on_stopped("game.exe", 1, now);
        assert!(e.is_applied());

        assert_eq!(e.on_untracked("game.exe", now), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.cooldown_remaining(now), None);
    }

    #[test]
    fn untracking_one_of_two_apps_keeps_the_packs_applied() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("a.exe", "a.exe", 1, now);
        e.on_started("b.exe", "b.exe", 2, now);
        assert_eq!(e.on_untracked("a.exe", now), None);
        assert!(e.is_applied());
        assert_eq!(e.source().as_deref(), Some("b.exe"));
        // the lockout the first apply started still runs
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
    }

    #[test]
    fn a_game_started_right_after_an_untrack_applies_immediately() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert_eq!(e.on_untracked("game.exe", now), Some(Transition::Reverted));

        // the manual revert left no lockout behind, so the next game applies at once
        let started = now + Duration::from_secs(1);
        assert_eq!(
            e.on_started("other.exe", "other.exe", 2, started),
            Some(Transition::Applied)
        );
        assert!(e.is_applied());
    }

    #[test]
    fn pause_freezes_automatic_switching_and_unpause_catches_up() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));

        // pause before anything runs: a start does nothing (the app is still tracked)
        e.set_paused(true, now);
        assert_eq!(e.on_started("game.exe", "game.exe", 1, now), None);
        assert!(!e.is_applied());
        assert!(e.running_names().contains(&"game.exe".to_string()));

        // unpausing with the app running applies at once
        assert_eq!(e.set_paused(false, now), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(now), None, "a manual action starts no lockout");

        // pausing drops the lockout and leaves nothing pending
        e.set_paused(true, now);
        e.on_stopped("game.exe", 1, now);
        assert_eq!(e.cooldown_remaining(now), None);
        assert!(e.is_applied(), "the packs stay as they are while paused");
        assert_eq!(e.tick(now + Duration::from_secs(5 * C)), None);
        assert!(e.is_applied());
    }

    #[test]
    fn unpausing_with_nothing_running_reverts_at_once() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.set_paused(true, now);
        e.on_stopped("game.exe", 1, now);

        // nothing runs and the packs are applied: the resume puts them back at once
        assert_eq!(
            e.set_paused(false, now),
            Some(Transition::Reverted),
            "nothing waits for a window the pause threw away"
        );
        assert!(!e.is_applied());
        assert_eq!(
            e.cooldown_remaining(now),
            None,
            "a manual action starts no window"
        );
    }

    #[test]
    fn a_mode_change_drops_the_lockout() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));

        // idle is deliberate: it drops the lockout and starts none
        assert_eq!(e.set_idle(true), Some(Transition::Reverted));
        assert_eq!(e.cooldown_remaining(now), None);
        assert_eq!(e.set_idle(false), Some(Transition::Applied));
        assert_eq!(e.cooldown_remaining(now), None);
        assert_eq!(e.state(now), EngineState::Triggered);
    }

    #[test]
    fn leaving_idle_reapplies_when_a_tracked_app_is_running() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.set_idle(true);
        assert!(!e.is_applied());

        assert_eq!(e.set_idle(false), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.state(now), EngineState::Triggered);

        e.set_idle(true);
        e.on_stopped("game.exe", 1, now);
        assert_eq!(e.set_idle(false), None);
        assert!(!e.is_applied());
        assert_eq!(e.state(now), EngineState::Idle);
    }

    #[test]
    fn a_cooldown_changed_while_a_window_runs_re_times_that_window() {
        // The setting must be in force at once (2026-10-22): a window is never a mix of the
        // old and the new length. The window keeps its start and takes the new length, so
        // it ends at `start + cooldown`.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(10));
        // the first launch: the apply lands at once and starts a window of 10 s
        assert_eq!(
            e.on_started("game.exe", "game.exe", 1, now),
            Some(Transition::Applied)
        );
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(10)));

        // the user changes the cooldown to 60 s while that window is running
        e.set_cooldown(Duration::from_secs(60), now);

        // the window in flight now runs 60 s from its start, not 10 s
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(60)));
        assert_eq!(
            e.cooldown_remaining(now + Duration::from_secs(59)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            e.cooldown_remaining(now + Duration::from_secs(60)),
            None,
            "the window ends at its start plus the new length"
        );
    }

    #[test]
    fn a_cooldown_changed_while_a_window_runs_moves_the_expiry_the_change_waits_for() {
        // The mirror of the case above: the change the window is holding lands at the NEW
        // expiry, not at the old one (2026-10-22). This is the 22 Oct report: the window
        // was armed with the old 10 s, the setting landed a second later, and the revert
        // landed in the gap before the game came back.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(10));
        assert_eq!(
            e.on_started("game.exe", "game.exe", 1, now),
            Some(Transition::Applied)
        );
        // the game closes inside the window: the revert is held
        assert_eq!(e.on_stopped("game.exe", 1, now + Duration::from_secs(5)), None);
        assert!(e.is_applied());
        assert_eq!(e.pending_change(), Some(Transition::Reverted));

        // the user changes the cooldown to 60 s while the window is running
        e.set_cooldown(Duration::from_secs(60), now + Duration::from_secs(5));

        // the old expiry passes: nothing lands — the window now ends 60 s after the start
        let old_expiry = now + Duration::from_secs(10);
        assert_eq!(e.tick(old_expiry), None);
        assert!(e.is_applied(), "the revert waits for the new expiry");
        assert_eq!(e.pending_change(), Some(Transition::Reverted));

        // the new expiry: the revert lands, and it starts a window of its own
        let new_expiry = now + Duration::from_secs(60);
        assert_eq!(e.tick(new_expiry), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.pending_change(), None, "nothing is waiting any more");
        assert_eq!(
            e.cooldown_remaining(new_expiry),
            Some(Duration::from_secs(60)),
            "the window of its own has the new length"
        );
    }

    #[test]
    fn a_cooldown_shortened_past_the_time_the_window_ran_ends_the_window() {
        // A window cannot end before it began: a new length that is shorter than the time
        // the window has already run ends it at once, and the change it was holding lands
        // with the next tick. The window is not re-armed from now — the user asked for a
        // short window, and more than that has passed.
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        assert_eq!(
            e.on_started("game.exe", "game.exe", 1, now),
            Some(Transition::Applied)
        );
        // the game closes inside the window: the revert is held
        assert_eq!(e.on_stopped("game.exe", 1, now + Duration::from_secs(30)), None);
        assert!(e.is_applied());

        // the user shortens the cooldown to 5 s, but the window has already run 30 s
        e.set_cooldown(Duration::from_secs(5), now + Duration::from_secs(30));
        assert_eq!(
            e.cooldown_remaining(now + Duration::from_secs(30)),
            None,
            "the window cannot end before it began"
        );

        // the change it was holding lands now
        assert_eq!(
            e.tick(now + Duration::from_secs(30)),
            Some(Transition::Reverted)
        );
        assert!(!e.is_applied());
    }

    #[test]
    fn a_cooldown_changed_while_the_window_is_settled_does_not_resurrect_it() {
        // The daemon drains a pipe request *before* the 1 Hz housekeeping runs `tick`, so a
        // window that has ended can still be in the engine when a new length arrives. Re-timing
        // it made a dead window alive again: the dashboard showed a fresh countdown and the
        // next launch was held for it (23 Oct review).
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(5));
        assert_eq!(
            e.on_started("game.exe", "game.exe", 1, now),
            Some(Transition::Applied)
        );
        // the game closes inside the window: the revert is held
        assert_eq!(e.on_stopped("game.exe", 1, now + Duration::from_secs(1)), None);
        // the window ends: the revert lands and starts a window of its own [5, 10]
        assert_eq!(e.tick(now + Duration::from_secs(5)), Some(Transition::Reverted));
        assert!(!e.is_applied());

        // 11 s: that window has ended too, and nothing is waiting. No tick has reaped it.
        let settled = now + Duration::from_secs(11);
        assert_eq!(e.state(settled), EngineState::Idle);
        e.set_cooldown(Duration::from_secs(60), settled);

        assert_eq!(
            e.cooldown_remaining(settled),
            None,
            "a window that has ended is not re-timed"
        );
        assert_eq!(e.state(settled), EngineState::Idle);
        // and the next launch is not held behind a window that never started
        assert_eq!(
            e.on_started("game.exe", "game.exe", 2, settled),
            Some(Transition::Applied),
            "the dead window must not hold the next launch"
        );
    }
}
