//! Killswitch: hide + mute the running tracked apps, then put back exactly what changed.
//!
//! Everything is recorded in a ledger so the restore is exact and only touches what we
//! altered (windows we hid, audio sessions we muted and their previous mute state).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use log::{info, warn};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::core::{BOOL, Interface};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioSessionControl, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GW_OWNER, GetWindow, GetWindowLongPtrW, GetWindowThreadProcessId,
    IsWindowVisible, SW_HIDE, SW_SHOW, ShowWindow, WS_EX_TOOLWINDOW,
};

/// One audio session, as the killswitch sees it: the process that owns it and its mute
/// switch. The real one is a Windows audio session; the test's one only remembers what it
/// was told, so that nothing on the machine is really muted.
trait Session {
    /// The process that owns the session.
    fn pid(&self) -> u32;
    /// A stable name of the session: the same session keeps it from sweep to sweep, so
    /// the host can tell the sessions it muted from the ones the user muted.
    fn id(&self) -> String;
    /// The mute state, when it can be read.
    fn is_muted(&self) -> Option<bool>;
    /// Set the mute state.
    fn set_muted(&self, muted: bool) -> Result<(), String>;
}

/// A session of the Windows audio device.
struct AudioSession {
    pid: u32,
    id: String,
    volume: ISimpleAudioVolume,
}

impl Session for AudioSession {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn is_muted(&self) -> Option<bool> {
        unsafe { self.volume.GetMute().ok().map(|m| m.as_bool()) }
    }

    fn set_muted(&self, muted: bool) -> Result<(), String> {
        unsafe {
            self.volume
                .SetMute(muted, std::ptr::null())
                .map_err(|e| e.to_string())
        }
    }
}

/// One session of a tracked app, and the mute state it was in before. The release puts
/// every session back exactly as it was — and it needs no sweep to do it, because the
/// session itself is in the ledger.
struct Muted {
    pid: u32,
    /// The mute state before the killswitch touched it.
    was_muted: bool,
    session: Arc<dyn Session>,
}

#[derive(Default)]
pub struct Ledger {
    /// (window, process) for every window we hid
    hidden: Vec<(isize, u32)>,
    /// every session of a tracked app, with the mute state it was in before and the
    /// session itself: the release puts them back from here, without a sweep
    muted: Vec<Muted>,
    active: bool,
}

impl Ledger {
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// How many apps the killswitch is suppressing right now: the distinct processes it has
    /// hidden a window for or muted a session for. A released killswitch holds nothing, so
    /// this is 0 — which is what the dashboard shows.
    ///
    /// A session the user had muted already is in the ledger but was not changed by the
    /// killswitch, so it is not counted: the dashboard reports what we suppressed.
    pub fn affected_apps(&self) -> usize {
        let mut pids: Vec<u32> = self.hidden.iter().map(|(_, pid)| *pid).collect();
        pids.extend(self.muted.iter().filter(|m| !m.was_muted).map(|m| m.pid));
        pids.sort_unstable();
        pids.dedup();
        pids.len()
    }
}

/// COM for the audio interfaces, once per thread.
pub fn init_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Hide every visible top-level window of these processes and mute their audio sessions.
pub fn engage(pids: &[u32], ledger: &mut Ledger) -> (usize, usize) {
    engage_with(pids, ledger, sessions)
}

