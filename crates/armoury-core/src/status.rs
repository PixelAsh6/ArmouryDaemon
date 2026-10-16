use serde::{Deserialize, Serialize};

/// Top-level daemon state machine (Phase 1: always Idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Idle,
    Triggered,
    Cooldown,
}

/// Snapshot of daemon status sent to clients over the pipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub daemon_version: String,
    pub uptime_secs: u64,
    pub state: RunState,
    pub trigger_source: Option<String>,
    pub cooldown_remaining_ms: Option<u64>,
    pub cooldown_seconds: u64,
    pub paused: bool,
    pub killswitch_active: bool,
    pub tracked_apps_running: Vec<String>,
}
