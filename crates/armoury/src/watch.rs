//! Tracked-app watcher — one process walk per second while something is tracked, diffed
//! by PID:
//!   1. a `CreateToolhelp32Snapshot` walk (~13 ms) hands over every process's PID and
//!      exe file name in one call — the PID *set* is the change detector, so a start and
//!      an unrelated exit in the same second can never cancel each other out. The walk
//!      is skipped entirely while nothing is tracked: there is nothing it could match,
//!      and it is the whole cost of the tick,
//!   2. a PID that appeared is matched by exe file name against the tracked list (the
//!      user's rule); only the few matches get their full image path resolved,
//!   3. one process handle per running tracked app, waited on with `WaitForMultipleObjects`
//!      so exits are exact (a signaled handle means *that* process object exited) and
//!      PID-reuse safe.

use armoury_core::config::TrackedApp;
use crossbeam_channel::Sender;
use log::{info, warn};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::SystemServices::MAXIMUM_WAIT_OBJECTS;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, SetEvent, WaitForMultipleObjects,
};

pub enum WatchEvent {
    /// `key` is the lowercased file name (the tracked identity), `path` the resolved
    /// image path (used for pack templates), `display` what the user sees.
    Started {
        key: String,
        display: String,
        path: String,
        pid: u32,
    },
    Stopped {
        key: String,
        pid: u32,
    },
}

/// How long the watcher waits before it looks again.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// One process sample: PID -> lowercased exe file name.
type Sample = BTreeMap<u32, String>;

struct Running {
    /// unique per entry, so a recycled PID cannot be mistaken for the old process
    id: u64,
    handle: HANDLE,
    exe: String,
    name: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

pub fn start(tracked: Arc<Mutex<Vec<TrackedApp>>>, tx: Sender<WatchEvent>, wake: usize) {
    thread::spawn(move || run(tracked, tx, wake));
}

fn run(tracked: Arc<Mutex<Vec<TrackedApp>>>, tx: Sender<WatchEvent>, wake: usize) {
    let mut watcher = Watcher::new();
    let mut version: u64 = 0; // bumps whenever the watch list changes
    let mut pairs: Vec<(u32, u64, HANDLE)> = Vec::new();
    let mut pairs_version: u64 = u64::MAX; // forces the first build
    info!("watcher started");

    loop {
        // ---- 1. the tracked list, the walk and the starts and stops ----
        if watcher.step(&tracked, &tx, wake, &mut || enum_processes()) {
            version += 1;
        }

        // ---- 2. the handles to wait on ----
        if pairs_version != version {
            pairs.clear();
            pairs.extend(watcher.running.iter().map(|(pid, r)| (*pid, r.id, r.handle)));
            pairs.sort_unstable_by_key(|(pid, _, _)| *pid);
            pairs_version = version;
        }

        if pairs.is_empty() {
            thread::sleep(TICK_INTERVAL);
            continue;
        }

        // ---- 3. wait for exits (exact, one handle per running tracked app) ----
        for chunk in pairs.chunks(MAXIMUM_WAIT_OBJECTS as usize) {
            let handles: Vec<HANDLE> = chunk.iter().map(|(_, _, h)| *h).collect();
            let r = unsafe {
                WaitForMultipleObjects(
                    handles.len() as u32,
                    handles.as_ptr(),
                    0,
                    TICK_INTERVAL.as_millis() as u32,
                )
            };
            if r == WAIT_TIMEOUT {
                continue;
            }
            if r < handles.len() as u32 {
                let (pid, id, _) = chunk[(r - WAIT_OBJECT_0) as usize];
                // the id guards against a PID that was recycled meanwhile
                if watcher.running.get(&pid).map(|e| e.id) == Some(id)
                    && watcher.forget(pid, &tx, wake)
                {
                    version += 1;
                }
            } else {
                warn!("WaitForMultipleObjects failed ({r}); re-sampling");
                break;
            }
        }
    }
}

fn signal(wake: usize) {
    if wake != 0 {
        unsafe { SetEvent(wake as HANDLE) };
    }
}

/// The watcher's own state: the apps it is watching, the tracked list it matches against
/// and the sample of the last walk.
struct Watcher {
    running: HashMap<u32, Running>,
    next_id: u64,
    last_hash: Option<u64>,
    wanted: Vec<Wanted>,
    previous: Sample,
}

impl Watcher {
    fn new() -> Self {
        Watcher {
            running: HashMap::new(),
            next_id: 0,
            last_hash: None,
            wanted: Vec::new(),
            previous: Sample::new(),
        }
    }