/// The killswitch proper, with the session sweep injected — the tests count the sweeps.
fn engage_with<F>(pids: &[u32], ledger: &mut Ledger, sweep: F) -> (usize, usize)
where
    F: Fn() -> Vec<Arc<dyn Session>>,
{
    let mut hidden_now = 0;
    let mut muted_now = 0;

    // ONE sweep for all the tracked apps: the sessions are indexed by the process that
    // owns them, so a pid is a lookup instead of another walk of every session on the
    // machine.
    let index = index_by_pid(sweep());

    for pid in pids {
        for hwnd in windows_of(*pid) {
            if !ledger.hidden.iter().any(|(w, _)| *w == hwnd) {
                unsafe {
                    let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
                }
                ledger.hidden.push((hwnd, *pid));
                hidden_now += 1;
            }
        }
        match index.get(pid) {
            Some(list) => {
                for session in list {
                    let Some(was) = session.is_muted() else {
                        // the state cannot be read: the session is left alone and not
                        // recorded, so the release never touches it
                        continue;
                    };
                    if !was {
                        match session.set_muted(true) {
                            Ok(()) => {
                                muted_now += 1;
                                info!("killswitch: pid {pid} session muted");
                            }
                            Err(e) => warn!("SetMute failed for pid {pid}: {e}"),
                        }
                    }
                    // the session and the state it was in: the release needs both
                    ledger.muted.push(Muted {
                        pid: *pid,
                        was_muted: was,
                        session: Arc::clone(session),
                    });
                }
            }
            None => info!("killswitch: no audio session for pid {pid} (nothing to mute)"),
        }
    }

    ledger.active = true;
    info!(
        "killswitch engaged: {hidden_now} window(s) hidden, {muted_now} session(s) muted (ledger: {} windows, {} sessions)",
        ledger.hidden.len(),
        ledger.muted.len()
    );
    (ledger.hidden.len(), ledger.muted.len())
}

/// Put back exactly what we changed.
pub fn disengage(ledger: &mut Ledger) {
    for (hwnd, _) in ledger.hidden.drain(..) {
        unsafe {
            let _ = ShowWindow(HWND(hwnd as *mut _), SW_SHOW);
        }
    }
    // The ledger holds the sessions themselves, so the release needs no sweep at all.
    for entry in ledger.muted.drain(..) {
        // only unmute what we muted (leave sessions that were already muted alone)
        if !entry.was_muted {
            if let Err(e) = entry.session.set_muted(false) {
                warn!("killswitch: pid {} could not be unmuted: {e}", entry.pid);
            }
        }
    }
    ledger.active = false;
    info!("killswitch released: windows shown, audio restored");
}

/// Visible, unowned, non-tool top-level windows belonging to a process.
pub fn windows_of(pid: u32) -> Vec<isize> {
    struct Ctx {
        pid: u32,
        out: Vec<isize>,
    }
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            if IsWindowVisible(hwnd).as_bool() {
                let mut owner_pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
                if owner_pid == ctx.pid
                    && GetWindow(hwnd, GW_OWNER).is_err()
                    && (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & (WS_EX_TOOLWINDOW.0 as isize)) == 0
                {
                    ctx.out.push(hwnd.0 as isize);
                }
            }
        }
        BOOL(1)
    }

    let mut ctx = Ctx {
        pid,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

/// Every audio session on every active output device (an app can play to a device
/// that is not the default one).
fn sessions() -> Vec<Arc<dyn Session>> {
    let mut out: Vec<Arc<dyn Session>> = Vec::new();
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return out;
        };
        let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        let device_count = collection.GetCount().unwrap_or(0);
        for index in 0..device_count {
            let Ok(device) = collection.Item(index) else {
                continue;
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(list) = manager.GetSessionEnumerator() else {
                continue;
            };
            let session_count = list.GetCount().unwrap_or(0);
            for session_index in 0..session_count {
                let Ok(control) = list.GetSession(session_index) else {
                    continue;
                };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = control2.GetProcessId().unwrap_or(0);
                let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                    continue;
                };
                let id = session_id(&control2, pid, session_index);
                out.push(Arc::new(AudioSession { pid, id, volume }));
            }
        }
    }
    out
}

/// The stable name of an audio session. Windows gives every session an identifier; when
/// it cannot be read, the process and the place of the session in the sweep stand in for
/// it — that is not as good, but it is still the same session from one sweep to the next.
fn session_id(control: &IAudioSessionControl2, pid: u32, index: i32) -> String {
    unsafe {
        match control.GetSessionIdentifier() {
            Ok(pointer) => {
                let text = pointer.to_string().unwrap_or_default();
                CoTaskMemFree(Some(pointer.0 as *const core::ffi::c_void));
                if text.is_empty() {
                    format!("{pid}#{index}")
                } else {
                    text
                }
            }
            Err(_) => format!("{pid}#{index}"),
        }
    }
}

