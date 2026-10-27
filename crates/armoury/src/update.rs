//! Feature 9: the daemon's own update.
//!
//! The daemon cannot overwrite the file it is running from, and it must not try: Windows
//! holds the image open. So an update is three steps — check the release feed, download the
//! new executable and verify it, then hand the swap to a small script that waits for this
//! process to exit, copies the new file over the old one, starts it, and puts the old file
//! back if the new build never comes up.
//!
//! **What "verified" means here.** The release publishes `ArmouryDaemon.exe` and an
//! `ArmouryDaemon.exe.sha256` sidecar in the 85-byte format `deploy/setup.ps1` writes. The
//! download is checked against that sidecar, and — when the release also carries
//! `ArmouryDaemon.exe.minisig` — against the minisign keyring this build already trusts for
//! the pack catalogue. A signature that is present but does not verify is a hard failure,
//! never a warning: a release that loses its signature must not update anyone. A release
//! with no signature at all is accepted on the strength of HTTPS to GitHub plus the
//! published hash, which is the same policy the installer has always had; `trusted_by`
//! says which of the two applied so the window can be honest about it.
//!
//! **How the rollback knows.** The new build touches `update-heartbeat` as soon as its pipe
//! is listening. The script deletes that file before it starts the new build and looks for
//! it again after the wait: a build that starts but never answers the pipe leaves the file
//! missing, and the old executable goes back.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use crate::catalog;
use crate::net;
use crate::trust;

/// The release feed. GitHub's API, not the releases page: one JSON body, no HTML.
pub const FEED: &str = "https://api.github.com/repos/PixelAsh6/ArmouryDaemon/releases/latest";
/// The two files a release carries, and the signature it may carry beside them.
pub const EXE_ASSET: &str = "ArmouryDaemon.exe";
pub const SIDECAR_ASSET: &str = "ArmouryDaemon.exe.sha256";
pub const SIG_ASSET: &str = "ArmouryDaemon.exe.minisig";
/// How long the new build is given to answer before the old one is put back.
pub const ROLLBACK_WAIT_MS: u64 = 30_000;

/// What the feed says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The version without its leading `v`: `1.3.1`.
    pub version: String,
    pub exe_url: String,
    pub sidecar_url: String,
    /// `None` for a release that carries no signature: accepted on the hash alone.
    pub sig_url: Option<String>,
}

/// Read the feed JSON. Kept pure so the shape is pinned by a test rather than by a network
/// round trip.
pub fn parse_feed(json: &str) -> Result<Release, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("update: the release feed is not JSON ({e})"))?;
    let version = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(|tag| tag.trim().trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "update: the release feed names no tag".to_string())?;
    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "update: the release carries no assets".to_string())?;
    let url_of = |name: &str| {
        assets.iter().find_map(|asset| {
            let same = asset.get("name").and_then(|v| v.as_str()) == Some(name);
            if same {
                asset
                    .get("browser_download_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
    };
    let exe_url = url_of(EXE_ASSET)
        .ok_or_else(|| format!("update: the release has no {EXE_ASSET} asset"))?;
    let sidecar_url = url_of(SIDECAR_ASSET).ok_or_else(|| {
        format!("update: the release has no {SIDECAR_ASSET} asset, so nothing can be verified")
    })?;
    Ok(Release {
        version,
        exe_url,
        sidecar_url,
        sig_url: url_of(SIG_ASSET),
    })
}

/// Fetch the feed and say whether there is a newer build.
pub fn check(feed: &str) -> Result<Release, String> {
    let body = fetch(feed)?;
    let text = String::from_utf8(body).map_err(|e| format!("update: the feed is not UTF-8 ({e})"))?;
    let release = parse_feed(&text)?;
    if !catalog::is_newer(&release.version, env!("CARGO_PKG_VERSION")) {
        return Err(format!(
            "update: {} is the newest release, this build is {}",
            release.version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    Ok(release)
}

/// Download the release's executable and check it. Returns the staged file, which sits
/// beside the target as `<exe>.new` so the swap script can find it.
///
/// `trusted_by` describes what actually vouched for the bytes, for the window and the log.
pub fn stage(release: &Release, exe: &Path) -> Result<(PathBuf, String), String> {
    let bytes = fetch(&release.exe_url)?;
    let sidecar = fetch(&release.sidecar_url)?;
    let sidecar_text = String::from_utf8(sidecar)
        .map_err(|e| format!("update: the sidecar is not text ({e})"))?;
    let expected = sidecar_hash(&sidecar_text)?;
    let actual = catalog::hex_sha256(&bytes);
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(format!(
            "update: the download's sha256 {actual} does not match the release's {expected}; \
             nothing was written"
        ));
    }
    let trusted_by = match &release.sig_url {
        Some(url) => {
            let sig = fetch(url)?;
            let sig_text = String::from_utf8(sig)
                .map_err(|e| format!("update: the signature is not text ({e})"))?;
            trust::TrustKeyring::load()
                .verify(&bytes, &sig_text)
                .map_err(|e| {
                    format!(
                        "update: the release's signature does not verify ({e}); nothing was written"
                    )
                })?;
            "the release's signature and its sha256".to_string()
        }
        None => "the release's published sha256 (the release carries no signature)".to_string(),
    };
    let staged = staged_path(exe);
    std::fs::write(&staged, &bytes)
        .map_err(|e| format!("update: cannot write {} ({e})", staged.display()))?;
    Ok((staged, trusted_by))
}

/// The hash in a sidecar file: `<hex>  <name>` and nothing else that matters.
pub fn sidecar_hash(text: &str) -> Result<String, String> {
    let first = text
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if first.len() != 64 || !first.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "update: the sidecar does not start with a sha256 ({:?})",
            first.chars().take(12).collect::<String>()
        ));
    }
    Ok(first)
}

/// Where the verified download waits: beside the executable, so the swap is a rename inside
/// one folder and not a copy across volumes.
pub fn staged_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    exe.with_file_name(name)
}

/// The heartbeat the new build writes once its pipe is listening.
pub fn heartbeat_path() -> PathBuf {
    armoury_core::config::config_dir().join("update-heartbeat")
}

/// Tell the waiting update script that this build is up. Called once the pipe is listening;
/// a build that crashes before that leaves no file behind, which is what the rollback reads.
pub fn heartbeat() {
    // The stamp of the old build is removed by the script, not here: this only ever creates
    // the file, so a daemon that was started by hand is not mistaken for an update landing.
    let path = heartbeat_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, catalog::hex_sha256(env!("CARGO_PKG_VERSION").as_bytes()));
}

