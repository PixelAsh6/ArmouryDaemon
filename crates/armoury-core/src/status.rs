use crate::config::{ConsoleApp, TrackedApp, default_console_open};
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
    /// While in Cooldown: what the pending switch will do — `"apply"` or `"revert"`.
    #[serde(default)]
    pub cooldown_action: Option<String>,
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
    /// Console launcher auto-open: `"always"` (daemon startup), `"pad"` (controller connect),
    /// `"never"`. The tray's manual open works regardless.
    #[serde(default = "default_console_open")]
    pub console_open: String,
    /// Apps pinned to the console launcher.
    #[serde(default)]
    pub console_apps: Vec<ConsoleApp>,
    /// Console: exe base names (lowercase) the band hides — tracked apps kept out of the strip.
    #[serde(default)]
    pub console_hidden: Vec<String>,
    /// Console: exe base name (lowercase) -> unix seconds of the last play (band recency order).
    #[serde(default)]
    pub last_played: std::collections::HashMap<String, u64>,
    /// SteamGridDB: whether an API key is configured (drives the "no key set" hint).
    #[serde(default)]
    pub sgdb_api_key_set: bool,
    /// SteamGridDB: the current key, for the settings field's masked prefill. The pipe is
    /// local-only and the key already lives in a local config.json.
    #[serde(default)]
    pub sgdb_api_key: String,
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
    /// Every dropdown the pack exposes. The dashboard shows the first one; the pack window all.
    #[serde(default)]
    pub settings: Vec<PackSettingStatus>,
    /// Currently applied (game mode on) per the journal.
    pub applied: bool,
}

/// One dropdown of a pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackSettingStatus {
    pub key: String,
    pub label: String,
    pub values: Vec<String>,
    pub value: Option<String>,
}
