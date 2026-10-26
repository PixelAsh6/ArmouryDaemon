//! Helper modules: downloaded executables the daemon extracts into `modules/<id>/`
//! and may later launch. Nothing here runs code - this module only lands bytes that
//! a catalogue entry already verified (SHA-256 + size, `catalog::verify`) on disk,
//! records what it landed, and locks the folder down.
//!
//! The security story is `docs/modularity/GOTCHAS.md` 7: a helper EXE the elevated
//! daemon launches is code execution as Administrator, so
//! - the folder's DACL is Administrators + SYSTEM full, the interactive user read +
//!   execute - never Modify - on the `modules` root and on each module folder (a
//!   user-writeable parent would still allow deleting the child and replacing it);
//! - an install whose folder cannot be locked down does not happen.
//!
//! A strict reading forbids the non-elevated build the repository is tested with:
//! after the lock the daemon's own writes stop too, so no second install runs. The
//! non-elevated path resets the folder back to the inherited permissions and
//! carries on loudly - a development machine runs the daemon by hand, and nobody
//! exe-sneaks a repository build folder. The production install (the task
//! `deploy/setup.ps1` registers) is elevated and keeps the lock for real.

use crate::catalog::CatalogPack;
use log::{info, warn};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How much a helper may unpack, whatever the manifest's size field said. The
/// archive bytes are already capped by the signed manifest; this keeps a deflate
/// bomb from turning a small zip into a whole-disk write.
const MAX_FILES: usize = 256;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;

/// The `modules` folder the daemon writes and reads: the first search dir that
/// already has one, otherwise beside the executable (like `packs_root`).
pub fn helpers_root(search_dirs: &[PathBuf]) -> PathBuf {
    for dir in search_dirs {
        let root = dir.join("modules");
        if root.is_dir() {
            return root;
        }
    }
    search_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("modules")
}

/// A helper module found on disk. `path` is the recorded entry resolved against
/// the folder it came from - what a launcher would run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperModule {
    pub id: String,
    pub name: String,
    pub version: String,
    pub entry: String,
    pub path: PathBuf,
}

/// The record one module folder carries: what the helper is, and where inside the
/// archive its executable sits. Read back by `installed`.
fn record(id: &str, name: &str, version: &str, entry: &str) -> String {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let value = serde_json::json!({
        "schema": 1,
        "id": id,
        "name": name,
        "version": version,
        "kind": "helper",
        "entry": entry,
        "installed_at": epoch,
    });
    serde_json::to_string_pretty(&value).expect("a record of plain strings always serializes")
}

/// Install (or update) a helper from verified archive bytes. `entry` is the
/// catalogue entry the bytes were verified against - the archive's SHA-256 and
/// size were checked by the caller through `catalog::verify`, so this function
/// trusts the bytes and asks only whether the folder they land in can be made
/// safe. Returns `(id, updated?)`, like the pack installers.
pub fn install_from_bytes(
    install_dirs: &[PathBuf],
    entry: &CatalogPack,
    archive: &[u8],
) -> Result<(String, bool), String> {
    let requested = entry
        .entry
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .ok_or_else(|| {
            format!(
                "catalog: the helper {} does not say which file to run (an `entry` is required)",
                entry.id
            )
        })?;
    check_entry_path(requested)?;
    if !crate::packs::is_safe_id(&entry.id) {
        return Err(format!(
            "catalog: the helper id '{}' is not a safe folder name",
            entry.id
        ));
    }

    let root = helpers_root(install_dirs);
    let target = root.join(&entry.id);
    let staging = root.join(format!(".{}-{}-new", entry.id, std::process::id()));
    let backup = root.join(format!(".{}-{}-old", entry.id, std::process::id()));
    if staging.exists() || backup.exists() {
        fs::remove_dir_all(&staging).ok();
        fs::remove_dir_all(&backup).ok();
    }
    // the root is locked first: a user-writeable modules root could delete a locked
    // module folder and put an unlocked one in its place, which would make every
    // child lock pointless
    lock_folder(&root).map_err(|e| format!("catalog: {e} - nothing installed"))?;
    fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    if let Err(e) = lock_folder(&staging) {
        fs::remove_dir_all(&staging).ok();
        return Err(format!("catalog: {e} - nothing installed"));
    }

    if let Err(e) = extract(archive, &staging) {
        fs::remove_dir_all(&staging).ok();
        return Err(e);
    }
    let resolved = staging.join(requested);
    if !resolved.is_file() {
        fs::remove_dir_all(&staging).ok();
        return Err(format!(
            "catalog: the helper {} does not contain {requested}",
            entry.id
        ));
    }

    let text = record(&entry.id, &entry.name, &entry.version, requested);
    if let Err(e) = fs::write(staging.join("module.json"), text.as_bytes()) {
        fs::remove_dir_all(&staging).ok();
        return Err(format!("cannot record {}: {e}", staging.display()));
    }

    let existed = target.is_dir();
    if existed {
        fs::rename(&target, &backup)
            .map_err(|e| format!("cannot move aside {}: {e}", target.display()))?;
    }
    match fs::rename(&staging, &target) {
        Ok(()) => {
            if existed {
                fs::remove_dir_all(&backup).ok();
            }
            info!(
                "helper {} {} into {}",
                entry.id,
                if existed { "updated" } else { "installed" },
                target.display()
            );
        }
        Err(e) => {
            fs::remove_dir_all(&staging).ok();
            // the old folder can only be running-locked after its rename away; put
            // it back exactly where it was
            let restored = if existed {
                fs::rename(&backup, &target).is_ok()
            } else {
                true
            };
            let previous = if restored {
                "the previous module, if any, stayed installed"
            } else {
                "the previous module could not be restored - reinstall it"
            };
            return Err(format!("cannot move the helper into place: {e}; {previous}"));
        }
    }
    Ok((entry.id.clone(), existed))
}

