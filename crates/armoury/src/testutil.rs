//! The scratch folders the tests write into, and the guard that takes them away again.
//!
//! The tests create folders under `%TEMP%` and clean them up at the end of the body — but a
//! failing assertion skips the rest of the body, and the folder stays behind for good
//! (2026-10-21: 250 empty `armoury-packs-*` folders in the temp directory). Cleaning up in
//! `Drop` happens whatever the test does.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A counter so two scratch folders of the same label never share a name.
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A scratch folder under the temp directory that removes itself when it goes out of scope.
///
/// The folder is always a direct child of the temp directory, named
/// `armoury-{label}-{pid}-{n}`. Two runs never share a folder, and the cleanup can only ever
/// reach that one folder.
pub(crate) struct Scratch {
    root: PathBuf,
    temp: PathBuf,
}

impl Scratch {
    /// An empty folder under the temp directory, named `armoury-{label}-{pid}-{n}`.
    ///
    /// A folder left by an earlier run is emptied first: the tests write into it and must
    /// not find anything of somebody else's.
    pub(crate) fn dir(label: &str) -> Scratch {
        assert!(
            !label.is_empty() && !label.contains('\\') && !label.contains('/'),
            "a scratch folder is a single name: {label}"
        );
        let temp = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = temp.join(format!("armoury-{label}-{}-{n}", std::process::id()));
        // The folder is a direct child of the temp directory and nothing else: the cleanup
        // in `Drop` can never reach outside it — not the temp directory, not a sibling.
        assert_eq!(
            root.parent(),
            Some(temp.as_path()),
            "a scratch folder lives in the temp directory: {}",
            root.display()
        );
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Scratch { root, temp }
    }

    /// A file in the temp directory itself, named exactly `{name}`.
    ///
    /// The catalogue download leaves its file in the temp folder itself and not in a
    /// subfolder, so the tests that exercise it need a file of their own there.
    pub(crate) fn file(name: &str) -> Scratch {
        assert!(
            !name.is_empty() && !name.contains('\\') && !name.contains('/'),
            "a scratch file is a single name: {name}"
        );
        let temp = std::env::temp_dir();
        let root = temp.join(name);
        // The file is a direct child of the temp directory and nothing else: the cleanup
        // in `Drop` can never reach outside it — not the temp directory, not a sibling.
        assert_eq!(
            root.parent(),
            Some(temp.as_path()),
            "a scratch file lives in the temp directory: {}",
            root.display()
        );
        Scratch { root, temp }
    }

    /// The folder (or the file) itself.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Belt and braces: the path is only removed while it is still the direct child of
        // the temp directory it was made in.
        if self.root.parent() != Some(self.temp.as_path()) {
            return;
        }
        if self.root.is_dir() {
            let _ = std::fs::remove_dir_all(&self.root);
        } else {
            let _ = std::fs::remove_file(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_folder_lives_in_the_temp_directory_and_nowhere_else() {
        // The cleanup in `Drop` may only ever reach this one folder: the guard refuses
        // anything that is not a direct child of the temp directory.
        let scratch = Scratch::dir("guard-scope");
        let root = scratch.root();
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(root.starts_with(std::env::temp_dir()));
        assert_ne!(
            root,
            std::env::temp_dir().as_path(),
            "the temp directory itself is not a scratch folder"
        );
    }

    #[test]
    fn a_scratch_folder_is_removed_when_the_guard_drops() {
        let root = {
            let scratch = Scratch::dir("guard-drop");
            let root = scratch.root().to_path_buf();
            std::fs::write(root.join("t.json"), "{}").unwrap();
            root
        };
        assert!(
            !root.exists(),
            "the scratch folder is gone: {}",
            root.display()
        );
        assert!(std::env::temp_dir().is_dir(), "the temp directory is still there");
    }

    #[test]
    fn a_failing_test_still_removes_its_scratch_folder() {
        // The whole point of the guard: a test that dies halfway leaves nothing behind.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut root: Option<PathBuf> = None;
        let result: Result<(), _> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let scratch = Scratch::dir("guard-panic");
            root = Some(scratch.root().to_path_buf());
            panic!("the body of the test fails on purpose");
        }));
        std::panic::set_hook(hook);
        assert!(result.is_err(), "the body of the test really failed");
        let root = root.expect("the folder was made");
        assert!(
            !root.exists(),
            "the scratch folder is gone even though the test failed: {}",
            root.display()
        );
    }

    #[test]
    fn a_scratch_file_lives_in_the_temp_directory_and_is_removed_with_the_guard() {
        let path = {
            let scratch = Scratch::file("armoury-guard-file.json");
            let path = scratch.root().to_path_buf();
            std::fs::write(&path, "{}").unwrap();
            path
        };
        assert!(
            !path.exists(),
            "the scratch file is gone: {}",
            path.display()
        );
        assert!(std::env::temp_dir().is_dir(), "the temp directory is still there");
    }
}
