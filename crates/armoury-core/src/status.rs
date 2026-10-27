use crate::config::{ConsoleApp, TrackedApp, default_console_open};
use serde::{Deserialize, Serialize};

/// Top-level daemon state machine (Phase 1: always Idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    #[default]
    Idle,
    Triggered,
    Cooldown,
}

/// The last thing that went wrong, so the dashboard can say so instead of only the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastError {
    /// Unix seconds.
    pub at: u64,
    /// Where it came from, e.g. "pack:g-helper", "catalog", "art".
    pub source: String,
    /// The human-readable message.
    pub message: String,
}

/// A snapshot that does not carry the field came from an older daemon: its killswitch has
/// neither a config key nor an IPC request, so nothing can have turned it off.
fn default_killswitch_enabled() -> bool {
    true
}

/// Snapshot of daemon status sent to clients over the pipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    #[serde(default)]
    pub daemon_version: String,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub state: RunState,
    #[serde(default)]
    pub trigger_source: Option<String>,
    #[serde(default)]
    pub cooldown_remaining_ms: Option<u64>,
    /// The change the lockout is holding back, if any: `"apply"` or `"revert"`. `None` when
    /// nothing is waiting - the machine is where the intent says it is, or the window is not
    /// holding anything. The field stays in the wire format so that older clients keep
    /// loading.
    #[serde(default)]
    pub cooldown_action: Option<String>,
    #[serde(default)]
    pub cooldown_seconds: u64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub killswitch_active: bool,
    /// How many tracked apps the killswitch is currently hiding/muting — the distinct
    /// processes it has hidden a window for or muted a session for. 0 when it is off.
    #[serde(default)]
    pub killswitch_count: u32,
    /// The daemon's config-level killswitch flag (`false`: the toggle is a no-op).
    #[serde(default = "default_killswitch_enabled")]
    pub killswitch_enabled: bool,
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
    /// The global hotkeys as the settings window shows them (`ctrl+alt+k`, `home`). Defaults
    /// on the wire so an older daemon's snapshot still loads.
    #[serde(default)]
    pub hotkeys: crate::config::Hotkeys,
    /// Feature 9: the newer daemon the release feed named, if any.
    #[serde(default)]
    pub update_available: Option<String>,
    /// What the last self-update check or install said, ready to show.
    #[serde(default)]
    pub update_message: String,
    /// The user's configured tracked-app list.
    #[serde(default)]
    pub tracked_apps: Vec<TrackedApp>,
    /// Display names of tracked apps that are currently running.
    #[serde(default)]
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
    /// The last failure (a pack step, the catalog, the cover art); `None` = nothing went wrong.
    #[serde(default)]
    pub last_error: Option<LastError>,
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
    /// What the last apply found when it went back to look (2026-10-27): a pack that claims it
    /// changed something, but whose captured value is exactly what it was before, is reported
    /// here instead of being shown as a silent success. `None` = nothing to report.
    #[serde(default)]
    pub note: Option<String>,
}

/// One dropdown of a pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackSettingStatus {
    pub key: String,
    pub label: String,
    pub values: Vec<String>,
    pub value: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_from_an_older_daemon_still_loads() {
        let s: StatusSnapshot = serde_json::from_str(r#"{"daemon_version":"1.0.0"}"#).unwrap();
        assert_eq!(s.daemon_version, "1.0.0");
        assert_eq!(s.state, RunState::Idle);
        assert!(s.last_error.is_none());
        assert!(s.tracked_apps.is_empty());
        assert_eq!(s.cooldown_seconds, 0);
        assert!(s.killswitch_enabled, "an older daemon's killswitch is on");
    }

    #[test]
    fn the_last_error_round_trips() {
        let err = LastError {
            at: 1_700_000_000,
            source: "pack:g-helper".into(),
            message: "hotkey did not land".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert_eq!(
            json,
            r#"{"at":1700000000,"source":"pack:g-helper","message":"hotkey did not land"}"#
        );
        let back: LastError = serde_json::from_str(&json).unwrap();
        assert_eq!(back.at, 1_700_000_000);
        assert_eq!(back.source, "pack:g-helper");
        assert_eq!(back.message, "hotkey did not land");
    }
}