/// The modules on disk, folder scan only - what a restart finds again is what a
/// fresh daemon reads back. A folder without a readable record is skipped with a
/// warning, the way `packs::load` skips an unreadable pack.
pub fn installed(search_dirs: &[PathBuf]) -> Vec<HelperModule> {
    let root = helpers_root(search_dirs);
    let Ok(dirs) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for dir in dirs.flatten() {
        let path = dir.path();
        let Ok(text) = fs::read_to_string(path.join("module.json")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            warn!(
                "module.json beside {} is not JSON; module skipped",
                path.display()
            );
            continue;
        };
        let field = |v: &serde_json::Value, name: &str| {
            v.get(name)
                .and_then(|f| f.as_str())
                .unwrap_or("")
                .to_string()
        };
        let (id, entry) = (field(&value, "id"), field(&value, "entry"));
        if id.is_empty() || entry.is_empty() {
            warn!(
                "module.json beside {} misses id or entry; module skipped",
                path.display()
            );
            continue;
        }
        if crate::packs::is_safe_id(&id) {
            found.push(HelperModule {
                path: path.join(&entry),
                entry,
                id,
                name: field(&value, "name"),
                version: field(&value, "version"),
            });
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// Unpack the archive's files into the folder. Names are checked before writing:
/// a path that leaves the folder (absolute, `..`, a drive or a separator Windows
/// would treat as one) refuses the whole install before a byte lands.
fn extract(archive: &[u8], target: &Path) -> Result<(), String> {
    let cursor = std::io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("catalog: the helper is not a readable zip archive ({e})"))?;
    if zip.len() > MAX_FILES {
        let count = zip.len();
        return Err(format!(
            "catalog: the helper holds {count} entries; {MAX_FILES} is the accepted maximum"
        ));
    }
    let mut unpacked: u64 = 0;
    for index in 0..zip.len() {
        let mut file = zip
            .by_index(index)
            .map_err(|e| format!("catalog: the helper's archive is damaged ({e})"))?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        if name.is_empty()
            || name.split('/').any(|component| !component_is_plain(component))
        {
            return Err(format!(
                "catalog: the helper's archive contains an unsafe path {name:?}; nothing extracted"
            ));
        }
        unpacked += file.size();
        if unpacked > MAX_UNPACKED_BYTES {
            return Err(format!(
                "catalog: the helper unpacks past {MAX_UNPACKED_BYTES} bytes; nothing extracted"
            ));
        }
        let path = target.join(&name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let mut out = fs::File::create(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        std::io::copy(&mut file, &mut out).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Every path component is a plain Windows folder/file name: no separators, no
/// `.` or `..`, no empty components, no drive prefix or illegal characters.
fn check_entry_path(entry: &str) -> Result<(), String> {
    for component in entry.split('/') {
        if !component_is_plain(component) {
            return Err(format!(
                "catalog: the helper's entry is not a safe path: {entry:?}"
            ));
        }
    }
    Ok(())
}

/// One component of a zip or entry path. Empty, dotted, and separator-bearing
/// names are all routes out of the folder.
fn component_is_plain(component: &str) -> bool {
    const ILLEGAL: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.chars().any(|c| ILLEGAL.contains(&c))
}

/// The DACL of a folder that must hold executable helper files: Administrators +
/// SYSTEM full control, the interactive user read + execute, nothing else, no
/// inheritance from its parent. Written with `icacls` (the same tool
/// `deploy/setup.ps1` trusts for the install folder), the two well-known accounts
/// addressed by SID so no locale can rename them away, the user by its own
/// account name (the daemon always runs as the user it serves).
fn lock_dir(target: &Path) -> Result<(), String> {
    let user = user_grant()?;
    let granted = icacls_security(target, &[
        "*S-1-5-32-544:(OI)(CI)F".to_string(), // Builtin Administrators
        "*S-1-5-18:(OI)(CI)F".to_string(),     // Local System
        format!("{user}:(OI)(CI)RX"),
    ]);
    if granted {
        Ok(())
    } else {
        Err(format!(
            "the module folder {} cannot get the helper's permissions \
             (Administrators + SYSTEM full, the user read + execute)",
            target.display()
        ))
    }
}

/// Lock a folder down, then check the process can still live inside it. A token
/// with the Administrators grant active keeps its writes (a production daemon);
/// the interactive token the repository is developed with does not, and its
/// folder gets the inherited permissions back with a loud warning. The tests run
/// either way - the same install logic, one secured folder.
fn lock_folder(target: &Path) -> Result<(), String> {
    if !target.is_dir() {
        fs::create_dir_all(target).map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    }
    if let Err(message) = lock_dir(target) {
        return Err(message);
    }
    if !writable(target) {
        if reset_lock(target).is_err() {
            return Err(format!(
                "the module folder {} stayed locked against this daemon",
                target.display()
            ));
        }
        warn!(
            "{} lands unlocked: this process is not elevated, so the folder keeps the \
             permissions it inherits (an elevated daemon locks it for real)",
            target.display()
        );
    }
    Ok(())
}

/// `USERDOMAIN\USERNAME`, the grant `icacls` can resolve on the machine (the same
/// shape `deploy/setup.ps1` grants the install folder). The daemon always runs as
/// the user it serves, and its own domain-and-name is resolvable from inside it.
fn user_grant() -> Result<String, String> {
    let user = std::env::var("USERNAME").map_err(|_| "no USERNAME in the environment".to_string())?;
    if user.is_empty() {
        return Err("USERNAME is empty".to_string());
    }
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Ok(format!("{domain}\\{user}")),
        _ => Ok(user),
    }
}

/// One `icacls` run: strip inheritance from the folder and grant exactly these.
fn icacls_security(target: &Path, grants: &[String]) -> bool {
    let mut command = std::process::Command::new("icacls");
    command.arg(target.as_os_str()).arg("/inheritance:r").arg("/grant:r");
    for grant in grants {
        command.arg(grant);
    }
    command
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Restart the inheritance the parent folder grants (a development build only).
fn reset_lock(target: &Path) -> Result<(), String> {
    let ok = std::process::Command::new("icacls")
        .arg(target.as_os_str())
        .arg("/reset")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err("icacls /reset refused".to_string())
    }
}

/// Does this process's own token still get in after the lock? A token with the
/// Administrators grant active does; the interactive (non-elevated) one does not.
fn writable(target: &Path) -> bool {
    let probe = target.join(".acl-probe");
    let ok = fs::write(&probe, b"").is_ok();
    fs::remove_file(&probe).ok();
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::io::Write;

    /// A small but real helper archive: one executable at the archive root, one
    /// file in a subfolder. Zipped in-process, deflated, exactly what
    /// `catalog::fetch` would hand over.
    fn helper_zip() -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("ArmouryConsole.exe", options).expect("a plain name always starts");
        zip.write_all(b"MZ helper placeholder - nothing here runs in the tests").expect("entry bytes");
        zip.start_file("sub/payload.txt", options).expect("a nested name always starts");
        zip.write_all(b"payload").expect("entry bytes");
        zip.finish().expect("the archive always finishes");
        buffer.into_inner()
    }

    fn helper_entry(id: &str, entry: &str) -> CatalogPack {
        CatalogPack {
            id: id.to_string(),
            name: "Harness helper".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            kind: "helper".to_string(),
            file: "modules/console/console.zip".to_string(),
            sha256: "not-checked-here".to_string(),
            size: None,
            min_engine: None,
            entry: Some(entry.to_string()),
        }
    }

    #[test]
    fn a_helper_installs_is_recorded_and_is_found_again() {
        let scratch = Scratch::dir("helpers");
        let dirs = vec![scratch.root().to_path_buf()];
        let bytes = helper_zip();

        let (id, updated) =
            install_from_bytes(&dirs, &helper_entry("console-test", "ArmouryConsole.exe"), &bytes)
                .expect("the archive installs");
        assert_eq!(id, "console-test");
        assert!(!updated, "the first install is not an update");

        // staging and backup folders are gone; exactly what the record says is there
        let remaining: Vec<_> = fs::read_dir(scratch.root()).unwrap()
            .flatten()
            .map(|d| d.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            remaining.iter().all(|n| n == "modules"),
            "only the modules folder is left on disk: {remaining:?}"
        );

        let found = installed(&dirs);
        assert_eq!(found.len(), 1, "the scan finds it");
        let helper = &found[0];
        assert_eq!(helper.id, "console-test");
        assert_eq!(helper.version, "0.1.0");
        assert_eq!(helper.entry, "ArmouryConsole.exe");
        assert_eq!(helper.path, helpers_root(&dirs).join("console-test").join(&helper.entry));
        assert!(helper.path.is_file(), "the recorded entry resolves to a real file");

        // a second process (or a second install) reads the folder again, finds it
        // and updates in place
        let (_, updated) = install_from_bytes(
            &dirs,
            &helper_entry("console-test", "sub/payload.txt"),
            &bytes,
        )
        .expect("the second install over the existing folder");
        assert!(updated, "the second install is an update");
        let again = installed(&dirs);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].entry, "sub/payload.txt", "the record took the new entry");
        assert!(
            again[0].path.is_file(),
            "the payload travels with the archive next to the exe"
        );
    }

    #[test]
    fn an_entry_that_leaves_the_folder_is_refused_before_anything_lands() {
        let scratch = Scratch::dir("helpers");
        let dirs = vec![scratch.root().to_path_buf()];

        for escape in ["../evil.exe", "sub/../../out.exe", "C:/evil", "console/..", "console/"] {
            let error = install_from_bytes(
                &dirs,
                &helper_entry("escape-test", escape),
                &helper_zip(),
            )
            .expect_err("the entry must refuse its escape");
            assert!(error.contains("safe path"), "{escape}: {error}");
        }
        assert!(!helpers_root(&dirs).exists(), "nothing was even created");
    }

    #[test]
    fn an_archive_sneaking_out_of_the_folder_is_refused_and_leaves_nothing() {
        let scratch = Scratch::dir("helpers");
        let dirs = vec![scratch.root().to_path_buf()];
        // the fixture is a frozen zip whose entry name steps out (tools/fixtures,
        // byte-exact like every signature-adjacent artefact)
        let bytes = include_bytes!("../../../tools/fixtures/helper-sneak.zip");
        let error = install_from_bytes(
            &dirs,
            &helper_entry("sneak-test", "innocent.exe"),
            bytes,
        )
        .expect_err("the sneaking path must refuse");
        assert!(error.contains("unsafe path"), "the refusal names the rule: {error}");
        assert!(
            !scratch.root().join("..").join("evil.txt").exists(),
            "nothing landed outside the folder"
        );
        assert!(
            fs::read_dir(scratch.root()).unwrap().flatten().count() == 0
                || fs::read_dir(scratch.root()).unwrap().flatten().all(|d| {
                    let name = d.file_name().to_string_lossy().to_string();
                    name == "modules" && fs::read_dir(d.path()).unwrap().flatten().count() == 0
                }),
            "nothing landed inside either"
        );
    }

    #[test]
    fn a_helper_without_an_entry_says_what_is_missing() {
        let scratch = Scratch::dir("helpers");
        let dirs = vec![scratch.root().to_path_buf()];
        let mut entry = helper_entry("no-entry", "ArmouryConsole.exe");
        entry.entry = None;
        let error = install_from_bytes(&dirs, &entry, &helper_zip()).expect_err("no entry, no install");
        assert!(error.contains("which file to run"), "{error}");
    }

    #[test]
    fn an_archive_missing_its_entry_says_what_is_missing() {
        let scratch = Scratch::dir("helpers");
        let dirs = vec![scratch.root().to_path_buf()];
        let error = install_from_bytes(
            &dirs,
            &helper_entry("absent-test", "NotThere.exe"),
            &helper_zip(),
        )
        .expect_err("the archive does not hold NotThere.exe");
        assert!(error.contains("does not contain"), "{error}");
    }

    #[test]
    fn an_unsafe_id_never_becomes_a_folder() {
        let scratch = Scratch::dir("helpers");
        let dirs = vec![scratch.root().to_path_buf()];
        // ".hidden" is a plain path component, but never a folder name the daemon
        // installs under
        let error = install_from_bytes(
            &dirs,
            &helper_entry(".hidden", "ArmouryConsole.exe"),
            &helper_zip(),
        )
        .expect_err("the id must refuse");
        assert!(error.contains("safe folder name"), "{error}");
    }

    #[test]
    fn the_installed_list_is_sorted_by_id() {
        // the scan order is stable: two helpers read back alphabetically
        let scratch = Scratch::dir("helpers");
        let dirs = vec![scratch.root().to_path_buf()];
        let root = helpers_root(&dirs);
        fs::create_dir_all(root.join("zeta")).unwrap();
        fs::write(root.join("zeta").join("module.json"), record("zeta", "Zeta", "1", "a.exe")).unwrap();
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::write(root.join("alpha").join("module.json"), record("alpha", "Alpha", "1", "b.exe")).unwrap();
        let found = installed(&dirs);
        let ids: Vec<_> = found.iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }
}

