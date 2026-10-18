//! Trigger engine: pure state machine driven by tracked-app start/stop events.
//!
//! Semantics (user-specified, revised 2026-10-18, final):
//! - A tracked app starting **applies the packs immediately**.
//! - When the last one closes, a **cooldown** starts; the packs revert when it expires, and not
//!   before — closing during the cooldown never reverts early.
//! - Reopening during the cooldown cancels it (the packs stay applied); closing again starts a
//!   fresh cooldown.
//! - After the revert the engine is simply Idle: there is **no second cooldown**.
//! - Idle mode cancels switching entirely (entering reverts, leaving re-applies immediately).
//! - Removing the last tracked app reverts immediately (manual action, no cooldown).
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
    /// The cooldown expired with nothing running: revert the packs.
    Reverted,
}

pub struct Engine {
    cooldown: Duration,
    /// Are the packs currently in the "triggered" state?
    applied: bool,
    /// Idle mode is up: packs are reverted and stay that way until idle ends.
    idle: bool,
    /// Paused: automatic switching is frozen (manual actions still run through the daemon).
    paused: bool,
    /// When the pending revert becomes due (set when the last tracked app closes).
    pending_revert: Option<Instant>,
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
            paused: false,
            pending_revert: None,
            running: BTreeMap::new(),
            last_source: None,
        }
    }

    pub fn set_cooldown(&mut self, cooldown: Duration) {
        self.cooldown = cooldown;
    }

    /// What the user should see.
    pub fn state(&self) -> EngineState {
        if self.pending_revert.is_some() {
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

    /// How long until the pending revert, if one is scheduled.
    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.pending_revert
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Always a revert when a cooldown is showing (the apply is immediate since 2026-10-18).
    pub fn pending_action(&self) -> Option<bool> {
        self.pending_revert.map(|_| false)
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

    /// Freeze/thaw *automatic* switching. Pausing drops any pending revert (the packs stay as they
    /// are); unpausing catches up — apply at once if a tracked app is running, otherwise start the
    /// revert cooldown. Manual actions never go through here (the daemon forces them).
    pub fn set_paused(&mut self, paused: bool, now: Instant) -> Option<Transition> {
        if self.paused == paused {
            return None;
        }
        self.paused = paused;
        if paused {
            self.pending_revert = None;
            return None;
        }
        if !self.running.is_empty() {
            if !self.applied {
                self.applied = true;
                self.last_source = Some(self.source_label());
                return Some(Transition::Applied);
            }
        } else if self.applied {
            self.pending_revert = Some(now + self.cooldown);
        }
        None
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
        self.pending_revert = None;
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
        if self.running.is_empty() && self.applied {
            self.applied = false;
            self.pending_revert = None;
            return Some(Transition::Reverted);
        }
        None
    }

    pub fn on_started(
        &mut self,
        key: &str,
        name: &str,
        pid: u32,
        _now: Instant,
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
        // While paused, automatic switching is frozen (the app is still tracked).
        if self.paused {
            return None;
        }
        if self.applied {
            // Already on: a reopen cancels the pending revert (the packs stay applied).
            self.pending_revert = None;
            return None;
        }
        // Apply straight away: the cooldown only governs the revert.
        self.pending_revert = None;
        self.applied = true;
        Some(Transition::Applied)
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

        if self.idle {
            return None;
        }
        if self.paused {
            return None;
        }

        // Every tracked app closed → start the countdown (once; reopening cancels it).
        if self.running.is_empty() && self.applied && self.pending_revert.is_none() {
            self.pending_revert = Some(now + self.cooldown);
        }
        None
    }

    /// Advance time (called ~1/s). Returns `Reverted` exactly when the cooldown expires with
    /// nothing running.
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        if self.paused {
            return None;
        }
        let Some(deadline) = self.pending_revert else {
            return None;
        };
        if now < deadline {
            return None;
        }
        // Expired while a tracked app is running: stay ready (the next close starts a new one).
        if !self.running.is_empty() {
            self.pending_revert = None;
            return None;
        }
        self.pending_revert = None;
        if !self.applied {
            return None;
        }
        self.applied = false;
        self.last_source = None;
        Some(Transition::Reverted)
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
    fn starting_applies_immediately_and_closing_starts_the_cooldown() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));

        assert_eq!(
            e.on_started("game.exe", "game.exe", 10, now),
            Some(Transition::Applied)
        );
        assert!(e.is_applied());
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.cooldown_remaining(now), None);

        assert_eq!(e.on_stopped("game.exe", 10, now), None);
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
    }

    #[test]
    fn nothing_reverts_before_the_cooldown_expires() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        // closed 10 s after applying: still a full cooldown, never an early revert
        let closed = now + Duration::from_secs(10);
        e.on_stopped("game.exe", 1, closed);
        assert_eq!(e.tick(now + Duration::from_secs(C - 1)), None);
        assert_eq!(e.tick(closed + Duration::from_secs(C - 1)), None);
        assert!(e.is_applied());
        assert_eq!(e.tick(closed + Duration::from_secs(C)), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.state(), EngineState::Idle);
    }

    #[test]
    fn reopening_during_the_cooldown_cancels_the_revert() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.on_stopped("game.exe", 1, now);

        // back before the countdown ends: the packs stay applied
        e.on_started("game.exe", "game.exe", 2, now + Duration::from_secs(10));
        assert_eq!(e.cooldown_remaining(now), None);
        assert!(e.is_applied());
        assert_eq!(e.tick(now + Duration::from_secs(5 * C)), None);
        assert!(e.is_applied());

        // closing again starts a fresh countdown
        e.on_stopped("game.exe", 2, now + Duration::from_secs(30));
        assert_eq!(
            e.cooldown_remaining(now + Duration::from_secs(30)),
            Some(Duration::from_secs(C))
        );
    }

    #[test]
    fn a_second_instance_and_a_second_app_keep_it_applied() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        assert_eq!(e.on_started("a.exe", "a.exe", 1, now), Some(Transition::Applied));
        assert_eq!(e.on_started("a.exe", "a.exe", 2, now), None);
        assert_eq!(e.on_started("b.exe", "b.exe", 3, now), None);
        assert_eq!(e.source().as_deref(), Some("a.exe +1"));

        e.on_stopped("a.exe", 1, now);
        e.on_stopped("a.exe", 2, now);
        assert_eq!(e.state(), EngineState::Triggered); // b.exe still running
        e.on_stopped("b.exe", 3, now);
        assert_eq!(e.state(), EngineState::Cooldown);
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
        assert_eq!(e.state(), EngineState::Cooldown);
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
    }

    #[test]
    fn idle_cancels_a_pending_revert() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.on_stopped("game.exe", 1, now);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));

        e.set_idle(true);
        assert_eq!(e.cooldown_remaining(now), None);
        assert_eq!(e.tick(now + Duration::from_secs(5 * C)), None);
        assert_eq!(e.state(), EngineState::Idle);
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
        assert_eq!(e.state(), EngineState::Idle);
    }

    #[test]
    fn last_tracked_app_removed_reverts_immediately() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
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
        assert_eq!(e.on_untracked("a.exe"), None);
        assert!(e.is_applied());
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.source().as_deref(), Some("b.exe"));
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

        // pause with a pending revert: the revert is dropped, the packs stay applied
        e.on_stopped("game.exe", 1, now);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
        e.set_paused(true, now);
        assert_eq!(e.cooldown_remaining(now), None);
        assert_eq!(e.tick(now + Duration::from_secs(5 * C)), None);
        assert!(e.is_applied());

        // closing while paused starts no countdown either
        e.on_started("game.exe", "game.exe", 2, now);
        e.on_stopped("game.exe", 2, now);
        assert_eq!(e.cooldown_remaining(now), None);

        // unpausing with nothing running schedules the revert, which then fires normally
        e.set_paused(false, now);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Reverted));
        assert!(!e.is_applied());
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
        assert_eq!(e.state(), EngineState::Triggered);

        e.set_idle(true);
        e.on_stopped("game.exe", 1, now);
        assert_eq!(e.set_idle(false), None);
        assert!(!e.is_applied());
        assert_eq!(e.state(), EngineState::Idle);
    }
}