/// The audio sessions of the machine, as (process, state): the state is "muted",
/// "playing" or "unknown" (a session whose mute state cannot be read).
pub fn session_states() -> Vec<(u32, String)> {
    sessions()
        .iter()
        .map(|session| {
            let state = match session.is_muted() {
                Some(true) => "muted",
                Some(false) => "playing",
                None => "unknown",
            };
            (session.pid(), state.to_string())
        })
        .collect()
}

/// The sessions a host call muted, by process and by session name: exactly the ones it
/// has to unmute again. A session the user had muted already is not in here — it was not
/// changed, and it must never be unmuted again.
///
/// A `OnceLock` and not a plain `Mutex`: `HashMap::new` is not a `const fn`, so the map
/// cannot be built in the static's own initializer.
static MUTED_BY_HOST: OnceLock<Mutex<HashMap<u32, HashSet<String>>>> = OnceLock::new();

/// The ledger of the sessions a host call muted.
fn muted_by_host() -> MutexGuard<'static, HashMap<u32, HashSet<String>>> {
    MUTED_BY_HOST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Mute (or unmute) the audio sessions of one process, and report what changed.
///
/// The sessions this call muted are remembered, so the unmute puts back exactly what was
/// changed: a session the user had muted already is never unmuted again.
pub fn mute_pid(pid: u32, muted: bool) -> String {
    let sessions = sessions();
    let mut changed = 0usize;
    let mut left_alone = 0usize;
    let mut failed = 0usize;
    {
        let mut ledger = muted_by_host();
        for session in sessions.iter().filter(|s| s.pid() == pid) {
            let Some(was) = session.is_muted() else {
                left_alone += 1; // the state cannot be read: the session is left alone
                continue;
            };
            if muted {
                if was {
                    left_alone += 1; // the user muted it: it is not ours to change
                    continue;
                }
                match session.set_muted(true) {
                    Ok(()) => {
                        changed += 1;
                        ledger.entry(pid).or_default().insert(session.id());
                    }
                    Err(e) => {
                        failed += 1;
                        warn!("SetMute failed for pid {pid}: {e}");
                    }
                }
            } else {
                let ours = ledger
                    .get(&pid)
                    .map(|ids| ids.contains(&session.id()))
                    .unwrap_or(false);
                if !ours {
                    left_alone += 1; // not a session we muted: the user's own mute stays
                    continue;
                }
                match session.set_muted(false) {
                    Ok(()) => changed += 1,
                    Err(e) => {
                        failed += 1;
                        warn!("SetMute failed for pid {pid}: {e}");
                    }
                }
            }
        }
        if !muted {
            ledger.remove(&pid);
        }
    }
    format!(
        "{changed} session(s) {}, {left_alone} left alone, {failed} could not be changed",
        if muted { "muted" } else { "unmuted" }
    )
}

/// The sessions, indexed by the process that owns them: one sweep answers for every
/// tracked app.
fn index_by_pid(sessions: Vec<Arc<dyn Session>>) -> HashMap<u32, Vec<Arc<dyn Session>>> {
    let mut index: HashMap<u32, Vec<Arc<dyn Session>>> = HashMap::new();
    for session in sessions {
        index.entry(session.pid()).or_default().push(session);
    }
    index
}

