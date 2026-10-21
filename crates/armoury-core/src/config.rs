use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CONSOLE_OPEN_ALWAYS: &str = "always";
pub const CONSOLE_OPEN_PAD: &str = "pad";
pub const CONSOLE_OPEN_NEVER: &str = "never";

/// Unset or unrecognised value = the historical behaviour (`pad`).
pub fn normalize_console_open(value: &str) -> &'static str {
    match value.trim().to_ascii_lowercase().as_str() {
        CONSOLE_OPEN_ALWAYS => CONSOLE_OPEN_ALWAYS,
        CONSOLE_OPEN_NEVER => CONSOLE_OPEN_NEVER,
        _ => CONSOLE_OPEN_PAD,
    }
}

pub fn default_console_open() -> String {
    CONSOLE_OPEN_PAD.to_string()
}

fn de_console_open<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(d)?;
    Ok(normalize_console_open(&raw).to_string())
}

/// The config schema this build understands and writes.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// How many `config.json.bad.N` copies are kept before a timestamped name is used.
const MAX_BACKUPS: usize = 5;

/// Config keys this build does not understand any more: they are dropped when the file is
/// read, so the next save writes the file without them. ("ui_position" was the dashboard's
/// last position; the dashboard always opens in the corner and the field was never used.)
const RETIRED_KEYS: &[&str] = &["ui_position"];

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
    /// Console launcher auto-open: `always` (daemon startup), `pad` (controller connect), `never`.
    /// The tray's manual open is never affected.
    #[serde(default = "default_console_open", deserialize_with = "de_console_open")]
    pub console_open: String,
    /// Apps pinned to the console launcher without being tracked.
    pub console_apps: Vec<ConsoleApp>,
    /// Exe base names (lowercase) the console band hides: tracked apps kept out of the strip.
    #[serde(default)]
    pub console_hidden: Vec<String>,
    /// Idle overlay safety cap: force-quit after `idle_failsafe_minutes`.
    pub idle_failsafe_enabled: bool,
    pub idle_failsafe_minutes: u64,
    /// Pack catalog manifest URL; `None` = the official catalog.
    #[serde(default)]
    pub catalog_url: Option<String>,
    /// Console launcher recency: exe base name (lowercase) -> unix seconds of the last play.
    #[serde(default)]
    pub last_played: HashMap<String, u64>,
    /// SteamGridDB API key for cover art (https://www.steamgriddb.com/profile/preferences).
    #[serde(default)]
    pub sgdb_api_key: Option<String>,
    /// The keys this build does not know. They are written back untouched, so a config
    /// written by a newer build is not silently truncated by the next save.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
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
            console_open: default_console_open(),
            console_apps: Vec::new(),
            console_hidden: Vec::new(),
            idle_failsafe_enabled: true,
            idle_failsafe_minutes: 180,
            catalog_url: None,
            last_played: HashMap::new(),
            sgdb_api_key: None,
            extra: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedApp {
    pub name: String,
    pub exe: String,
}

/// An app pinned to the console launcher. Launchable from the pad UI but never tracked, so it
/// triggers no pack switching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleApp {
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
/// A malformed file is never destroyed: its bytes are kept next to it (`config.json.bad`)
/// and the defaults are used in memory.
pub fn load_or_create() -> io::Result<AppConfig> {
    load_or_create_at(&config_path())
}

