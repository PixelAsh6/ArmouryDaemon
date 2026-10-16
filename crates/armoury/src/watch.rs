//! Tracked-app watcher — tiered and budget-aware:
//!   1. a 1 s `GetPerformanceInfo` process-count probe (~0.03 ms),
//!   2. only when that count changes: one `EnumProcesses` call (PIDs only) diffed against
//!      the previous sample — new PIDs get their full image path resolved and matched
//!      against the tracked list (a handful of cheap calls, never a full name walk),
//!   3. one process handle per running tracked app, waited on with `WaitForMultipleObjects`
//!      so exits are exact (a signaled handle means *that* process object exited) and
//!      PID-reuse safe.

use armoury_core::config::TrackedApp;
use crossbeam_channel::Sender;
use log::{info, warn};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::ProcessStatus::{
    EnumProcesses, GetPerformanceInfo, PERFORMANCE_INFORMATION,
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

const PROBE_INTERVAL: Duration = Duration::from_secs(1);

struct Running {
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
    let mut running: HashMap<u32, Running> = HashMap::new();
    let mut last_count: u32 = u32::MAX;
    let mut last_hash: u64 = 0;
    let mut last_pids: Vec<u32> = Vec::new();
    info!("watcher started");

    loop {
        // ---- 1. cheap probe, and a PID-level diff only when something changed ----
        let (hash, wanted) = tracked_snapshot(&tracked);
        let list_changed = hash != last_hash;
        let count = process_count();
        let count_changed = count != last_count;

        if count_changed || list_changed {
            let pids = enum_pids();
            if list_changed {
                // The tracked list changed: re-check everything currently running, and
                // forget handles for apps that are no longer tracked.
                for &pid in &pids {
                    consider(pid, &wanted, &mut running, &tx, wake);
                }
                let stale: Vec<u32> = running
                    .iter()
                    .filter(|(_, r)| !wanted.iter().any(|w| w.base == r.exe))
                    .map(|(pid, _)| *pid)
                    .collect();
                for pid in stale {
                    if let Some(r) = running.remove(&pid) {
                        let _ = tx.send(WatchEvent::Stopped {
                            key: r.exe.clone(),
                            pid,
                        });
                        signal(wake);
                    }
                }
            } else {
                // Only the PIDs that appeared since the previous sample can be starts.
                let previous: HashSet<u32> = last_pids.iter().copied().collect();
                for &pid in &pids {
                    if !previous.contains(&pid) {
                        consider(pid, &wanted, &mut running, &tx, wake);
                    }
                }
                // Belt and braces: a watched PID that vanished is a stop (the handle wait
                // normally reports it first).
                let current: HashSet<u32> = pids.iter().copied().collect();
                let gone: Vec<u32> = running
                    .keys()
                    .copied()
                    .filter(|pid| !current.contains(pid))
                    .collect();
                for pid in gone {
                    if let Some(r) = running.remove(&pid) {
                        let _ = tx.send(WatchEvent::Stopped {
                            key: r.exe.clone(),
                            pid,
                        });
                        signal(wake);
                    }
                }
            }
            last_pids = pids;
            last_count = count;
            last_hash = hash;
        }

        // ---- 2. wait for exits (exact, one handle per running tracked app) ----
        let mut pairs: Vec<(u32, HANDLE)> = running.iter().map(|(pid, r)| (*pid, r.handle)).collect();
        pairs.sort_unstable_by_key(|(pid, _)| *pid);

        if pairs.is_empty() {
            thread::sleep(PROBE_INTERVAL);
            continue;
        }

        for chunk in pairs.chunks(MAXIMUM_WAIT_OBJECTS as usize) {
            let handles: Vec<HANDLE> = chunk.iter().map(|(_, h)| *h).collect();
            let r = unsafe {
                WaitForMultipleObjects(
                    handles.len() as u32,
                    handles.as_ptr(),
                    0,
                    PROBE_INTERVAL.as_millis() as u32,
                )
            };
            if r == WAIT_TIMEOUT {
                continue;
            }
            if r >= WAIT_OBJECT_0 && r < WAIT_OBJECT_0 + handles.len() as u32 {
                let (pid, _) = chunk[(r - WAIT_OBJECT_0) as usize];
                if let Some(run) = running.remove(&pid) {
                    info!("tracked app stopped: {} (pid {pid})", run.name);
                    let _ = tx.send(WatchEvent::Stopped {
                        key: run.exe.clone(),
                        pid,
                    });
                    signal(wake);
                }
                // force a fresh PID sample next tick: a start in the same second could
                // have cancelled out the process-count probe
                last_count = u32::MAX;
            } else {
                warn!("WaitForMultipleObjects failed ({r}); re-sampling");
                last_count = u32::MAX;
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

/// Is this PID one of the tracked apps we are not watching yet?
/// Matching is by **file name** (the user's choice): any process whose exe name is in
/// the tracked list counts, regardless of where it runs from.
fn consider(
    pid: u32,
    wanted: &[Wanted],
    running: &mut HashMap<u32, Running>,
    tx: &Sender<WatchEvent>,
    wake: usize,
) {
    if wanted.is_empty() || pid == 0 || running.contains_key(&pid) {
        return;
    }
    let Some(path) = process_path(pid) else {
        return; // protected/exiting process
    };
    let base = base_name(&path);
    let Some(w) = wanted.iter().find(|w| w.base == base) else {
        return; // not one of ours
    };
    match open_for_wait(pid) {
        Some(handle) => {
            info!("tracked app started: {} (pid {pid})", w.display);
            running.insert(
                pid,
                Running {
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
        }
        None => warn!("cannot watch {} (pid {pid}) for exit: access denied", w.display),
    }
}

/// The tracked list, lowercased, plus a hash that changes on any edit.
fn tracked_snapshot(tracked: &Arc<Mutex<Vec<TrackedApp>>>) -> (u64, Vec<Wanted>) {
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
    let mut hasher = DefaultHasher::new();
    wanted.hash(&mut hasher);
    (hasher.finish(), wanted)
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

/// All PIDs, growing the buffer once if the system has more processes than expected.
fn enum_pids() -> Vec<u32> {
    let mut buf: Vec<u32> = vec![0; 2048];
    loop {
        let mut needed: u32 = 0;
        let bytes = (buf.len() * std::mem::size_of::<u32>()) as u32;
        let ok = unsafe { EnumProcesses(buf.as_mut_ptr(), bytes, &mut needed) };
        if ok == 0 {
            return Vec::new();
        }
        let count = (needed as usize) / std::mem::size_of::<u32>();
        if count < buf.len() {
            buf.truncate(count);
            return buf;
        }
        buf.resize(buf.len() * 2, 0);
    }
}

fn process_count() -> u32 {
    let mut info: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetPerformanceInfo(
            &mut info,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
    };
    if ok != 0 { info.ProcessCount as u32 } else { 0 }
}

pub fn process_path(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    let ok =
        unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) }
            != 0;
    unsafe { CloseHandle(handle) };
    if !ok {
        return None;
    }
    buf.truncate(len as usize);
    Some(String::from_utf16_lossy(&buf))
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
