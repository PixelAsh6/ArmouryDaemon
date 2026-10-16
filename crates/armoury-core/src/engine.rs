//! Trigger engine: pure state machine driven by tracked-app start/stop events.
//!
//! Semantics (user-specified, 2026-10-16):
//! - A tracked app starting **applies** the packs immediately.
//! - When every tracked app has closed, a cooldown **starts and keeps counting**, no
//!   matter how often the app reopens (reopening never resets it).
//! - The packs only **revert** when the cooldown has expired.
//! - If the cooldown expires while a tracked app is running, nothing happens: we "stay
//!   ready". The switch happens on the *next* close, and that switch starts a new cooldown.
//! - After the new cooldown passes with nothing running we go idle (no further switching).
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
    /// When the pending revert becomes due (kept even after it has passed, so an
    /// overdue switch happens on the next close).
    deadline: Option<Instant>,
    /// exe file name (lowercase, the tracked identity) -> (display name, live pids)
    running: BTreeMap<String, (String, Vec<u32>)>,
    last_source: Option<String>,
}

impl Engine {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            applied: false,
            deadline: None,
            running: BTreeMap::new(),
            last_source: None,
        }
    }

    pub fn set_cooldown(&mut self, cooldown: Duration) {
        self.cooldown = cooldown;
    }

    /// Display state: what the user should see.
    pub fn state(&self) -> EngineState {
        if !self.running.is_empty() {
            EngineState::Triggered
        } else if self.deadline.is_some() {
            EngineState::Cooldown
        } else {
            EngineState::Idle
        }
    }

    pub fn source(&self) -> Option<String> {
        self.last_source.clone()
    }

    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
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

        // Reopening during a cooldown leaves the countdown alone ("stay ready").
        if self.applied {
            return None;
        }
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
        if self.applied {
            self.last_source = Some(self.source_label());
        }

        // Every tracked app closed → the countdown starts (once).
        if self.running.is_empty() && self.applied && self.deadline.is_none() {
            self.deadline = Some(now + self.cooldown);
        }
        None
    }

    /// Advance time (called ~1/s). Returns `Reverted` exactly when the packs must revert.
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        let Some(deadline) = self.deadline else {
            return None;
        };
        if now < deadline {
            return None;
        }

        // Expired while a tracked app is running: stay ready, switch on the next close.
        if !self.running.is_empty() {
            return None;
        }

        if self.applied {
            self.applied = false;
            self.last_source = None;
            // The switch starts a new cooldown (per the user's model).
            self.deadline = Some(now + self.cooldown);
            return Some(Transition::Reverted);
        }

        // Nothing applied any more and the extra cooldown has passed → idle.
        self.deadline = None;
        None
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
    fn start_applies_and_close_starts_the_cooldown() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));

        assert_eq!(
            e.on_started("game.exe", "game.exe", 10, now),
            Some(Transition::Applied)
        );
        assert_eq!(e.state(), EngineState::Triggered);
        assert!(e.is_applied());

        assert_eq!(e.on_stopped("game.exe", 10, now), None);
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(C)));
    }

    #[test]
    fn reopening_never_resets_the_countdown() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.on_stopped("game.exe", 1, now);

        // three quick reopens, as in the user's example
        for i in 0..3 {
            let t = now + Duration::from_secs(10 * (i + 1));
            assert_eq!(e.on_started("game.exe", "game.exe", 100 + i as u32, t), None);
            assert_eq!(e.state(), EngineState::Triggered);
            assert!(e.is_applied());
            assert_eq!(e.on_stopped("game.exe", 100 + i as u32, t), None);
            // the original deadline is untouched
            assert_eq!(
                e.cooldown_remaining(t),
                Some(Duration::from_secs(C) - Duration::from_secs(10 * (i + 1)))
            );
        }
    }

    #[test]
    fn expired_while_running_stays_ready_and_switches_on_the_next_close() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.on_stopped("game.exe", 1, now);

        // comes back and stays past the deadline
        let back = now + Duration::from_secs(30);
        e.on_started("game.exe", "game.exe", 2, back);
        assert_eq!(e.tick(now + Duration::from_secs(C + 5)), None); // stay ready
        assert_eq!(e.state(), EngineState::Triggered);
        assert!(e.is_applied());

        // next close: the switch happens (now overdue) and a new cooldown starts
        let close = now + Duration::from_secs(120);
        e.on_stopped("game.exe", 2, close);
        assert_eq!(e.tick(close), Some(Transition::Reverted));
        assert!(!e.is_applied());
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.cooldown_remaining(close), Some(Duration::from_secs(C)));

        // and once that new cooldown passes quietly we are idle
        assert_eq!(e.tick(close + Duration::from_secs(C)), None);
        assert_eq!(e.state(), EngineState::Idle);
        assert_eq!(e.cooldown_remaining(close + Duration::from_secs(C)), None);
    }

    #[test]
    fn quiet_cooldown_reverts_then_goes_idle() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.on_stopped("game.exe", 1, now);

        assert_eq!(e.tick(now + Duration::from_secs(C - 1)), None);
        assert_eq!(e.tick(now + Duration::from_secs(C)), Some(Transition::Reverted));
        assert_eq!(e.state(), EngineState::Cooldown); // the post-switch cooldown
        assert_eq!(e.tick(now + Duration::from_secs(2 * C)), None);
        assert_eq!(e.state(), EngineState::Idle);
    }

    #[test]
    fn start_after_a_revert_applies_again() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("game.exe", "game.exe", 1, now);
        e.on_stopped("game.exe", 1, now);
        e.tick(now + Duration::from_secs(C));

        let later = now + Duration::from_secs(C + 30);
        assert_eq!(
            e.on_started("game.exe", "game.exe", 9, later),
            Some(Transition::Applied)
        );
        assert!(e.is_applied());
    }

    #[test]
    fn second_instance_and_second_app_keep_it_applied() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(C));
        e.on_started("a.exe", "a.exe", 1, now);
        assert_eq!(e.on_started("a.exe", "a.exe", 2, now), None);
        assert_eq!(e.on_started("b.exe", "b.exe", 3, now), None);
        assert_eq!(e.source().as_deref(), Some("a.exe +1"));

        assert_eq!(e.on_stopped("a.exe", 1, now), None);
        assert_eq!(e.on_stopped("a.exe", 2, now), None);
        assert_eq!(e.state(), EngineState::Triggered); // b.exe still running
        assert_eq!(e.on_stopped("b.exe", 3, now), None);
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.source().as_deref(), Some("—"));
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
}
