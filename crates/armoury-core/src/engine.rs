//! Trigger engine: pure state machine driven by tracked-app start/stop events.
//!
//! Idle → Triggered (any tracked app runs) → Cooldown (all closed; countdown) → Idle.
//! A tracked app starting during the cooldown cancels the revert (delay-revert model).
//! Deliberately free of any Windows or clock dependency so it can be unit-tested.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Triggered,
    Cooldown,
}

/// Something the rest of the daemon must react to (Phase 3 wires packs to these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// At least one tracked app is running: packs should apply.
    Triggered,
    /// All tracked apps closed: the revert countdown started.
    Cooldown,
    /// The countdown finished with nothing running: packs should revert.
    Reverted,
}

pub struct Engine {
    cooldown: Duration,
    state: EngineState,
    /// exe path (lowercase) -> (display name, live pids)
    running: BTreeMap<String, (String, Vec<u32>)>,
    cooldown_until: Option<Instant>,
    last_source: Option<String>,
}

impl Engine {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            state: EngineState::Idle,
            running: BTreeMap::new(),
            cooldown_until: None,
            last_source: None,
        }
    }

    pub fn set_cooldown(&mut self, cooldown: Duration) {
        self.cooldown = cooldown;
    }

    pub fn state(&self) -> EngineState {
        self.state
    }

    pub fn source(&self) -> Option<String> {
        self.last_source.clone()
    }

    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.cooldown_until
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Display names of the tracked apps that are currently running.
    pub fn running_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.running.values().map(|(name, _)| name.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn on_started(&mut self, exe: &str, name: &str, pid: u32, _now: Instant) -> Option<Transition> {
        let key = exe.to_ascii_lowercase();
        let entry = self
            .running
            .entry(key)
            .or_insert_with(|| (name.to_string(), Vec::new()));
        if !entry.1.contains(&pid) {
            entry.1.push(pid);
        }

        if self.state == EngineState::Triggered {
            self.last_source = Some(self.source_label());
            return None;
        }

        // Idle or Cooldown → trigger (a start during the cooldown cancels the revert).
        self.state = EngineState::Triggered;
        self.cooldown_until = None;
        self.last_source = Some(self.source_label());
        Some(Transition::Triggered)
    }

    pub fn on_stopped(&mut self, exe: &str, pid: u32, now: Instant) -> Option<Transition> {
        let key = exe.to_ascii_lowercase();
        if let Some(entry) = self.running.get_mut(&key) {
            entry.1.retain(|p| *p != pid);
            if entry.1.is_empty() {
                self.running.remove(&key);
            }
        }

        if self.running.is_empty() && self.state == EngineState::Triggered {
            self.state = EngineState::Cooldown;
            self.cooldown_until = Some(now + self.cooldown);
            return Some(Transition::Cooldown);
        }
        if self.state == EngineState::Triggered {
            self.last_source = Some(self.source_label());
        }
        None
    }

    /// Advance time-based state (call ~1/s). Returns `Reverted` exactly once when the
    /// countdown finishes.
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        if self.state != EngineState::Cooldown {
            return None;
        }
        if let Some(deadline) = self.cooldown_until {
            if now >= deadline {
                self.state = EngineState::Idle;
                self.cooldown_until = None;
                self.last_source = None;
                return Some(Transition::Reverted);
            }
        }
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

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn start_triggers_and_stop_starts_cooldown() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));

        assert_eq!(e.on_started("C:\\a\\game.exe", "game.exe", 10, now), Some(Transition::Triggered));
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.source().as_deref(), Some("game.exe"));

        assert_eq!(e.on_stopped("C:\\a\\game.exe", 10, now), Some(Transition::Cooldown));
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.cooldown_remaining(now), Some(Duration::from_secs(60)));
    }

    #[test]
    fn second_instance_keeps_it_triggered_until_all_close() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        assert!(e.on_started("a.exe", "a.exe", 1, now).is_some());
        assert!(e.on_started("a.exe", "a.exe", 2, now).is_none());
        assert_eq!(e.on_stopped("a.exe", 1, now), None);
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.on_stopped("a.exe", 2, now), Some(Transition::Cooldown));
    }

    #[test]
    fn two_different_apps_keep_it_triggered() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        e.on_started("a.exe", "a.exe", 1, now);
        e.on_started("b.exe", "b.exe", 2, now);
        assert_eq!(e.on_stopped("a.exe", 1, now), None);
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.source().as_deref(), Some("b.exe"));
    }

    #[test]
    fn cooldown_expires_to_reverted_once() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        e.on_started("a.exe", "a.exe", 1, now);
        e.on_stopped("a.exe", 1, now);

        assert_eq!(e.tick(now + Duration::from_secs(59)), None);
        assert_eq!(e.state(), EngineState::Cooldown);
        assert_eq!(e.tick(now + Duration::from_secs(60)), Some(Transition::Reverted));
        assert_eq!(e.state(), EngineState::Idle);
        assert_eq!(e.tick(now + Duration::from_secs(120)), None);
    }

    #[test]
    fn restart_during_cooldown_cancels_the_revert() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        e.on_started("a.exe", "a.exe", 1, now);
        e.on_stopped("a.exe", 1, now);

        let later = now + Duration::from_secs(30);
        assert_eq!(
            e.on_started("a.exe", "a.exe", 7, later),
            Some(Transition::Triggered)
        );
        assert_eq!(e.state(), EngineState::Triggered);
        assert_eq!(e.cooldown_remaining(later), None);
        // the original deadline must not fire later
        assert_eq!(e.tick(now + Duration::from_secs(61)), None);
    }

    #[test]
    fn stop_without_start_is_ignored() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        assert_eq!(e.on_stopped("ghost.exe", 5, now), None);
        assert_eq!(e.state(), EngineState::Idle);
    }

    #[test]
    fn names_are_case_insensitive_per_exe() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        e.on_started("C:\\X\\App.EXE", "App.EXE", 3, now);
        assert_eq!(e.on_stopped("c:\\x\\app.exe", 3, now), Some(Transition::Cooldown));
    }

    #[test]
    fn multiple_running_apps_are_labelled() {
        let now = t0();
        let mut e = Engine::new(Duration::from_secs(60));
        e.on_started("a.exe", "a.exe", 1, now);
        let tr = e.on_started("b.exe", "b.exe", 2, now);
        assert_eq!(tr, None); // already triggered, but the label updates
        assert_eq!(e.source().as_deref(), Some("a.exe +1"));
        assert_eq!(e.running_names().len(), 2);
    }
}
