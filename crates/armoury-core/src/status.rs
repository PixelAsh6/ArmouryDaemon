use crate::config::TrackedApp;
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
    /// How many tracked apps the killswitch is currently hiding/muting.
    #[serde(default)]
    pub killswitch_count: u32,
    /// Idle overlay state.
    #[serde(default)]
    pub idle_active: bool,
    #[serde(default)]
    pub idle_monitors: u32,
    #[serde(default)]
    pub idle_devices: u32,
    #[serde(default)]
    pub idle_failsafe_enabled: bool,
    #[serde(default)]
    pub idle_failsafe_minutes: u64,
    /// The user's configured tracked-app list.
    pub tracked_apps: Vec<TrackedApp>,
    /// Display names of tracked apps that are currently running.
    pub tracked_apps_running: Vec<String>,
    /// Installed packs and their state, for the dashboard.
    #[serde(default)]
    pub packs: Vec<PackStatus>,
}

/// One installed pack, as the dashboard sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackStatus {
    pub id: String,
    pub name: String,
    /// Pack version, shown small in the pack window.
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    /// The pack's single dropdown (v1).
    pub setting_key: Option<String>,
    pub setting_label: Option<String>,
    pub values: Vec<String>,
    pub value: Option<String>,
    /// Currently applied (game mode on) per the journal.
    pub applied: bool,
}
