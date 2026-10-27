//! Feature 10: the diagnostics bundle — one zip a bug report can carry.
//!
//! Everything that has been needed to explain a report so far: the last lines of every log,
//! the config, the status snapshot, the display layout (the window-jump reports needed it)
//! and the pack inventory. **The SteamGridDB key is blanked** before anything is written: a
//! bundle is made to be handed to somebody else, so nothing secret goes into it.
//!
//! The zip is written by the same `zip` crate that extracts helper archives, so no new
//! dependency arrives with this feature.

use std::io::Write;
use std::path::{Path, PathBuf};

use armoury_core::config;
use armoury_core::status::StatusSnapshot;
use zip::write::SimpleFileOptions;

/// How many lines of each log are carried. Enough for a session's worth of context, small
/// enough that the bundle stays mailable.
const LOG_LINES: usize = 600;

/// Write the bundle and return its path. `layout` is free text from the caller (the UI knows
/// the DPI and the window rectangles; the daemon does not).
pub fn write(
    dir: &Path,
    status: Option<&StatusSnapshot>,
    layout: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("diagnostics: cannot create {} ({e})", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path = dir.join(format!("armoury-diagnostics-{stamp}.zip"));
    let file = std::fs::File::create(&path)
        .map_err(|e| format!("diagnostics: cannot create {} ({e})", path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut put = |name: &str, body: &[u8]| -> Result<(), String> {
        zip.start_file(name, options)
            .map_err(|e| format!("diagnostics: cannot add {name} ({e})"))?;
        zip.write_all(body)
            .map_err(|e| format!("diagnostics: cannot write {name} ({e})"))
    };

    put("status.json", &redacted_status(status))?;
    put("config.json", &redacted_config())?;
    put("display.txt", layout.as_bytes())?;
    put("logs.txt", &logs())?;
    put("README.txt", README.as_bytes())?;
    zip.finish()
        .map_err(|e| format!("diagnostics: cannot finish {} ({e})", path.display()))?;
    Ok(path)
}

/// What the bundle is and what was left out of it. A person who receives one should not have
/// to guess.
const README: &str = "ArmouryDaemon diagnostics\n\
    \n\
    status.json     the daemon's status snapshot at the time of the copy\n\
    config.json     the config, with the SteamGridDB API key blanked\n\
    logs.txt        the last lines of the daemon, UI and console logs\n\
    display.txt     the display layout: DPI, monitors, window rectangles\n\
    \n\
    No API key, token or password is in this archive.\n";

/// The status snapshot as JSON, with the SteamGridDB key blanked.
fn redacted_status(status: Option<&StatusSnapshot>) -> Vec<u8> {
    match status {
        Some(status) => {
            let mut value = serde_json::to_value(status).unwrap_or(serde_json::Value::Null);
            redact(&mut value);
            serde_json::to_vec_pretty(&value).unwrap_or_default()
        }
        None => b"{\"connected\": false}".to_vec(),
    }
}

/// The config as JSON, with the key blanked. Read through the config loader so a corrupt file
/// still produces something readable in the bundle instead of an empty entry.
fn redacted_config() -> Vec<u8> {
    let text = std::fs::read_to_string(config::config_dir().join("config.json")).ok();
    let mut value: serde_json::Value = text
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(serde_json::Value::Null);
    redact(&mut value);
    serde_json::to_vec_pretty(&value).unwrap_or_default()
}

/// Blank every field that could carry a secret. By name, so a field added later is a
/// one-line change here rather than a hunt through the archive.
fn redact(value: &mut serde_json::Value) {
    const SECRET_KEYS: [&str; 3] = ["sgdb_api_key", "api_key", "token"];
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if SECRET_KEYS.contains(&key.as_str()) {
                    *inner = serde_json::Value::String(String::new());
                } else {
                    redact(inner);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// The tail of every log the daemon family writes, oldest file first, each one labelled.
fn logs() -> Vec<u8> {
    let dir = config::config_dir().join("logs");
    let mut out: Vec<u8> = Vec::new();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".log"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(LOG_LINES);
        out.extend_from_slice(format!("=== {name} ===\n").as_bytes());
        for line in &lines[start..] {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn the_key_is_blanked_before_it_reaches_the_archive() {
        let mut value = serde_json::json!({
            "sgdb_api_key": "0123456789abcdef",
            "packs": {"g-helper": {"enabled": true}},
            "nested": [{"token": "secret"}],
        });
        redact(&mut value);
        let text = value.to_string();
        assert!(!text.contains("0123456789abcdef"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("g-helper"), "everything else is kept: {text}");
    }

    #[test]
    fn the_bundle_holds_the_four_files_and_no_key() {
        let dir = std::env::temp_dir().join(format!("armoury-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = write(&dir, None, "monitor 0: 2560x1440 @ 1.5\n").expect("the bundle is written");
        let file = std::fs::File::open(&path).expect("the bundle exists");
        let mut archive = zip::ZipArchive::new(file).expect("it is a zip");
        let mut seen: Vec<String> = Vec::new();
        for i in 0..archive.len() {
            seen.push(archive.by_index(i).unwrap().name().to_string());
        }
        seen.sort();
        assert_eq!(
            seen,
            vec!["README.txt", "config.json", "display.txt", "logs.txt", "status.json"]
        );
        let mut layout = String::new();
        archive
            .by_name("display.txt")
            .unwrap()
            .read_to_string(&mut layout)
            .unwrap();
        assert!(layout.contains("2560x1440"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