fn load_or_create_at(path: &Path) -> io::Result<AppConfig> {
    if !path.exists() {
        let cfg = AppConfig::default();
        save_at(path, &cfg)?;
        return Ok(cfg);
    }
    let text = fs::read_to_string(path)?;
    match serde_json::from_str::<AppConfig>(&text) {
        Ok(mut cfg) => {
            // Keys this build used to keep and no longer understands are dropped, so the
            // next save cleans them out of the file. Keys of a *newer* build are kept (see
            // `extra`).
            for key in RETIRED_KEYS {
                cfg.extra.remove(*key);
            }
            if cfg.schema_version > SUPPORTED_SCHEMA_VERSION {
                log::warn!(
                    "config at {} has schema version {} — this build supports {SUPPORTED_SCHEMA_VERSION} and will not write the file back",
                    path.display(),
                    cfg.schema_version
                );
            } else if cfg.schema_version < SUPPORTED_SCHEMA_VERSION {
                cfg.schema_version = SUPPORTED_SCHEMA_VERSION;
            }
            Ok(cfg)
        }
        Err(e) => {
            match backup_malformed(path) {
                Ok(backup) => log::warn!(
                    "config at {} is malformed ({e}); the original bytes were kept at {} — the daemon is running on defaults and the file itself will be overwritten by the next save",
                    path.display(),
                    backup.display()
                ),
                Err(err) => log::error!(
                    "config at {} is malformed ({e}) and the backup failed ({err}) — the daemon is running on defaults and the file itself will be overwritten by the next save",
                    path.display()
                ),
            }
            Ok(AppConfig::default())
        }
    }
}

/// Atomic-ish save: write a temp file, then replace the target.
/// A config from a newer build is never overwritten: this binary does not know its fields.
pub fn save(cfg: &AppConfig) -> io::Result<()> {
    save_at(&config_path(), cfg)
}

