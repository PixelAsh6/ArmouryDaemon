//! Trigger engine: pure state machine driven by tracked-app start/stop events.
//!
//! Semantics (user-specified, revised 2026-10-18):
//! - The packs only switch **after the tracked state has been stable for the cooldown**, in both
//!   directions: a tracked app must be running for `cooldown` before the packs apply, and nothing
//!   may have been running for `cooldown` before they revert. Every state change starts (or
//!   cancels) that countdown, so a launcher that opens and closes quickly never causes a switch.
//! - Reopening during a pending revert cancels it; closing during a pending apply cancels that.
//! - Idle mode cancels switching entirely (entering reverts, leaving re-applies when one runs).
//! - Removing the last tracked app is not an app event: it reverts immediately.
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
    /// Nothing has been running for a cooldown: revert the packs.
    Reverted,
}

pub struct Engine {
    cooldown: Duration,
    /// Are the packs currently in the "triggered" state?
    applied: bool,
    /// Idle mode is up: packs are reverted and stay that way until idle ends.
    idle: bool,
    /// A switch waiting for the cooldown: (deadline, target applied state).
    pending: Option<(Instant, bool)>,
    /// exe file name (lowercase, the tracked identity) -> (display name, live pids)
    running: BTreeMap<String, (String, Vec<u32>)>,
    last_source: Option<String>,
}