/// Keep the compiler honest about the interface imports we rely on.
#[allow(dead_code)]
fn _assert_types(_: Option<IAudioSessionControl>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A session that only remembers: nothing on the machine is really muted.
    struct Fake {
        pid: u32,
        id: String,
        muted: Rc<Cell<bool>>,
    }

    impl Session for Fake {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn id(&self) -> String {
            self.id.clone()
        }

        fn is_muted(&self) -> Option<bool> {
            Some(self.muted.get())
        }

        fn set_muted(&self, muted: bool) -> Result<(), String> {
            self.muted.set(muted);
            Ok(())
        }
    }

    /// A ledger entry for a session that only remembers.
    fn fake_muted(pid: u32, was_muted: bool) -> Muted {
        Muted {
            pid,
            was_muted,
            session: Arc::new(Fake {
                pid,
                id: format!("fake-{pid}"),
                muted: Rc::new(Cell::new(was_muted)),
            }),
        }
    }

    #[test]
    fn the_ledger_counts_the_apps_it_suppresses() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.affected_apps(), 0, "an unused ledger suppresses nothing");
        assert!(!ledger.is_active());

        // two windows of one app and one session of another: two apps
        ledger.hidden.push((10, 42));
        ledger.hidden.push((11, 42));
        ledger.muted.push(fake_muted(42, false));
        ledger.muted.push(fake_muted(43, false));
        assert_eq!(ledger.affected_apps(), 2);
        assert_eq!(ledger.hidden.len(), 2, "the windows are counted on their own");
        assert_eq!(ledger.muted.len(), 2, "and so are the muted sessions");
    }

    #[test]
    fn a_session_the_user_had_muted_is_not_counted_as_suppressed() {
        // The ledger holds every session of a tracked app, including the ones the user
        // had muted already: the killswitch did not change those, so they are not
        // something it suppresses.
        let mut ledger = Ledger::default();
        ledger.muted.push(fake_muted(42, true)); // the user had muted it
        ledger.muted.push(fake_muted(43, false)); // the killswitch muted it
        assert_eq!(
            ledger.affected_apps(),
            1,
            "only the app the killswitch muted is suppressed"
        );
    }

    #[test]
    fn a_released_killswitch_suppresses_nothing() {
        let mut ledger = Ledger::default();
        ledger.hidden.push((10, 42));
        ledger.muted.push(fake_muted(42, false));
        ledger.active = true;
        assert!(ledger.is_active());

        // what `disengage` does: the windows are shown and the sessions unmuted again
        ledger.hidden.clear();
        ledger.muted.clear();
        ledger.active = false;
        assert_eq!(ledger.affected_apps(), 0);
        assert!(!ledger.is_active());
    }

    #[test]
    fn engaging_many_apps_sweeps_the_sessions_once() {
        // The sweep walks every audio session on the machine, so it must happen once for
        // the whole killswitch — not once per tracked app.
        let mut ledger = Ledger::default();
        let sweeps = Cell::new(0);
        let pids = [11u32, 22, 33, 44];
        engage_with(&pids, &mut ledger, || {
            sweeps.set(sweeps.get() + 1);
            Vec::new()
        });
        assert_eq!(
            sweeps.get(),
            1,
            "the sessions must be swept once, not once per tracked app"
        );
    }

    #[test]
    fn the_release_needs_no_sweep() {
        // The ledger holds the sessions, so the release puts them back without walking
        // the machine again.
        let mut ledger = Ledger::default();
        let sweeps = Cell::new(0);
        let pids = [11u32, 22];
        engage_with(&pids, &mut ledger, || {
            sweeps.set(sweeps.get() + 1);
            Vec::new()
        });
        let after_engage = sweeps.get();
        disengage(&mut ledger);
        assert_eq!(
            sweeps.get(),
            after_engage,
            "the release must not sweep the sessions again"
        );
    }

    #[test]
    fn the_ledger_restores_each_session_to_the_state_it_was_in() {
        // One session that was playing, and one the user had muted already.
        let playing = Rc::new(Cell::new(false));
        let muted_by_the_user = Rc::new(Cell::new(true));
        let mut ledger = Ledger::default();
        engage_with(&[42], &mut ledger, || {
            vec![
                Arc::new(Fake {
                    pid: 42,
                    id: "fake-playing".into(),
                    muted: Rc::clone(&playing),
                }) as Arc<dyn Session>,
                Arc::new(Fake {
                    pid: 42,
                    id: "fake-muted".into(),
                    muted: Rc::clone(&muted_by_the_user),
                }) as Arc<dyn Session>,
            ]
        });

        // the killswitch muted the one that was playing and left the other alone
        assert!(playing.get(), "the session that was playing must be muted");
        assert!(
            muted_by_the_user.get(),
            "the session the user had muted stays muted"
        );

        // and the release puts both back the way they were
        disengage(&mut ledger);
        assert!(
            !playing.get(),
            "the session that was playing must be unmuted again"
        );
        assert!(
            muted_by_the_user.get(),
            "the session the user had muted must stay muted"
        );
    }
}