/// The swap script. A `.cmd`, not PowerShell: no execution policy, and it does nothing a
/// shell can be talked out of.
///
/// `pid` is this daemon, `exe` the file to replace, `staged` the verified download.
pub fn swap_script(pid: u32, exe: &Path, staged: &Path) -> String {
    let exe = exe.display().to_string();
    let staged = staged.display().to_string();
    let backup = format!("{exe}.old");
    let heartbeat = heartbeat_path().display().to_string();
    let wait_ms = ROLLBACK_WAIT_MS / 1000;
    format!(
        "@echo off\r\n\
         rem ArmouryDaemon self-update: wait for the old daemon to go, swap the exe, start the\r\n\
         rem new one, and put the old one back if the new build never answers its pipe.\r\n\
         set \"EXE={exe}\"\r\n\
         set \"NEW={staged}\"\r\n\
         set \"BAK={backup}\"\r\n\
         set \"BEAT={heartbeat}\"\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" 2>nul | find \" {pid} \" >nul || goto swap\r\n\
         timeout /t 1 /nobreak >nul\r\n\
         goto wait\r\n\
         :swap\r\n\
         copy /y \"%EXE%\" \"%BAK%\" >nul || goto giveup\r\n\
         set tries=0\r\n\
         :copy\r\n\
         copy /y \"%NEW%\" \"%EXE%\" >nul && goto start\r\n\
         set /a tries+=1\r\n\
         if %tries% GEQ 30 goto rollback\r\n\
         timeout /t 1 /nobreak >nul\r\n\
         goto copy\r\n\
         :start\r\n\
         del \"%BEAT%\" >nul 2>nul\r\n\
         start \"\" \"%EXE%\"\r\n\
         timeout /t {wait_ms} /nobreak >nul\r\n\
         if exist \"%BEAT%\" goto done\r\n\
         :rollback\r\n\
         copy /y \"%BAK%\" \"%EXE%\" >nul\r\n\
         del \"%BEAT%\" >nul 2>nul\r\n\
         start \"\" \"%EXE%\"\r\n\
         del \"%NEW%\" >nul 2>nul\r\n\
         del \"%~f0\" >nul 2>nul\r\n\
         exit /b 1\r\n\
         :done\r\n\
         del \"%NEW%\" >nul 2>nul\r\n\
         del \"%BAK%\" >nul 2>nul\r\n\
         del \"%~f0\" >nul 2>nul\r\n\
         exit /b 0\r\n\
         :giveup\r\n\
         del \"%NEW%\" >nul 2>nul\r\n\
         del \"%~f0\" >nul 2>nul\r\n\
         exit /b 1\r\n"
    )
}