impl Engine {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            applied: false,
            idle: false,
            pending: None,
            running: BTreeMap::new(),
            last_source: None,
        }
    }

    pub fn set_cooldown(&mut self, cooldown: Duration) {
        self.cooldown = cooldown;
    }

    /// What the user should see. `Cooldown` covers both directions (see `pending_action`).
    pub fn state(&self) -> EngineState {
        if self.pending.is_some() {
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

    /// How long until the pending switch, if one is scheduled.
    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.pending
            .map(|(deadline, _)| deadline.saturating_duration_since(now))
    }

    /// `true` when the pending switch is an apply, `false` for a revert.
    pub fn pending_action(&self) -> Option<bool> {
        self.pending.map(|(_, apply)| apply)
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

    /// Enter or leave idle mode (black screens + muted audio). Entering reverts whatever is
    /// applied; leaving re-applies when a tracked app is running right now. Returns the transition
    /// the daemon must run, if any.
    pub fn set_idle(&mut self, idle: bool) -> Option<Transition> {
        if self.idle == idle {
            return None;
        }
        self.idle = idle;
        // Idle cancels any pending switch: nothing may move while it is up.
        self.pending = None;
        if idle {
            if self.applied {
                self.applied = false;
                self.last_source = None;
                return Some(Transition::Reverted);
            }
            None
        } else if !self.running.is_empty() && !self.applied {
            // Back to normal with a tracked app still running: apply again (no cooldown: this is
            // a mode change, not an app start).
            self.applied = true;
            self.last_source = Some(self.source_label());
            Some(Transition::Applied)
        } else {
            None
        }
    }

    /// A tracked app was removed from the user's list: forget it entirely. If nothing tracked is
    /// running any more, the packs revert **now** instead of waiting out the cooldown — the user
    /// just said the app no longer matters (2026-10-17 feedback).
    pub fn on_untracked(&mut self, key: &str) -> Option<Transition> {
        self.running.remove(&key.to_ascii_lowercase());
        self.last_source = if self.running.is_empty() {
            None
        } else {
            Some(self.source_label())
        };
        if self.running.is_empty() {
            // No apply can be pending for an app we just stopped tracking.
            if self.pending.map(|(_, apply)| apply) == Some(true) {
                self.pending = None;
            }
            if self.applied {
                self.applied = false;
                self.pending = None;
                return Some(Transition::Reverted);
            }
        }
        None
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

        // While idle, a tracked app is just another app (idle cancels switching).
        if self.idle {
            return None;
        }
        // Already on, or an apply is already counting down: nothing to schedule.
        if self.applied || self.pending.map(|(_, apply)| apply) == Some(true) {
            // A pending revert is cancelled: the app is back before the cooldown elapsed.
            if self.pending.map(|(_, apply)| apply) == Some(false) {
                self.pending = None;
            }
            return None;
        }
        self.pending = Some((now + self.cooldown, true));
        None
    }

    pub fn on_stopped(&mut self, key: &str, pid: u32, now: Instant) -> Option<Transition> {
        let lookup = key.to_ascii_lowercase();
        if let Some(entry) = self.running.get_mut(&lookup) {
            entry.1.retain(|p| *p != pid);
            if entry.1.is_empty() {
                self.running.remove(&lookup);
            }
        }
        if self.running.is_empty() {
            self.last_source = None;
        } else {
            self.last_source = Some(self.source_label());
        }

        if self.idle {
            return None;
        }

        if self.running.is_empty() {
            if self.applied {
                // Start the revert countdown (keep an existing one: no reset on reopen/close churn).
                if self.pending.is_none() {
                    self.pending = Some((now + self.cooldown, false));
                }
            } else if self.pending.map(|(_, apply)| apply) == Some(true) {
                // Closed before the apply deadline: nothing to switch.
                self.pending = None;
            }
        }
        None
    }

    /// Advance time (called ~1/s). Returns the transition exactly when the pending switch is due.
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        let Some((deadline, apply)) = self.pending else {
            return None;
        };
        if now < deadline {
            return None;
        }
        // The tracked state must still match the target; app events cancel pending switches.
        if self.running.is_empty() == apply {
            self.pending = None;
            return None;
        }
        self.pending = None;
        if apply == self.applied {
            return None;
        }
        self.applied = apply;
        if !apply {
            self.last_source = None;
        }
        Some(if apply {
            Transition::Applied
        } else {
            Transition::Reverted
        })
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

    /// Drive the engine from idle to "packs applied" and return the engine.
    fn applied_engine(now: Instant) -> Engine {
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Applied));
        e
    }

    #[test]
    fn applying_waits_for_the_cooldown() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));

        assert_eq!(e.on_started("game.exe", "game.exe", 10, now), None);
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.pending_action(), Some(true));
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
        assert!(!e.is_applied());

        // just before the deadline: still nothing
        assert_eq!(e.tick(now + Duration::from_secs(C - 1)), None);
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.pending_action(), None);
    }

    #[test]
    fn a_quick_open_and_close_never_switches() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));

        // a launcher that opens and closes inside the cooldown: no switch at all
        e.on_started("setup.exe", "setup.exe", 5, now);
        e.on_stopped("setup.exe", 5, now + Duration::from_secs(5));
        assert_eq!(e.pending_action(), None);
        assert_eq!(e.state(), EngineState::Idle);
        assert!(!e.is_applied());
        assert_eq!(e.tick(now + Duration::from_secs(10 * C)), None);
        assert!(!e.is_applied());
    }

    #[test]
    fn reopening_during_the_pending_revert_cancels_it() {
        let now = t0();
        let mut e = applied_engine(now);

        e.on_stopped("game.exe", 1, now + Duration::from_secs(C + 1));
        assert_eq!(e.pending_action(), Some(false));
        assert_eq!(e.state(), EngineState::Cooldown);

        // back before the countdown ends: the packs stay applied
        e.on_started("game.exe", "game.exe", 2, now + Duration::from_secs(C + 10));
        assert_eq!(e.pending_action(), None);
        assert!(e.is_applied());
        assert_eq!(e.tick(now + Duration::from_secs(5 * C)), None);
        assert!(e.is_applied());
    }

    #[test]
    fn reverting_waits_for_the_cooldown_and_then_goes_idle() {
        let now = t0();
        let mut e = applied_engine(now);
        let closed = now + Duration::from_secs(C + 1);

        e.on_stopped("game.exe", 1, closed);
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(
            e.cooldown_remaining(closed),
            Some(Duration::from_secs(C))
        );
        assert_eq!(e.tick(closed + Duration::from_secs(C - 1)), None);
        assert_eq!(e.tick(closed + Duration::from_secs(C)), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.state(), EngineState::Idle);
    }

    #[test]
    fn a_second_instance_and_a_second_app_keep_the_state_stable() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("a.exe", "a.exe", 1, now);
        e.on_started("a.exe", "a.exe", 2, now);
        e.on_started("b.exe", "b.exe", 3, now);
        assert_eq!(e.source().as_deref(), Some("a.exe +1"));
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Applied));

        e.on_stopped("a.exe", 1, now + Duration::from_secs(C + 1));
        e.on_stopped("a.exe", 2, now + Duration::from_secs(C + 1));
        assert_eq!(e.state(), EngineState::Triggered); // b.exe still running
        e.on_stopped("b.exe", 3, now + Duration::from_secs(C + 1));
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.pending_action(), Some(false));
    }

    #[test]
    fn stop_without_start_is_ignored() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        assert_eq!(e.on_stopped("ghost.exe", 5, now), None);
        assert_eq!(e.state(), EngineState::Idle);
        assert!(!e.is_applied());
    }

    #[test]
    fn keys_are_case_insensitive() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("App.EXE", "App.EXE", 3, now);
        assert!(e.is_running("app.exe"));
        e.on_stopped("app.exe", 3, now);
        assert_eq!(e.state(), EngineState::Idle);
    }

    #[test]
    fn entering_idle_reverts_the_applied_packs() {
        let now = t0();
        let mut e = applied_engine(now);
        assert!(e.is_applied());

        assert_eq!(e.set_idle(true), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert!(e.is_idle_mode());
        assert_eq!(e.set_idle(true), None); // a second press changes nothing
    }

    #[test]
    fn idle_cancels_a_pending_switch() {
        let now = t0();
        let mut e = applied_engine(now);
        e.on_stopped("game.exe", 1, now + Duration::from_secs(C + 1));
        assert_eq!(e.pending_action(), Some(false));

        e.set_idle(true);
        assert_eq!(e.pending_action(), None);
        assert_eq!(e.tick(now + Duration::from_secs(5 * C)), None);
        assert_eq!(e.state(), EngineState::Idle);
    }

    #[test]
    fn while_idle_a_tracked_app_does_not_apply() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.set_idle(true);
        assert_eq!(e.on_started("game.exe", "game.exe", 1, now), None);
        assert_eq!(e.pending_action(), None);
        assert_eq!(e.tick(now + Duration::from_secs(5 * C)), None);
        assert!(!e.is_applied());
        e.on_stopped("game.exe", 1, now);
        assert_eq!(e.pending_action(), None);
    }

    #[test]
    fn last_tracked_app_removed_reverts_immediately() {
        let now = t0();
        let mut e = applied_engine(now);
        assert!(e.is_applied());

        assert_eq!(e.on_untracked("game.exe"), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.state(), EngineState::Idle);
        assert_eq!(e.cooldown_remaining(now), None);

        assert_eq!(e.on_untracked("ghost.exe"), None);
        assert!(!e.is_applied());
    }

    #[test]
    fn untracking_one_of_two_apps_keeps_the_packs_applied() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("a.exe", "a.exe", 1, now);
        e.on_started("b.exe", "b.exe", 2, now);
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Applied));

        assert_eq!(e.on_untracked("a.exe"), None);
        assert!(e.is_applied());
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.source().as_deref(), Some("b.exe"));
    }

    #[test]
    fn leaving_idle_reapplies_when_a_tracked_app_is_running() {
        let now = t0();
        let mut e = applied_engine(now);
        e.set_idle(true);
        assert!(!e.is_applied());

        assert_eq!(e.set_idle(false), Some(Transition::Applied));
        assert!(e.is_applied());
        assert_eq!(e.state(), EngineState::Triggered);

        e.set_idle(true);
        e.on_stopped("game.exe", 1, now + Duration::from_secs(C + 2));
        assert_eq!(e.set_idle(false), None);
        assert!(!e.is_applied());
        assert_eq!(e.state(), EngineState::Idle);
    }
}