    /// One tick: refresh the tracked list, walk the process table and report the starts
    /// and the stops. True when the set of watched apps changed.
    ///
    /// The walk is a full snapshot of every process on the machine, so it is skipped when
    /// nothing is tracked: there is nothing it could match. The tracked list itself is
    /// still read every tick, so a list that becomes non-empty is walked in the very tick
    /// it changes — a start is never noticed a tick later than it was before.
    fn step(
        &mut self,
        tracked: &Arc<Mutex<Vec<TrackedApp>>>,
        tx: &Sender<WatchEvent>,
        wake: usize,
        sample: &mut dyn FnMut() -> Sample,
    ) -> bool {
        let mut changed = false;

        // the tracked list, rebuilt only when it actually changed
        let hash = tracked_hash(tracked);
        let list_changed = self.last_hash != Some(hash);
        if list_changed {
            self.wanted = tracked_wanted(tracked);
            self.last_hash = Some(hash);
        }

        // one walk per tick, and only while something is tracked: the PID set is the
        // change detector
        // Nothing is tracked: the walk cannot match anything, and it is the expensive
        // part of the tick.
        if !self.wanted.is_empty() {
            let current = sample();
            let (started, gone) = diff_samples(&self.previous, &current);
            if list_changed {
                // The tracked list changed: re-check everything currently running, so an
                // app added to the list while it was already running is picked up.
                for (pid, name) in &current {
                    if self.consider(*pid, name, tx, wake) {
                        changed = true;
                    }
                }
            } else {
                // Only the PIDs that appeared since the previous sample can be starts.
                for (pid, name) in &started {
                    if self.consider(*pid, name, tx, wake) {
                        changed = true;
                    }
                }
            }
            // Belt and braces: a watched PID that vanished is a stop (the handle wait
            // normally reports it first).
            for pid in gone {
                if self.forget(pid, tx, wake) {
                    changed = true;
                }
            }
            self.previous = current;
        }

        if list_changed {
            // forget handles for apps that are no longer tracked
            let stale: Vec<u32> = self
                .running
                .iter()
                .filter(|(_, r)| !self.wanted.iter().any(|w| w.base == r.exe))
                .map(|(pid, _)| *pid)
                .collect();
            for pid in stale {
                if self.forget(pid, tx, wake) {
                    changed = true;
                }
            }
        }

        changed
    }

    /// Is this PID one of the tracked apps we are not watching yet?
    /// Matching is by **file name** (the user's choice): any process whose exe name is in
    /// the tracked list counts, regardless of where it runs from.
    /// Returns true when this PID joined the watched set — a start, or a recycled PID
    /// whose old entry was dropped.
    fn consider(
        &mut self,
        pid: u32,
        name: &str,
        tx: &Sender<WatchEvent>,
        wake: usize,
    ) -> bool {
        if self.wanted.is_empty() || pid == 0 {
            return false;
        }
        let mut changed = false;
        if let Some(old) = self.running.get(&pid) {
            if old.exe == name {
                return false; // already watched
            }
            // the PID was recycled: the old process is gone, this is a new one
            self.forget(pid, tx, wake);
            changed = true;
        }
        let Some(w) = self.wanted.iter().find(|w| w.base == name) else {
            return changed; // not one of ours
        };
        // Only the matches need their full path (pack templates).
        let Some(path) = process_path(pid) else {
            return changed; // protected/exiting process
        };
        match open_for_wait(pid) {
            Some(handle) => {
                info!("tracked app started: {} (pid {pid})", w.display);
                self.next_id += 1;
                self.running.insert(
                    pid,
                    Running {
                        id: self.next_id,
                        handle,
                        exe: w.base.clone(),
                        name: w.display.clone(),
                    },
                );
                let _ = tx.send(WatchEvent::Started {
                    key: w.base.clone(),
                    display: w.display.clone(),
                    path,
                    pid,
                });
                signal(wake);
                true
            }
            None => {
                warn!("cannot watch {} (pid {pid}) for exit: access denied", w.display);
                changed
            }
        }
    }

