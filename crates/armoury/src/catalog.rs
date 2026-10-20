//! The pack catalog (Phase 8): read the manifest, verify a pack, install it.
//!
//! The catalog lives in its own repository (`PixelAsh6/ArmouryDaemon_Packs`): a JSON manifest plus
//! one JSON file per pack. The daemon treats everything from the network as **data**: a pack is
//! downloaded, its sha256 and size are checked against the manifest, and only then does it go
//! through the same validation and install path as a local file. Nothing is ever executed.
//!
//! This module is transport + verification only; the UI and the protocol wiring build on it.

use std::path::{Path, PathBuf};

use armoury_core::pack::version_at_least;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::packs;

/// Where the official catalog manifest lives.
pub const DEFAULT_URL: &str =
    "https://raw.githubusercontent.com/PixelAsh6/ArmouryDaemon_Packs/main/catalog/index.json";

const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    pub schema: u32,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub packs: Vec<CatalogPack>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogPack {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Repository-relative path to the pack file.
    pub file: String,
    /// Lowercase hex SHA-256 of the exact file bytes.
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
    /// The smallest daemon that may install this pack, e.g. "1.1.0". No minimum means every
    /// daemon may run it.
    #[serde(default)]
    pub min_engine: Option<String>,
}

/// What this daemon can do with a catalogue entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineGate {
    /// The pack may be installed here: it states no minimum, or this daemon reaches it.
    Supported,
    /// The pack may not be installed here. `reason` says why, in one line, ready to show.
    Refused {
        /// The version the pack asks for, as the manifest states it.
        required: String,
        /// Why it was refused.
        reason: String,
    },
}

impl CatalogPack {
    /// Whether this daemon may install this pack, and why not when it may not. A malformed
    /// minimum is refused too: an unreadable version is not a version this daemon reaches.
    pub fn engine_gate(&self) -> EngineGate {
        let Some(required) = self.min_engine.as_deref() else {
            return EngineGate::Supported; // no minimum: every daemon may run it
        };
        match version_at_least(packs::DAEMON_VERSION, required) {
            Ok(true) => EngineGate::Supported,
            Ok(false) => EngineGate::Refused {
                required: required.to_string(),
                reason: format!(
                    "needs daemon {required} or newer; this daemon is {}",
                    packs::DAEMON_VERSION
                ),
            },
            Err(e) => EngineGate::Refused {
                required: required.to_string(),
                reason: format!("the catalogue states an unreadable minimum engine version ({e})"),
            },
        }
    }
}

impl Catalog {
    /// The packs this daemon may install. The ones it may not are not hidden: they are in
    /// `refused`, with the reason, so the UI can say why.
    pub fn offered(&self) -> Vec<&CatalogPack> {
        self.packs
            .iter()
            .filter(|pack| pack.engine_gate() == EngineGate::Supported)
            .collect()
    }

    /// The packs this daemon may not install, with the reason the UI can show.
    pub fn refused(&self) -> Vec<(&CatalogPack, String)> {
        self.packs
            .iter()
            .filter_map(|pack| match pack.engine_gate() {
                EngineGate::Supported => None,
                EngineGate::Refused { reason, .. } => Some((pack, reason)),
            })
            .collect()
    }
}

/// Parse and sanity-check a catalog manifest.
pub fn parse(json: &str) -> Result<Catalog, String> {
    let catalog: Catalog = serde_json::from_str(json.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("catalog: invalid JSON ({e})"))?;
    if catalog.schema != SCHEMA {
        return Err(format!(
            "catalog: unsupported schema {} (this build understands {SCHEMA})",
            catalog.schema
        ));
    }
    for pack in &catalog.packs {
        if pack.id.is_empty() || pack.file.is_empty() || pack.sha256.is_empty() {
            return Err("catalog: a pack entry is missing id/file/sha256".to_string());
        }
    }
    Ok(catalog)
}

