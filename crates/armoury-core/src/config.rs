use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub schema_version: u32,
    pub cooldown_seconds: u64,
    pub tracked_apps: Vec<TrackedApp>,
    /// Per-pack user settings: enabled flag + the single dropdown value.
    pub packs: HashMap<String, PackUserConfig>,
    /// Pause all pack switching (tray menu / dashboard button).
    pub paused: bool,
    /// Idle overlay safety cap: force-quit after `idle_failsafe_minutes`.
    pub idle_failsafe_enabled: bool,
    pub idle_failsafe_minutes: u64,
    /// Where the dashboard was last left (logical points); `None` = the default bottom corner.
    #[serde(default)]
    pub ui_position: Option<[f32; 2]>,
    /// Pack catalog manifest URL; `None` = the official catalog.
    #[serde(default)]
    pub catalog_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackUserConfig {
    #[serde(default)]
    pub enabled: bool,
    /// setting key -> chosen value
    #[serde(default)]
    pub values: HashMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            cooldown_seconds: 60,
            tracked_apps: Vec::new(),
            packs: HashMap::new(),
            paused: false,
            idle_failsafe_enabled: true,
            idle_failsafe_minutes: 180,
            ui_position: None,
            catalog_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedApp {
    pub name: String,
    pub exe: String,
}

/// %APPDATA%\ArmouryDaemon (falls back to ./ArmouryDaemon if APPDATA is missing).
/// `ARMOURY_TEST_NS` redirects everything to a throw-away folder (integration harness only).
pub fn config_dir() -> PathBuf {
    if let Ok(ns) = std::env::var("ARMOURY_TEST_NS") {
        if !ns.trim().is_empty() {
            let base = std::env::temp_dir();
            return base.join(format!("ArmouryDaemon-test-{}", ns.trim()));
        }
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ArmouryDaemon")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Load the config; create the file with defaults if missing.
/// A malformed file is preserved on disk (never clobbered) and defaults are used in memory.
pub fn load_or_create() -> io::Result<AppConfig> {
    let path = config_path();
    if !path.exists() {
        let cfg = AppConfig::default();
        save(&cfg)?;
        return Ok(cfg);
    }
    let text = fs::read_to_string(&path)?;
    match serde_json::from_str::<AppConfig>(&text) {
        Ok(cfg) => Ok(cfg),
        Err(e) => {
            log::warn!(
                "config at {} is malformed ({e}); using defaults in memory, file left untouched",
                path.display()
            );
            Ok(AppConfig::default())
        }
    }
}

/// Atomic-ish save: write a temp file, then replace the target.
pub fn save(cfg: &AppConfig) -> io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(cfg).map_err(io::Error::other)?;
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.cooldown_seconds, 60);
        assert!(cfg.tracked_apps.is_empty());
    }

    #[test]
    fn missing_fields_get_defaults() {
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.cooldown_seconds, 60);
        assert_eq!(cfg.schema_version, 1);
    }

    #[test]
    fn roundtrip_serde() {
        let mut cfg = AppConfig::default();
        cfg.tracked_apps.push(TrackedApp {
            name: "Demo".into(),
            exe: "demo.exe".into(),
        });
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.tracked_apps.len(), 1);
        assert_eq!(back.tracked_apps[0].exe, "demo.exe");
    }

    #[test]
    fn ui_position_roundtrips_and_older_configs_still_load() {
        let mut cfg = AppConfig::default();
        cfg.ui_position = Some([120.0, 340.5]);
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.ui_position, Some([120.0, 340.5]));

        // a config written before the field existed must still load (None, not an error)
        let old: AppConfig = serde_json::from_str(r#"{"cooldown_seconds":30}"#).unwrap();
        assert_eq!(old.cooldown_seconds, 30);
        assert_eq!(old.ui_position, None);
    }
}