    /// Drop a PID from the watch list and report it as stopped. True when it was watched.
    fn forget(&mut self, pid: u32, tx: &Sender<WatchEvent>, wake: usize) -> bool {
        let Some(r) = self.running.remove(&pid) else {
            return false;
        };
        info!("tracked app stopped: {} (pid {pid})", r.name);
        let _ = tx.send(WatchEvent::Stopped {
            key: r.exe.clone(),
            pid,
        });
        signal(wake);
        true
    }
}

/// What changed between two samples: the PIDs that appeared (the only possible starts)
/// and the PIDs that vanished (the only possible stops). A start and an unrelated exit
/// cancel out in the process *count*, never in the PID set.
fn diff_samples(previous: &Sample, current: &Sample) -> (Vec<(u32, String)>, Vec<u32>) {
    let mut started: Vec<(u32, String)> = Vec::new();
    for (pid, name) in current {
        if previous.get(pid) != Some(name) {
            started.push((*pid, name.clone()));
        }
    }
    let mut gone: Vec<u32> = Vec::new();
    for pid in previous.keys() {
        if !current.contains_key(pid) {
            gone.push(*pid);
        }
    }
    (started, gone)
}

/// A cheap fingerprint of the tracked list — it changes on any edit.
fn tracked_hash(tracked: &Arc<Mutex<Vec<TrackedApp>>>) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Ok(list) = tracked.lock() {
        for app in list.iter() {
            app.exe.hash(&mut hasher);
            app.name.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// The tracked list, lowercased — rebuilt only when the hash above changes.
fn tracked_wanted(tracked: &Arc<Mutex<Vec<TrackedApp>>>) -> Vec<Wanted> {
    let list = tracked.lock().map(|g| g.clone()).unwrap_or_default();
    let mut wanted: Vec<Wanted> = list
        .iter()
        .map(|a| Wanted {
            exe: a.exe.to_ascii_lowercase(),
            base: base_name(&a.exe),
            display: display_name(a),
        })
        .collect();
    wanted.sort_by(|a, b| a.exe.cmp(&b.exe));
    wanted
}

/// One tracked app, pre-chewed for the watcher.
#[derive(Clone, Hash, PartialEq, Eq)]
struct Wanted {
    /// full path, lowercase — the identity used everywhere
    exe: String,
    /// lowercase file name — kept for diagnostics/logging
    base: String,
    /// what the user sees
    display: String,
}

fn base_name(exe: &str) -> String {
    std::path::Path::new(exe)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| exe.to_ascii_lowercase())
}

fn display_name(app: &TrackedApp) -> String {
    // The user's rule: tracked apps are shown by exe file name, not window title,
    // so this stays right even for entries added before the rule existed.
    std::path::Path::new(&app.exe)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.exe.clone())
}

/// One walk of the process list: PID -> lowercased exe file name.
fn enum_processes() -> Sample {
    let mut out = Sample::new();
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() || snap == INVALID_HANDLE_VALUE {
        return out;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while ok {
        let pid = entry.th32ProcessID;
        if pid != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            out.insert(pid, name.to_ascii_lowercase());
        }
        ok = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snap) };
    out
}

pub fn process_path(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let path = image_path(handle);
    unsafe { CloseHandle(handle) };
    path
}

/// The full image path of a process. 1024 characters cover every realistic path; the
/// buffer only grows when Windows asks for more.
fn image_path(handle: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) }
        != 0
    {
        return Some(String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]));
    }
    if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    // the call reported the size it needs (terminator included)
    let need = (len as usize).saturating_add(1).max(buf.len() + 1);
    let mut big = vec![0u16; need];
    len = big.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, big.as_mut_ptr(), &mut len) }
        != 0
    {
        return Some(String::from_utf16_lossy(&big[..(len as usize).min(big.len())]));
    }
    None
}