fn save_at(path: &Path, cfg: &AppConfig) -> io::Result<()> {
    if cfg.schema_version > SUPPORTED_SCHEMA_VERSION {
        log::error!(
            "refusing to write the config: its schema version ({}) is newer than this build supports ({SUPPORTED_SCHEMA_VERSION}) — an older binary does not overwrite a newer config",
            cfg.schema_version
        );
        return Err(io::Error::other(format!(
            "config schema version {} is newer than the supported version {SUPPORTED_SCHEMA_VERSION}",
            cfg.schema_version
        )));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(cfg).map_err(io::Error::other)?;
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Keep the bytes of a malformed config next to it, so the next save cannot destroy them.
fn backup_malformed(path: &Path) -> io::Result<PathBuf> {
    let backup = backup_path(path)?;
    fs::copy(path, &backup)?;
    Ok(backup)
}

/// A free name for the backup: `config.json.bad`, then `config.json.bad.2`, `config.json.bad.3`,
/// ... and a timestamped name once those are used up. An existing backup is never overwritten.
fn backup_path(path: &Path) -> io::Result<PathBuf> {
    let first = with_suffix(path, ".bad");
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..=MAX_BACKUPS {
        let candidate = with_suffix(path, &format!(".bad.{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stamped = with_suffix(path, &format!(".bad.{millis}"));
    if !stamped.exists() {
        return Ok(stamped);
    }
    for n in 2..100 {
        let candidate = with_suffix(path, &format!(".bad.{millis}.{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::other("no free name for the malformed-config backup"))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    /// A throw-away directory of its own, so a concurrent test run cannot collide with it.
    ///
    /// The guard owns the folder: it goes away when the test ends, failed or not.
    fn temp_dir(name: &str) -> Scratch {
        Scratch::dir(&format!("config-{name}"))
    }

    #[test]
    fn default_config_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.cooldown_seconds, 60);
        assert!(cfg.tracked_apps.is_empty());
        assert_eq!(cfg.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert!(cfg.extra.is_empty());
    }

    #[test]
    fn missing_fields_get_defaults() {
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.cooldown_seconds, 60);
        assert_eq!(cfg.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert_eq!(cfg.catalog_url, None);
        assert_eq!(cfg.sgdb_api_key, None);
        assert!(cfg.console_apps.is_empty());
        assert!(cfg.console_hidden.is_empty());
        assert!(cfg.last_played.is_empty());
        assert!(cfg.packs.is_empty());
        assert!(!cfg.paused);
        assert!(cfg.idle_failsafe_enabled);
        assert_eq!(cfg.idle_failsafe_minutes, 180);
        assert_eq!(cfg.console_open, "pad");
        assert!(cfg.extra.is_empty());
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
    fn console_fields_roundtrip_and_old_configs_still_load() {
        let mut cfg = AppConfig::default();
        cfg.console_apps.push(ConsoleApp {
            name: "Notepad".into(),
            exe: "C:\\Windows\\notepad.exe".into(),
        });
        cfg.console_hidden.push("notepad.exe".into());
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.console_apps[0].exe, "C:\\Windows\\notepad.exe");
        assert_eq!(back.console_hidden, vec!["notepad.exe".to_string()]);

        // a config written before console mode existed must load with console defaults
        let old: AppConfig = serde_json::from_str(r#"{"cooldown_seconds":30}"#).unwrap();
        assert_eq!(old.cooldown_seconds, 30);
        assert!(old.console_apps.is_empty());
        assert!(old.console_hidden.is_empty());
    }

    #[test]
    fn console_open_defaults_to_pad() {
        assert_eq!(AppConfig::default().console_open, "pad");

        // legacy config without the field (and with the retired `console_enabled`) still loads
        let old: AppConfig =
            serde_json::from_str(r#"{"console_enabled": true, "cooldown_seconds": 30}"#).unwrap();
        assert_eq!(old.console_open, "pad");
        assert_eq!(old.cooldown_seconds, 30);
    }

    #[test]
    fn console_open_roundtrips_and_unknown_values_fall_back() {
        for mode in ["always", "pad", "never"] {
            let mut cfg = AppConfig::default();
            cfg.console_open = mode.into();
            let text = serde_json::to_string(&cfg).unwrap();
            let back: AppConfig = serde_json::from_str(&text).unwrap();
            assert_eq!(back.console_open, mode);
        }

        let odd: AppConfig = serde_json::from_str(r#"{"console_open": "sometimes"}"#).unwrap();
        assert_eq!(odd.console_open, "pad");
        let empty: AppConfig = serde_json::from_str(r#"{"console_open": ""}"#).unwrap();
        assert_eq!(empty.console_open, "pad");
    }

    #[test]
    fn a_malformed_config_is_kept_and_the_defaults_are_used() {
        let dir = temp_dir("malformed");
        let path = dir.join("config.json");
        let broken = "{ \"tracked_apps\": [ , }";
        fs::write(&path, broken).unwrap();

        let cfg = load_or_create_at(&path).unwrap();
        assert!(cfg.tracked_apps.is_empty());
        assert_eq!(cfg.schema_version, SUPPORTED_SCHEMA_VERSION);

        // the bytes are safe next to the file, and the file itself is still the user's
        let backup = dir.join("config.json.bad");
        assert!(backup.exists());
        assert_eq!(fs::read_to_string(&backup).unwrap(), broken);
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn a_backup_never_overwrites_an_earlier_one() {
        let dir = temp_dir("backups");
        let path = dir.join("config.json");
        fs::write(&path, "first").unwrap();
        load_or_create_at(&path).unwrap();
        fs::write(&path, "second").unwrap();
        load_or_create_at(&path).unwrap();

        assert_eq!(fs::read_to_string(dir.join("config.json.bad")).unwrap(), "first");
        assert_eq!(
            fs::read_to_string(dir.join("config.json.bad.2")).unwrap(),
            "second"
        );
    }

    #[test]
    fn a_config_from_a_newer_build_is_never_overwritten() {
        let dir = temp_dir("newer");
        let path = dir.join("config.json");
        let newer = format!(r#"{{"schema_version": {}, "cooldown_seconds": 30}}"#, SUPPORTED_SCHEMA_VERSION + 1);
        fs::write(&path, &newer).unwrap();

        // loaded as-is, so the refusal below can trigger
        let cfg = load_or_create_at(&path).unwrap();
        assert_eq!(cfg.schema_version, SUPPORTED_SCHEMA_VERSION + 1);

        let before = fs::read(&path).unwrap();
        assert!(save_at(&path, &cfg).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn loading_stamps_the_current_schema_version() {
        let dir = temp_dir("version");
        let path = dir.join("config.json");
        fs::write(&path, r#"{"schema_version": 0, "cooldown_seconds": 30}"#).unwrap();

        let cfg = load_or_create_at(&path).unwrap();
        assert_eq!(cfg.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert_eq!(cfg.cooldown_seconds, 30);

        // a normal load-and-save round trip keeps the current version
        save_at(&path, &cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.schema_version, SUPPORTED_SCHEMA_VERSION);
    }

    #[test]
    fn unknown_fields_survive_a_load_and_save() {
        let dir = temp_dir("unknown");
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{"cooldown_seconds": 30, "future_setting": 42, "future_flag": true}"#,
        )
        .unwrap();

        let cfg = load_or_create_at(&path).unwrap();
        assert_eq!(cfg.cooldown_seconds, 30);
        assert_eq!(cfg.extra.get("future_setting"), Some(&serde_json::json!(42)));
        assert_eq!(cfg.extra.get("future_flag"), Some(&serde_json::json!(true)));

        save_at(&path, &cfg).unwrap();
        let back: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.get("future_setting"), Some(&serde_json::json!(42)));
        assert_eq!(back.get("future_flag"), Some(&serde_json::json!(true)));
        assert_eq!(back.get("cooldown_seconds"), Some(&serde_json::json!(30)));
    }

    #[test]
    fn a_retired_key_is_dropped_when_the_config_is_read() {
        // "ui_position" was the dashboard's last position. The field is gone, so the file
        // must not keep it any more — but a key of a newer build is still kept.
        let dir = temp_dir("retired");
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{"cooldown_seconds": 30, "ui_position": [0.0, 0.0], "future_setting": 42}"#,
        )
        .unwrap();

        let cfg = load_or_create_at(&path).unwrap();
        assert_eq!(cfg.cooldown_seconds, 30);
        assert!(
            !cfg.extra.contains_key("ui_position"),
            "the retired key must be gone"
        );
        assert_eq!(cfg.extra.get("future_setting"), Some(&serde_json::json!(42)));

        // and the next save writes the file without it
        save_at(&path, &cfg).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("ui_position"), "{text}");
    }

    #[test]
    fn every_field_still_round_trips_with_the_catch_all() {
        let mut cfg = AppConfig {
            cooldown_seconds: 42,
            paused: true,
            idle_failsafe_enabled: false,
            idle_failsafe_minutes: 5,
            catalog_url: Some("https://example.invalid/catalog.json".into()),
            sgdb_api_key: Some("key".into()),
            console_open: "never".into(),
            ..Default::default()
        };
        cfg.console_hidden.push("x.exe".into());
        cfg.last_played.insert("y.exe".into(), 99);
        cfg.tracked_apps.push(TrackedApp {
            name: "Demo".into(),
            exe: "demo.exe".into(),
        });
        cfg.packs.insert("gh".into(), PackUserConfig::default());

        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.cooldown_seconds, 42);
        assert!(back.paused);
        assert!(!back.idle_failsafe_enabled);
        assert_eq!(back.idle_failsafe_minutes, 5);
        assert_eq!(back.catalog_url.as_deref(), Some("https://example.invalid/catalog.json"));
        assert_eq!(back.sgdb_api_key.as_deref(), Some("key"));
        assert_eq!(back.console_open, "never");
        assert_eq!(back.console_hidden, vec!["x.exe".to_string()]);
        assert_eq!(back.last_played.get("y.exe"), Some(&99));
        assert_eq!(back.tracked_apps[0].exe, "demo.exe");
        assert!(back.packs.contains_key("gh"));
        assert!(back.extra.is_empty());
    }
}