/// Fetch the manifest over HTTPS.
pub fn fetch(url: &str) -> Result<Catalog, String> {
    let body = fetch_text(url)?;
    parse(&body)
}

/// Download and install one catalog pack. Returns (pack id, updated?) as the local installer does.
pub fn install(install_dirs: &[PathBuf], catalog_url: &str, id: &str) -> Result<(String, bool), String> {
    let catalog = fetch(catalog_url)?;
    // A pack this daemon is too old to run is not offered: say so before the download.
    let refused = catalog.refused();
    if let Some((_, reason)) = refused.iter().find(|(pack, _)| pack.id == id) {
        return Err(format!("catalog: {id} — {reason}"));
    }
    let entry = catalog
        .offered()
        .into_iter()
        .find(|pack| pack.id == id)
        .ok_or_else(|| format!("catalog: no pack called {id:?}"))?;
    let bytes = fetch_bytes(&join_url(catalog_url, &entry.file))?;
    verify(&bytes, &entry.sha256, entry.size)?;
    packs::install_from_bytes(install_dirs, &bytes)
}

/// Download one catalog entry to `dest`, verified against the manifest.
///
/// The settings window uses this: the UI does the network round trip on its own thread and
/// hands the verified file to the daemon through the normal `InstallPack` request, so the
/// daemon keeps owning validation and installation.
pub fn download_to(catalog_url: &str, entry: &CatalogPack, dest: &Path) -> Result<(), String> {
    let bytes = fetch_bytes(&join_url(catalog_url, &entry.file))?;
    verify(&bytes, &entry.sha256, entry.size)?;
    std::fs::write(dest, &bytes)
        .map_err(|e| format!("catalog: cannot write {} ({e})", dest.display()))
}

/// Check a downloaded pack against the manifest.
pub fn verify(bytes: &[u8], expected_sha256: &str, expected_size: Option<u64>) -> Result<(), String> {
    if let Some(size) = expected_size {
        if bytes.len() as u64 != size {
            return Err(mismatch(format!(
                "catalog: pack size {} does not match the manifest ({size})",
                bytes.len()
            )));
        }
    }
    let actual = hex_sha256(bytes);
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        return Err(mismatch(format!(
            "catalog: pack sha256 {actual} does not match the manifest ({expected_sha256})"
        )));
    }
    Ok(())
}

/// A pack that does not match the manifest. The numbers first, then what did not happen, then
/// the two causes worth looking at: a catalog that has not reached the CDN yet, and a hash that
/// was taken from a working-tree copy instead of the published bytes.
fn mismatch(detail: String) -> String {
    format!(
        "{detail}. Nothing was installed. A catalog that was published a moment ago may not have \
         reached the CDN yet — raw.githubusercontent.com caches `main` for about five minutes — \
         and a hash taken from a Windows working-tree copy (CRLF) never matches the published \
         bytes."
    )
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Resolve a repository-relative file against the manifest URL. The catalog lives in
/// `<root>/catalog/index.json` by convention, and its `file` values are relative to `<root>`
/// (matching the repository layout, not the manifest's own folder):
/// `https://host/a/b/catalog/index.json` + `packs/x/pack.json` -> `https://host/a/b/packs/x/pack.json`.
/// A manifest at the host root (`https://host/index.json`) resolves one level up only.
pub fn join_url(manifest_url: &str, file: &str) -> String {
    let trimmed = manifest_url.trim_end_matches('/');
    let manifest_dir = trimmed
        .rsplit_once('/')
        .map(|(dir, _file)| dir)
        .unwrap_or(trimmed);
    let one_up = manifest_dir
        .rsplit_once('/')
        .map(|(parent, _dir)| parent)
        .unwrap_or(manifest_dir);
    let root = if one_up.contains("://") { one_up } else { manifest_dir };
    format!("{root}/{}", file.trim_start_matches('/'))
}

fn fetch_text(url: &str) -> Result<String, String> {
    let response = get(url)?;
    response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("catalog: cannot read {url} ({e})"))
}

fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let mut response = get(url)?;
    response
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("catalog: cannot download {url} ({e})"))
}

fn get(url: &str) -> Result<ureq::http::Response<ureq::Body>, String> {
    ureq::get(url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .call()
        .map_err(|e| format!("catalog: cannot fetch {url} ({e})"))
}

/// Where the catalog URL comes from: the config value when set, else the official one.
pub fn url_or_default(configured: Option<&str>) -> String {
    configured
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// True when `candidate` is a newer version than `current`: dotted numbers compared
/// numerically ("1.10.0" is newer than "1.9.9"), any suffix after `-`/`+` ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim_start_matches(['v', ' '])
            .split(['.', '-', '+'])
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    parts(candidate) > parts(current)
}

/// List installed pack ids (for the "update available" comparison in the UI).
pub fn installed_ids(search_dirs: &[PathBuf]) -> Vec<String> {
    packs::load(search_dirs)
        .into_iter()
        .map(|installed| installed.pack.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
        "schema": 1,
        "updated": "2026-10-18",
        "packs": [
            {
                "id": "g-helper",
                "name": "G-Helper",
                "version": "1.3.1",
                "description": "Switches modes.",
                "file": "packs/g-helper/pack.json",
                "sha256": "b6fdaa3b1bbdd45e5b904322fdb2ae91aca5926a90563f29997cb82a140d07e3",
                "size": 1235,
                "updated": "2026-10-18"
            }
        ]
    }"#;

    #[test]
    fn the_manifest_parses_and_rejects_other_schemas() {
        let catalog = parse(MANIFEST).expect("the shipped manifest shape must parse");
        assert_eq!(catalog.packs.len(), 1);
        assert_eq!(catalog.packs[0].id, "g-helper");
        assert!(parse(r#"{"schema": 99, "packs": []}"#).is_err());
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"schema": 1, "packs": [{"id": "", "file": "x", "sha256": "y"}]}"#).is_err());
    }

    #[test]
    fn files_resolve_against_the_repository_root() {
        // the shipped layout: <root>/catalog/index.json names <root>/packs/...
        assert_eq!(
            join_url(
                "https://raw.githubusercontent.com/PixelAsh6/ArmouryDaemon_Packs/main/catalog/index.json",
                "packs/g-helper/pack.json"
            ),
            "https://raw.githubusercontent.com/PixelAsh6/ArmouryDaemon_Packs/main/packs/g-helper/pack.json"
        );
        // a manifest at the host root resolves one level up only
        assert_eq!(
            join_url("https://host/index.json", "/packs/x/pack.json"),
            "https://host/packs/x/pack.json"
        );
        assert_eq!(
            join_url("https://host/catalog/index.json", "packs/x/pack.json"),
            "https://host/packs/x/pack.json"
        );
    }

    #[test]
    fn sha256_and_size_are_verified() {
        let bytes = b"hello";
        // sha256("hello")
        let hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify(bytes, hash, Some(5)).is_ok());
        assert!(verify(bytes, hash, Some(6)).is_err(), "size mismatch must refuse");
        assert!(verify(bytes, "00", Some(5)).is_err(), "hash mismatch must refuse");
        assert!(verify(bytes, &hash.to_ascii_uppercase(), Some(5)).is_ok());
    }

    #[test]
    fn a_mismatch_says_what_did_not_happen_and_why() {
        let bytes = b"hello";
        let hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let size = verify(bytes, hash, Some(6)).expect_err("the sizes differ");
        assert!(size.contains('6') && size.contains("5"), "the numbers stay: {size}");
        let sha = verify(bytes, "00", None).expect_err("the hashes differ");
        assert!(sha.contains(hash), "the numbers stay: {sha}");
        for message in [size, sha] {
            assert!(message.contains("Nothing was installed"), "{message}");
            assert!(message.contains("five minutes"), "{message}");
            assert!(message.contains("CRLF"), "{message}");
        }
    }

    #[test]
    fn a_configured_url_wins_over_the_default() {
        assert_eq!(url_or_default(None), DEFAULT_URL);
        assert_eq!(url_or_default(Some("   ")), DEFAULT_URL);
        assert_eq!(url_or_default(Some("https://example.com/c.json")), "https://example.com/c.json");
    }

    #[test]
    fn version_comparison_finds_updates() {
        assert!(is_newer("1.4.0", "1.3.1"));
        assert!(is_newer("1.10.0", "1.9.9"), "numeric, not a string compare");
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.3.1", "1.3.1"));
        assert!(!is_newer("1.2.0", "1.3.1"));
        assert!(!is_newer("garbage", "1.0.0"));
    }

    #[test]
    fn install_bytes_validates_before_writing() {
        let dir = std::env::temp_dir().join(format!("armoury-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = br#"{"spec": 1}"#;
        assert!(packs::install_from_bytes(&[dir.clone()], bad).is_err());
        assert!(!dir.join("packs").exists(), "nothing may be written for an invalid pack");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest with three entries: one this daemon is too old for, one without a
    /// minimum and one with a minimum it reaches.
    const MANIFEST_WITH_ENGINES: &str = r#"{
        "schema": 1,
        "updated": "2026-10-20",
        "packs": [
            {
                "id": "too-new",
                "name": "Too new",
                "version": "2.0.0",
                "file": "packs/too-new/pack.json",
                "sha256": "00",
                "min_engine": "99.0.0"
            },
            {
                "id": "any",
                "name": "Any",
                "version": "1.0.0",
                "file": "packs/any/pack.json",
                "sha256": "00"
            },
            {
                "id": "old-enough",
                "name": "Old enough",
                "version": "1.0.0",
                "file": "packs/old-enough/pack.json",
                "sha256": "00",
                "min_engine": "0.0.1"
            }
        ]
    }"#;

    #[test]
    fn the_manifest_reads_the_minimum_engine_of_a_pack() {
        let catalog = parse(MANIFEST_WITH_ENGINES).expect("the manifest must parse");
        assert_eq!(catalog.packs[0].min_engine.as_deref(), Some("99.0.0"));
        assert!(catalog.packs[1].min_engine.is_none(), "no minimum means none");
        assert_eq!(catalog.packs[2].min_engine.as_deref(), Some("0.0.1"));
    }

    #[test]
    fn the_client_does_not_offer_a_pack_this_daemon_is_too_old_for() {
        let catalog = parse(MANIFEST_WITH_ENGINES).expect("the manifest must parse");
        let offered: Vec<&str> = catalog.offered().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(offered, vec!["any", "old-enough"], "the rest is still offered");

        let refused = catalog.refused();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0.id, "too-new");
        let reason = &refused[0].1;
        assert!(reason.contains("99.0.0"), "the reason names the version: {reason}");
        assert!(
            reason.contains(packs::DAEMON_VERSION),
            "the reason names this daemon: {reason}"
        );

        // the entry itself carries the same answer
        assert_eq!(
            catalog.packs[0].engine_gate(),
            EngineGate::Refused {
                required: "99.0.0".to_string(),
                reason: reason.clone(),
            }
        );
        assert_eq!(catalog.packs[1].engine_gate(), EngineGate::Supported);
    }

    #[test]
    fn a_manifest_with_an_unreadable_minimum_is_refused_with_a_reason() {
        let json = r#"{
            "schema": 1,
            "packs": [ { "id": "broken", "file": "packs/broken/pack.json", "sha256": "00",
                         "min_engine": "not-a-version" } ]
        }"#;
        let catalog = parse(json).expect("one bad entry does not kill the manifest");
        assert!(catalog.offered().is_empty(), "an unreadable minimum is never offered");
        let refused = catalog.refused();
        assert_eq!(refused.len(), 1);
        assert!(
            refused[0].1.contains("not-a-version"),
            "the reason names what is wrong: {}",
            refused[0].1
        );
    }
}