fn open_for_wait(pid: u32) -> Option<HANDLE> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if !handle.is_null() {
        return Some(handle);
    }
    // Fall back to the minimal right needed for waiting.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() { None } else { Some(handle) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use std::cell::Cell;
    use std::process::{Command, Stdio};

    fn sample(pairs: &[(u32, &str)]) -> Sample {
        pairs.iter().map(|(pid, name)| (*pid, name.to_string())).collect()
    }

    #[test]
    fn a_start_offset_by_an_exit_is_still_seen() {
        let previous = sample(&[(10, "explorer.exe"), (11, "notepad.exe")]);
        let current = sample(&[(10, "explorer.exe"), (12, "game.exe")]);
        assert_eq!(previous.len(), current.len(), "the process count is unchanged");
        let (started, gone) = diff_samples(&previous, &current);
        assert_eq!(started, vec![(12, "game.exe".to_string())]);
        assert_eq!(gone, vec![11]);
    }

    #[test]
    fn an_unchanged_sample_reports_nothing() {
        let previous = sample(&[(10, "explorer.exe")]);
        let current = sample(&[(10, "explorer.exe")]);
        let (started, gone) = diff_samples(&previous, &current);
        assert!(started.is_empty(), "{started:?}");
        assert!(gone.is_empty(), "{gone:?}");
    }

    #[test]
    fn a_recycled_pid_counts_as_a_new_process() {
        let previous = sample(&[(10, "notepad.exe")]);
        let current = sample(&[(10, "game.exe")]);
        let (started, gone) = diff_samples(&previous, &current);
        assert_eq!(started, vec![(10, "game.exe".to_string())]);
        assert!(gone.is_empty(), "{gone:?}");
    }

    #[test]
    fn tracked_apps_match_by_file_name() {
        let tracked = Arc::new(Mutex::new(vec![TrackedApp {
            name: "Game".into(),
            exe: "C:\\Games\\Steam\\bin\\game.exe".into(),
        }]));
        let wanted = tracked_wanted(&tracked);
        assert_eq!(wanted[0].base, "game.exe");
        // a moved copy still matches ...
        let moved = TrackedApp {
            name: "Game".into(),
            exe: "D:\\Elsewhere\\game.exe".into(),
        };
        assert_eq!(base_name(&moved.exe), wanted[0].base);
        // ... a renamed exe does not
        let renamed = TrackedApp {
            name: "Game".into(),
            exe: "C:\\Games\\Steam\\bin\\game2.exe".into(),
        };
        assert_ne!(base_name(&renamed.exe), wanted[0].base);
    }

    #[test]
    fn the_tracked_hash_follows_the_list() {
        let tracked = Arc::new(Mutex::new(Vec::new()));
        let before = tracked_hash(&tracked);
        tracked.lock().unwrap().push(TrackedApp {
            name: "Notepad".into(),
            exe: "C:\\Windows\\System32\\notepad.exe".into(),
        });
        assert_ne!(tracked_hash(&tracked), before);
    }

    #[test]
    fn an_empty_tracked_list_never_walks_the_process_table() {
        // The walk is a full snapshot of every process on the machine. With nothing
        // tracked it cannot match anything, so it must not happen at all.
        let tracked = Arc::new(Mutex::new(Vec::new()));
        let (tx, _rx) = unbounded();
        let mut watcher = Watcher::new();
        let walks = Cell::new(0);
        let mut walk = || {
            walks.set(walks.get() + 1);
            Sample::new()
        };
        for _ in 0..3 {
            watcher.step(&tracked, &tx, 0, &mut walk);
        }
        assert_eq!(
            walks.get(),
            0,
            "an empty tracked list must not walk the process table"
        );
    }

    #[test]
    fn a_newly_tracked_app_is_picked_up_on_the_next_tick() {
        // A real process, so that the watcher can resolve its path and wait for its exit.
        let probe = std::path::Path::new("C:\\Windows\\System32\\ping.exe");
        if !probe.is_file() {
            return;
        }
        let mut child = Command::new(probe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("ping.exe starts");
        let pid = child.id();

        let tracked = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = unbounded();
        let mut watcher = Watcher::new();
        let mut walk = || sample(&[(pid, "ping.exe")]);

        // nothing is tracked: the walk is skipped and nothing is reported
        watcher.step(&tracked, &tx, 0, &mut walk);
        assert!(
            rx.try_recv().is_err(),
            "nothing may be reported while nothing is tracked"
        );

        // the app is tracked now: the next tick must notice it, not the one after
        tracked.lock().unwrap().push(TrackedApp {
            name: "Ping".into(),
            exe: "C:\\Windows\\System32\\ping.exe".into(),
        });
        watcher.step(&tracked, &tx, 0, &mut walk);
        match rx
            .try_recv()
            .expect("the newly tracked app must be noticed on the next tick")
        {
            WatchEvent::Started { key, pid: seen, .. } => {
                assert_eq!(key, "ping.exe");
                assert_eq!(seen, pid);
            }
            WatchEvent::Stopped { .. } => panic!("the app started, it did not stop"),
        }

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn process_path_resolves_a_live_process() {
        let probe = std::path::Path::new("C:\\Windows\\System32\\ping.exe");
        if !probe.is_file() {
            return;
        }
        let mut child = Command::new(probe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("ping.exe starts");
        let path = process_path(child.id());
        let _ = child.kill();
        let _ = child.wait();
        let path = path.expect("the image path resolves");
        let file = std::path::Path::new(&path);
        assert!(file.is_file(), "{path}");
        assert_eq!(
            file.file_name().map(|s| s.to_string_lossy().to_ascii_lowercase()),
            Some("ping.exe".to_string()),
            "{path}"
        );
    }
}