/// Hand the swap to the script and return at once. The caller exits the daemon right after
/// this: the script's first act is to wait for this pid to disappear.
pub fn apply(exe: &Path, staged: &Path) -> Result<(), String> {
    let script_path = std::env::temp_dir().join(format!("armoury-update-{}.cmd", std::process::id()));
    let script = swap_script(std::process::id(), exe, staged);
    std::fs::write(&script_path, script)
        .map_err(|e| format!("update: cannot write {} ({e})", script_path.display()))?;
    std::process::Command::new("cmd")
        .arg("/c")
        .arg("start")
        .arg("")
        .arg("/min")
        .arg(&script_path)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()
        .map_err(|e| format!("update: cannot start the swap script ({e})"))?;
    Ok(())
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let mut response = net::agent()
        .get(url)
        .header("User-Agent", concat!("ArmouryDaemon/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("update: cannot fetch {url} ({e})"))?;
    response
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("update: cannot download {url} ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"{
        "tag_name": "v1.3.1",
        "assets": [
            {"name": "ArmouryDaemon.exe.sha256",
             "browser_download_url": "https://x/ArmouryDaemon.exe.sha256"},
            {"name": "ArmouryDaemon.exe",
             "browser_download_url": "https://x/ArmouryDaemon.exe"}
        ]
    }"#;

    #[test]
    fn the_feed_names_the_two_assets_an_update_needs() {
        let release = parse_feed(FEED).expect("the feed parses");
        assert_eq!(release.version, "1.3.1");
        assert_eq!(release.exe_url, "https://x/ArmouryDaemon.exe");
        assert_eq!(release.sidecar_url, "https://x/ArmouryDaemon.exe.sha256");
        assert_eq!(release.sig_url, None, "no signature in this release");
    }

    #[test]
    fn a_feed_without_a_sidecar_is_refused() {
        let json = r#"{"tag_name":"v1.3.1","assets":[{"name":"ArmouryDaemon.exe",
            "browser_download_url":"https://x/ArmouryDaemon.exe"}]}"#;
        let err = parse_feed(json).expect_err("no sidecar, no update");
        assert!(err.contains("nothing can be verified"), "{err}");
    }

    #[test]
    fn a_signature_beside_the_exe_is_picked_up() {
        let json = r#"{"tag_name":"v1.3.1","assets":[
            {"name":"ArmouryDaemon.exe","browser_download_url":"https://x/ArmouryDaemon.exe"},
            {"name":"ArmouryDaemon.exe.sha256",
             "browser_download_url":"https://x/ArmouryDaemon.exe.sha256"},
            {"name":"ArmouryDaemon.exe.minisig",
             "browser_download_url":"https://x/ArmouryDaemon.exe.minisig"}
        ]}"#;
        let release = parse_feed(json).expect("the feed parses");
        assert_eq!(
            release.sig_url.as_deref(),
            Some("https://x/ArmouryDaemon.exe.minisig")
        );
    }

    #[test]
    fn the_sidecar_hash_is_read_the_way_setup_writes_it() {
        let text = format!("{}  ArmouryDaemon.exe\r\n", "a".repeat(64));
        assert_eq!(sidecar_hash(&text).unwrap(), "a".repeat(64));
        // uppercase is the same hash
        let upper = format!("{}  ArmouryDaemon.exe\n", "A".repeat(64));
        assert_eq!(sidecar_hash(&upper).unwrap(), "a".repeat(64));
        assert!(sidecar_hash("not a hash").is_err());
        assert!(sidecar_hash("").is_err());
    }

    #[test]
    fn the_script_waits_for_this_pid_and_puts_the_old_exe_back() {
        let exe = Path::new("C:\\Apps\\ArmouryDaemon.exe");
        let staged = Path::new("C:\\Apps\\ArmouryDaemon.exe.new");
        let script = swap_script(4242, exe, staged);
        assert!(script.contains("PID eq 4242"), "the old daemon is waited for");
        assert!(script.contains("copy /y \"%BAK%\" \"%EXE%\""), "the rollback copies the backup back");
        assert!(script.contains("if exist \"%BEAT%\" goto done"), "the heartbeat decides");
        assert!(script.contains("start \"\" \"%EXE%\""), "the new build is started");
        assert!(script.contains(".new"), "the staged file is the one copied in");
    }
}
