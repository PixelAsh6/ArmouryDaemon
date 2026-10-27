//! Tracked-app watcher — one process walk per second while something is tracked, diffed
//! by PID:
//!   1. one `EnumProcesses` call hands over the machine's PID array — no names, ~60 µs
//!      — and the PID *set* is the change detector, so a start and an unrelated exit in
//!      the same second can never cancel each other out. The call is skipped entirely
//!      while nothing is tracked: there is nothing it could match, and it is the whole
//!      cost of the tick. (Measured 2026-10-27 on a 373-process machine: the old
//!      `CreateToolhelp32Snapshot` walk plus one `Process32NextW` per process cost
//!      19-23 ms a tick, this call 60 µs.),
//!   2. a PID the last tick did not show is the only one whose exe file name is resolved
//!      (~40 µs: `OpenProcess` plus `QueryFullProcessImageNameW`) and matched against the
//!      tracked list (the user's rule); only the few matches get their full image path
//!      resolved again for the pack templates,
//!   3. one process handle per running tracked app, waited on with `WaitForMultipleObjects`
//!      so exits are exact (a signaled handle means *that* process object exited) and
//!      PID-reuse safe,
//!   4. a **recycled** PID — the old process exited and a new one took its PID — is a
//!      genuine stop followed by a start: the handle wait reports the stop, and the next
//!      tick that shows the PID again is reported as a start. The PID set cannot see that
//!      on its own (the PID is simply there again), so the watcher looks the exited PIDs
//!      up again.

use armoury_core::config::TrackedApp;
use crossbeam_channel::Sender;
use log::{info, warn};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
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

/// How many PIDs one walk asks for: 64 kB of PID array, several times the process count
/// of a busy machine. The buffer grows on its own if a machine ever exceeds it.
const PID_CAPACITY: usize = 16 * 1024;

