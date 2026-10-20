//! Trigger engine: pure state machine driven by tracked-app start/stop events.
//!
//! Semantics (user-specified, revised 2026-10-20):
//! - A tracked-app event states the **intent**: the packs belong to the applied state while a
//!   tracked app runs, and back to their own state once the last one is gone. `desired` is that
//!   intent, `applied` is what the machine does — the two are allowed to differ, and that gap is
//!   exactly a deferred change.
//! - The packs follow the intent, but only when no **lockout** is running. Every change starts a
//!   lockout of `cooldown` seconds; while it runs no other change may touch the packs, and the
//!   intent that arrives meanwhile is *deferred*, not dropped — it is applied when the lockout
//!   expires.
//! - The lockout is a minimum spacing between two changes, and only a change that actually
//!   happened starts one: a lockout that expires while the intent still holds changes nothing.
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

pub struct Engine {
    cooldown: Duration,
    /// Are the packs applied right now?
    applied: bool,
    /// Should they be applied? (true = a tracked app is running)
    desired: bool,
    idle: bool,
    paused: bool,
    /// The settling window. While it is running, NO pack change may be made.
    lock_until: Option<Instant>,
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
            lock_until: None,
            running: BTreeMap::new(),
            last_source: None,
        }
    }

    pub fn set_cooldown(&mut self, cooldown: Duration) {
        self.cooldown = cooldown;
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
        self.lock_until
            .filter(|until| *until > now)
            .map(|until| until.saturating_duration_since(now))
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

    pub fn is_idle_mode(&self) -> bool {
        self.idle
    }

    /// Freeze/thaw *automatic* switching. Pausing drops the lockout (no pack change may happen)
    /// and makes the intent match the current state, so nothing is pending. Unpausing catches up
    /// at once: a running tracked app is applied right away, and with nothing running the revert
    /// is scheduled (the lockout times it). Manual actions never go through here (the daemon
    /// forces them).
    pub fn set_paused(&mut self, paused: bool, now: Instant) -> Option<Transition> {
        if self.paused == paused {
            return None;
        }
        self.paused = paused;
        if paused {
            // Freeze: drop the lockout, and let nothing be pending.
            self.lock_until = None;
            self.desired = self.applied;
            return None;
        }
        // Unpause: catch up with whatever is running.
        if !self.running.is_empty() {
            self.desired = true;
            self.lock_until = None;
            if !self.applied {
                self.applied = true;
                self.last_source = Some(self.source_label());
                return Some(Transition::Applied);
            }
            return None;
        }
        // Nothing is running: schedule the revert, the lockout times it.
        self.desired = false;
        self.lock_until = if self.applied {
            Some(now + self.cooldown)
        } else {
            None
        };
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
        self.lock_until = None;
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
        self.lock_until = None;
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
            self.lock_until = None;
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

    /// Advance time (called ~1/s). When the lockout expires the intent is applied under the
    /// same rules: if it still matches the state nothing happens (and no new lockout starts),
    /// otherwise the packs change and a new lockout begins.
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        self.try_change(now)
    }

    /// Make the packs match the intent — if that is allowed right now.
    ///
    /// A change is only allowed when no lockout is running: while one runs the intent is
    /// deferred, not dropped, and the lockout is not restarted. Idle and pause freeze the
    /// switching entirely. A change that does not happen starts no lockout.
    fn try_change(&mut self, now: Instant) -> Option<Transition> {
        // An expired lockout is gone.
        if self.lock_until.is_some_and(|until| until <= now) {
            self.lock_until = None;
        }
        if self.idle || self.paused {
            return None;
        }
        if self.lock_until.is_some() {
            return None; // the lockout runs: the change is deferred
        }
        if self.desired == self.applied {
            return None; // nothing to do — and no lockout for it
        }
        self.applied = self.desired;
        self.lock_until = Some(now + self.cooldown);
        if self.applied {
            self.last_source = Some(self.source_label());
            Some(Transition::Applied)
        } else {
            self.last_source = None;
            Some(Transition::Reverted)
        }
    }

    /// Is the lockout still running?
    fn lock_running(&self, now: Instant) -> bool {
        self.lock_until.is_some_and(|until| until > now)
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
    fn starting_applies_immediately_and_starts_the_lockout() {
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
    fn closing_after_the_lockout_reverts_immediately() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 10, now);
        // the lockout ends with the game still running: nothing happens
        assert_eq!(e.tick(now + Duration::from_secs(C)), None);

        // an hour later the game ends: no lockout runs, so the packs revert at once
        let closed = now + Duration::from_secs(3600);
        assert_eq!(
            e.on_stopped("game.exe", 10, closed),
            Some(Transition::Reverted)
        );
        assert!(!e.is_applied());
        assert_eq!(e.state(closed), EngineState::Cooldown);
        assert_eq!(e.cooldown_remaining(closed), Some(Duration::from_secs(C)));
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
    fn unpausing_with_nothing_running_schedules_the_revert() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.set_paused(true, now);
        e.on_stopped("game.exe", 1, now);

        // nothing runs and the packs are applied: the resume schedules the revert
        assert_eq!(e.set_paused(false, now), None);
        assert!(e.is_applied());
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Reverted));
        assert!(!e.is_applied());
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
}