/// The two machine calls the watcher makes, injectable so the tests can drive them.
struct Probe<'a> {
    /// Fill the buffer with the machine's PIDs and return how many were written; 0 means
    /// the walk failed.
    pids: &'a mut dyn FnMut(&mut Vec<u32>) -> usize,
    /// The lowercased exe file name of a PID — resolved only when it is needed.
    name: &'a mut dyn FnMut(u32) -> Option<String>,
}

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
    let mut pids_fn = |buf: &mut Vec<u32>| process_list(buf);
    let mut name_fn = |pid: u32| process_base_name(pid);
    let mut probe = Probe {
        pids: &mut pids_fn,
        name: &mut name_fn,
    };
    info!("watcher started");

    loop {
        // ---- 1. the tracked list, the process list and the starts and stops ----
        if watcher.step(&tracked, &tx, wake, &mut probe) {
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
                if watcher.exited(pid, id, &tx, wake) {
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
/// and the PIDs of the last walk.
struct Watcher {
    running: HashMap<u32, Running>,
    next_id: u64,
    last_hash: Option<u64>,
    wanted: Vec<Wanted>,
    /// The machine's PIDs as of the last tick, sorted: the set is the change detector, so
    /// a start and an unrelated exit in the same tick can never cancel each other out.
    previous: Vec<u32>,
    /// Scratch: this tick's PID array, straight from the machine, sorted in place.
    pids: Vec<u32>,
    /// The PIDs whose process object exited: the stop was reported, but the PID may be
    /// taken by a new process. The next tick that shows one of them again is a *new*
    /// process, and that is a start.
    recycled: Vec<u32>,
}

impl Watcher {
    fn new() -> Self {
        Watcher {
            running: HashMap::new(),
            next_id: 0,
            last_hash: None,
            wanted: Vec::new(),
            previous: Vec::new(),
            pids: vec![0u32; PID_CAPACITY],
            recycled: Vec::new(),
        }
    }

    /// One tick: refresh the tracked list, take the machine's PID list and report the
    /// starts and the stops. True when the set of watched apps changed.
    ///
    /// The PID list is the whole cost of the tick and it is skipped when nothing is
    /// tracked: there is nothing it could match. The tracked list itself is still read
    /// every tick, so a list that becomes non-empty is walked in the very tick it changes
    /// — a start is never noticed a tick later than it was before. A name is resolved only
    /// for a PID that is new (or, on a list change, for everything running), because
    /// resolving one is its own pair of syscalls.
    fn step(
        &mut self,
        tracked: &Arc<Mutex<Vec<TrackedApp>>>,
        tx: &Sender<WatchEvent>,
        wake: usize,
        probe: &mut Probe,
    ) -> bool {
        let mut changed = false;

        // the tracked list, rebuilt only when it actually changed
        let hash = tracked_hash(tracked);
        let list_changed = self.last_hash != Some(hash);
        if list_changed {
            self.wanted = tracked_wanted(tracked);
            self.last_hash = Some(hash);
        }

        // one PID list per tick, and only while something is tracked: the set is the
        // change detector.
        // Nothing is tracked: the list cannot match anything, and it is the whole cost of
        // the tick.
        if !self.wanted.is_empty() {
            let n = (probe.pids)(&mut self.pids);
            // A walk that reports nothing at all failed. The watched set is left as it is
            // and the next tick tries again: reading a failed walk as "every process
            // exited" would stop every tracked app and start every pack over.
            if n > 0 {
                self.pids[..n].sort_unstable();
                let (started, gone) = diff_pids(&self.previous, &self.pids[..n]);
                // A PID whose process object exited may have been recycled: a PID set
                // cannot see that — the PID is simply there again — so the exited PIDs
                // are looked at again. The handle is the proof that the old process is
                // gone.
                for pid in take_recycled(&mut self.recycled, &self.pids[..n]) {
                    let Some(name) = (probe.name)(pid) else {
                        continue;
                    };
                    if self.consider(pid, &name, tx, wake) {
                        changed = true;
                    }
                }
                if list_changed {
                    // The tracked list changed: re-check everything currently running, so
                    // an app added to the list while it was already running is picked up.
                    // This one tick resolves every name — a few milliseconds, once.
                    for i in 0..n {
                        let pid = self.pids[i];
                        let Some(name) = (probe.name)(pid) else {
                            continue;
                        };
                        if self.consider(pid, &name, tx, wake) {
                            changed = true;
                        }
                    }
                } else {
                    // Only the PIDs that appeared since the last tick can be starts.
                    for pid in started {
                        let Some(name) = (probe.name)(pid) else {
                            continue;
                        };
                        if self.consider(pid, &name, tx, wake) {
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
                self.previous.clear();
                self.previous.extend_from_slice(&self.pids[..n]);
            }
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

    /// The process object behind the handle exited: the app is gone and the stop is
    /// reported now. The PID may be **recycled** — a new process may take it — so the PID
    /// is remembered: the next tick that shows it again is a new process, and that is a
    /// start. The id guards against an entry that was replaced meanwhile.
    fn exited(&mut self, pid: u32, id: u64, tx: &Sender<WatchEvent>, wake: usize) -> bool {
        if self.running.get(&pid).map(|e| e.id) != Some(id) {
            return false; // the entry belongs to another process now
        }
        if !self.forget(pid, tx, wake) {
            return false;
        }
        self.recycled.push(pid);
        true
    }
}

/// What changed between two walks: the PIDs that appeared (the only possible starts) and
/// the PIDs that vanished (the only possible stops). A start and an unrelated exit cancel
/// out in the process *count*, never in the PID set. Both lists must be sorted, which is
/// what the watcher holds.
fn diff_pids(previous: &[u32], current: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let started: Vec<u32> = current
        .iter()
        .copied()
        .filter(|pid| previous.binary_search(pid).is_err())
        .collect();
    let gone: Vec<u32> = previous
        .iter()
        .copied()
        .filter(|pid| current.binary_search(pid).is_err())
        .collect();
    (started, gone)
}

/// The PIDs the current walk shows again: their process object exited, so whatever holds
/// the PID now is a *new* process. The list is emptied — every exit is looked at exactly
/// once.
fn take_recycled(recycled: &mut Vec<u32>, current: &[u32]) -> Vec<u32> {
    let out: Vec<u32> = recycled
        .iter()
        .copied()
        .filter(|pid| current.binary_search(pid).is_ok())
        .collect();
    recycled.clear();
    out
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

/// One walk of the process table: the PIDs only, into the caller's buffer. Returns how
/// many were written, 0 when the walk failed.
///
/// `EnumProcesses` answers with a plain PID array — about a kilobyte for the whole
/// machine. The old walk (`CreateToolhelp32Snapshot` plus one `Process32NextW` per
/// process) also returned the exe file name of every process, and that is exactly what it
/// charged for. Names are needed only for PIDs that are new, and those have one resolved
/// on the spot.
fn process_list(buf: &mut Vec<u32>) -> usize {
    // present the whole buffer: the walk fills it up to its length
    let capacity = buf.capacity().max(PID_CAPACITY);
    buf.resize(capacity, 0);
    for _ in 0..4 {
        let mut needed: u32 = 0;
        let ok = unsafe {
            EnumProcesses(
                buf.as_mut_ptr(),
                (buf.len() * std::mem::size_of::<u32>()) as u32,
                &mut needed,
            )
        };
        let n = needed as usize / std::mem::size_of::<u32>();
        if ok == 0 {
            // the buffer was too small: psapi asks for a bigger one instead of truncating
            if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || n <= buf.len() {
                return 0;
            }
        } else if n < buf.len() {
            return n;
        }
        buf.resize(buf.len() * 2, 0);
    }
    0
}

/// The lowercased exe file name of a PID — the tracked-app identity. Resolving it is its
/// own pair of syscalls, so it happens only for a PID that needs it. `None` for a process
/// that cannot be opened: it could not be watched either way.
fn process_base_name(pid: u32) -> Option<String> {
    process_path(pid).map(|path| base_name(&path))
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

    /// A PID list, sorted the way the watcher holds one.
    fn pids(list: &[u32]) -> Vec<u32> {
        let mut out = list.to_vec();
        out.sort_unstable();
        out
    }

    #[test]
    fn a_start_offset_by_an_exit_is_still_seen() {
        let previous = pids(&[10, 11]);
        let current = pids(&[10, 12]);
        assert_eq!(previous.len(), current.len(), "the process count is unchanged");
        let (started, gone) = diff_pids(&previous, &current);
        assert_eq!(started, vec![12]);
        assert_eq!(gone, vec![11]);
    }

    #[test]
    fn an_unchanged_pid_set_reports_nothing() {
        let previous = pids(&[10]);
        let current = pids(&[10]);
        let (started, gone) = diff_pids(&previous, &current);
        assert!(started.is_empty(), "{started:?}");
        assert!(gone.is_empty(), "{gone:?}");
    }

    #[test]
    fn a_pid_that_vanished_and_came_back_is_a_start_again() {
        let (started, gone) = diff_pids(&pids(&[10]), &pids(&[]));
        assert!(started.is_empty(), "{started:?}");
        assert_eq!(gone, vec![10]);
        let (started, gone) = diff_pids(&pids(&[]), &pids(&[10]));
        assert_eq!(started, vec![10]);
        assert!(gone.is_empty(), "{gone:?}");
    }

    #[test]
    fn a_recycled_pid_is_invisible_to_the_pid_set() {
        // A PID set cannot see a recycle at all: the PID is simply there again. The proof
        // that the old process is gone is its handle, and the start is reported from
        // there — the end-to-end case is pinned by
        // `a_recycled_pid_is_reported_as_a_stop_and_then_a_start` below.
        let (started, gone) = diff_pids(&pids(&[10]), &pids(&[10]));
        assert!(started.is_empty(), "{started:?}");
        assert!(gone.is_empty(), "{gone:?}");
    }

    #[test]
    fn names_are_resolved_only_for_pids_that_are_new() {
        // Resolving a name is a syscall pair of its own: it must happen for a PID the last
        // tick did not show, and for nothing else.
        let tracked = Arc::new(Mutex::new(vec![TrackedApp {
            name: "Game".into(),
            exe: "C:\\Games\\game.exe".into(),
        }]));
        let (tx, _rx) = unbounded();
        let mut watcher = Watcher::new();
        let tick = Cell::new(0usize);
        let resolved = Cell::new(0usize);
        let mut pids_fn = |buf: &mut Vec<u32>| {
            buf.clear();
            buf.extend([10u32, 11, 12]);
            if tick.get() == 2 {
                buf.push(13);
            }
            tick.set(tick.get() + 1);
            buf.len()
        };
        let mut name_fn = |pid: u32| {
            resolved.set(resolved.get() + 1);
            Some(format!("other{pid}.exe"))
        };
        let mut probe = Probe {
            pids: &mut pids_fn,
            name: &mut name_fn,
        };

        watcher.step(&tracked, &tx, 0, &mut probe);
        assert_eq!(
            resolved.get(),
            3,
            "the first tick resolves the names of everything it sees"
        );
        watcher.step(&tracked, &tx, 0, &mut probe);
        assert_eq!(resolved.get(), 3, "an unchanged PID set resolves nothing");
        watcher.step(&tracked, &tx, 0, &mut probe);
        assert_eq!(resolved.get(), 4, "only the PID that appeared is resolved");
    }

    #[test]
    fn a_walk_that_reports_nothing_changes_nothing() {
        // A failed walk must not read as "every process exited": that would stop every
        // tracked app and start every pack over again.
        let probe_path = std::path::Path::new("C:\\Windows\\System32\\ping.exe");
        if !probe_path.is_file() {
            return;
        }
        let mut child = Command::new(probe_path)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("ping.exe starts");
        let pid = child.id();
        let tracked = Arc::new(Mutex::new(vec![TrackedApp {
            name: "Ping".into(),
            exe: "C:\\Windows\\System32\\ping.exe".into(),
        }]));
        let (tx, rx) = unbounded();
        let mut watcher = Watcher::new();
        let tick = Cell::new(0usize);
        let mut pids_fn = |buf: &mut Vec<u32>| {
            buf.clear();
            let t = tick.get();
            tick.set(t + 1);
            if t == 1 {
                return 0; // the walk fails on the second tick
            }
            buf.push(pid);
            buf.len()
        };
        let mut name_fn = |pid: u32| process_base_name(pid);
        let mut probe = Probe {
            pids: &mut pids_fn,
            name: &mut name_fn,
        };

        watcher.step(&tracked, &tx, 0, &mut probe);
        assert!(
            matches!(rx.try_recv(), Ok(WatchEvent::Started { .. })),
            "the tracked app is watched"
        );

        watcher.step(&tracked, &tx, 0, &mut probe);
        assert!(
            rx.try_recv().is_err(),
            "a walk that reports nothing must report nothing"
        );
        assert_eq!(watcher.running.len(), 1, "the app stays watched");

        // and the tick after the failed one still sees the same PID without re-reporting it
        watcher.step(&tracked, &tx, 0, &mut probe);
        assert!(rx.try_recv().is_err(), "an unchanged PID set reports nothing");
        assert_eq!(watcher.running.len(), 1, "the app is still watched");

        let _ = child.kill();
        let _ = child.wait();
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
        let mut pids_fn = |_buf: &mut Vec<u32>| {
            walks.set(walks.get() + 1);
            0
        };
        let mut name_fn = |_pid: u32| None;
        let mut probe = Probe {
            pids: &mut pids_fn,
            name: &mut name_fn,
        };
        for _ in 0..3 {
            watcher.step(&tracked, &tx, 0, &mut probe);
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
        let mut pids_fn = |buf: &mut Vec<u32>| {
            buf.clear();
            buf.push(pid);
            buf.len()
        };
        let mut name_fn = |pid: u32| process_base_name(pid);
        let mut probe = Probe {
            pids: &mut pids_fn,
            name: &mut name_fn,
        };

        // nothing is tracked: the walk is skipped and nothing is reported
        watcher.step(&tracked, &tx, 0, &mut probe);
        assert!(
            rx.try_recv().is_err(),
            "nothing may be reported while nothing is tracked"
        );

        // the app is tracked now: the next tick must notice it, not the one after
        tracked.lock().unwrap().push(TrackedApp {
            name: "Ping".into(),
            exe: "C:\\Windows\\System32\\ping.exe".into(),
        });
        watcher.step(&tracked, &tx, 0, &mut probe);
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
    fn a_recycled_pid_is_reported_as_a_stop_and_then_a_start() {
        // A game exits and a new one takes the same PID with the same exe name. The sample
        // diff cannot see that — the same PID with the same name looks unchanged — so the
        // watcher looks the exited PID up again: the handle is the proof that the old
        // process is gone. The engine has to hear both, or the packs would revert and
        // never come back (2026-10-21).
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

        let tracked = Arc::new(Mutex::new(vec![TrackedApp {
            name: "Ping".into(),
            exe: "C:\\Windows\\System32\\ping.exe".into(),
        }]));
        let (tx, rx) = unbounded();
        let mut watcher = Watcher::new();
        let mut pids_fn = |buf: &mut Vec<u32>| {
            buf.clear();
            buf.push(pid);
            buf.len()
        };
        let mut name_fn = |pid: u32| process_base_name(pid);
        let mut probe = Probe {
            pids: &mut pids_fn,
            name: &mut name_fn,
        };

        // the app is tracked and running: the watcher watches it
        watcher.step(&tracked, &tx, 0, &mut probe);
        match rx
            .try_recv()
            .expect("the tracked app must be reported as started")
        {
            WatchEvent::Started { key, pid: seen, .. } => {
                assert_eq!(key, "ping.exe");
                assert_eq!(seen, pid);
            }
            WatchEvent::Stopped { .. } => panic!("the app started, it did not stop"),
        }

        // the process object exits: the stop is reported at once
        let id = watcher.running.get(&pid).expect("the app is watched").id;
        assert!(watcher.exited(pid, id, &tx, 0), "the exit is reported");
        match rx.try_recv().expect("the exit must be reported") {
            WatchEvent::Stopped { key, pid: seen } => {
                assert_eq!(key, "ping.exe");
                assert_eq!(seen, pid);
            }
            WatchEvent::Started { .. } => panic!("the app stopped, it did not start"),
        }

        // the PID is taken by a new process with the same exe name: the PID set sees
        // nothing, but the watcher must report the start
        watcher.step(&tracked, &tx, 0, &mut probe);
        match rx
            .try_recv()
            .expect("the recycled PID must be reported as started")
        {
            WatchEvent::Started { key, pid: seen, .. } => {
                assert_eq!(key, "ping.exe");
                assert_eq!(seen, pid);
            }
            WatchEvent::Stopped { .. } => panic!("the recycled PID is a start, not a stop"),
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
