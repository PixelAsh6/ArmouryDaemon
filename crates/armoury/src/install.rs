//! The daemon installs itself: the autostart task it registers for itself and the
//! first-run marker that decides whether the dashboard opens itself.
//!
//! The scheduled task is the same one `deploy/setup.ps1` registers: the same name, the
//! same action (`<exe> --autostart`), the same triggers (logon and resume from sleep)
//! and the same settings (highest privileges). The script stays authoritative when it
//! runs; this module is what makes the executable usable on its own, without the script.
//!
//! The Windows API is used directly (the Task Scheduler COM API through the `windows`
//! crate), so nothing is shelled out to and the task is defined in one place only.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use log::{error, info, warn};
use windows::core::{BSTR, HRESULT, Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, VARIANT_BOOL};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::TaskScheduler::{
    CLSID_CTaskScheduler, IEventTrigger, IExecAction, ILogonTrigger, IPrincipal, ITaskFolder,
    ITaskService, ITaskSettings, TASK_ACTION_EXEC, TASK_CREATE_OR_UPDATE,
    TASK_INSTANCES_IGNORE_NEW, TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_HIGHEST,
    TASK_TRIGGER_EVENT, TASK_TRIGGER_LOGON,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, WaitForSingleObject};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// The scheduled task the daemon registers for itself. `deploy/setup.ps1` registers the
/// same task under the same name — the two have to agree, and a test keeps them in step.
pub const TASK_NAME: &str = "ArmouryDaemon";

/// What the task passes to the executable: the daemon starts without opening the
/// dashboard, so a resume from sleep never disturbs the user.
pub const TASK_ARGUMENTS: &str = "--autostart";

/// The resume-from-sleep trigger. Task Scheduler has no resume trigger, so it is the
/// Kernel-Power event 107 (the machine resumed from sleep) as an event trigger.
pub const RESUME_SUBSCRIPTION: &str = "<QueryList><Query Id=\"0\" Path=\"System\"><Select Path=\"System\">*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and EventID=107]]</Select></Query></QueryList>";

/// The file that remembers that the dashboard has been shown once. Its absence means
/// "the daemon has not been set up yet", its presence means "leave the user alone".
pub const FIRST_RUN_MARKER: &str = "first-run.done";

/// The answer the task scheduler gives when there is no task under that name:
/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`. Every other error means the task is
/// there and cannot be read - which is not the same as "there is no task".
const TASK_NOT_FOUND: HRESULT = HRESULT::from_win32(2);

/// The flag the elevated copy of the daemon is started with: it registers the task and
/// exits, it does not become the daemon.
pub const INSTALL_FLAG: &str = "--install";

/// The application manifest, relative to the crate root. The build embeds it as the
/// executable's `RT_MANIFEST` resource (resource type 24, id 1 - the id the loader looks
/// for), and it asks Windows for administrator rights. So a build with the manifest is
/// elevated from the first double-click; a build without it (no Windows SDK at build time)
/// is not, and the daemon says so in the log.
pub const MANIFEST: &str = "assets/ArmouryDaemon.manifest";

// ---------------------------------------------------------------------------
// The decision (pure)
// ---------------------------------------------------------------------------

/// What is registered under the task name right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Existing {
    /// Nothing is registered under the task name.
    None,
    /// The task is ours: it runs this executable. `arguments` is what it passes to it.
    Ours { arguments: String },
    /// The task belongs to something else: its action runs another program.
    Foreign { path: String },
}

/// What the daemon should do about the autostart task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to do: the task is already registered and correct.
    Nothing,
    /// A task with our name belongs to something else. It is never touched.
    LeaveAlone,
    /// Register the task now: this process may do it.
    Register,
    /// Register the task, but it needs administrator rights: an elevated copy of this
    /// daemon has to do it.
    Elevate,
}

/// What the daemon does about the autostart task. Pure: the same input always gives
/// the same plan, and a task that belongs to something else is never touched.
pub fn decide(existing: &Existing, elevated: bool) -> Plan {
    match existing {
        // Someone else's task: never touched, whoever is asking.
        Existing::Foreign { .. } => Plan::LeaveAlone,
        Existing::None => {
            if elevated {
                Plan::Register
            } else {
                Plan::Elevate
            }
        }
        Existing::Ours { arguments } => {
            if arguments.trim() == TASK_ARGUMENTS {
                Plan::Nothing
            } else if elevated {
                // Ours, but it runs the daemon the wrong way: fix it.
                Plan::Register
            } else {
                Plan::Elevate
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The first-run marker
// ---------------------------------------------------------------------------

/// Where the marker lives: next to the config.
pub fn first_run_marker() -> PathBuf {
    armoury_core::config::config_dir().join(FIRST_RUN_MARKER)
}

/// True when the dashboard has never been shown: the daemon has not been set up yet.
pub fn is_first_run(marker: &Path) -> bool {
    !marker.exists()
}

/// Remember that the dashboard has been shown. It is written once and never removed, so
/// the dashboard does not come back on every start and closing it is final.
pub fn mark_first_run_done(marker: &Path) -> std::io::Result<()> {
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(marker, b"ArmouryDaemon: the dashboard has been shown.\n")
}

// ---------------------------------------------------------------------------
// The Windows side
// ---------------------------------------------------------------------------

/// True when this daemon is a throw-away test instance (`ARMOURY_TEST_NS`). Such an
/// instance never touches the machine: no scheduled task, no install.
pub fn is_test_instance() -> bool {
    std::env::var("ARMOURY_TEST_NS")
        .map(|ns| !ns.trim().is_empty())
        .unwrap_or(false)
}

/// Is this process running with an elevated token? Registering a task that runs with
/// the highest privileges needs administrator rights, so this decides who registers it.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut read = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut read,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// The task scheduler service and the root folder, ready to use.
fn service() -> Result<(ITaskService, ITaskFolder), String> {
    unsafe {
        // COM for this thread. It may already be up (the daemon initialises it for the
        // audio interfaces); the second call is a no-op then.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let service: ITaskService =
            CoCreateInstance::<_, ITaskService>(&CLSID_CTaskScheduler, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("the task scheduler is not reachable ({e})"))?;
        let folder = service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|e| format!("the task folder is not reachable ({e})"))?;
        Ok((service, folder))
    }
}

/// What is registered under the task name right now.
pub fn read_existing() -> Result<Existing, String> {
    let (_service, folder) = service()?;
    unsafe {
        // The task lives in the root folder, so its name is the path. The full path is
        // tried as well, for a scheduler that wants it that way.
        let full = format!("\\{TASK_NAME}");
        let task = match folder.GetTask(&BSTR::from(TASK_NAME)) {
            Ok(task) => task,
            Err(first) => match folder.GetTask(&BSTR::from(full.as_str())) {
                Ok(task) => task,
                Err(second) => {
                    let existing = missing_or_unreadable(first.code(), second.code());
                    if let Existing::Foreign { .. } = existing {
                        warn!(
                            "the task '{TASK_NAME}' cannot be read ({}, {}) - it is treated as a task of \
                             somebody else's and is left alone",
                            first.code(),
                            second.code()
                        );
                    }
                    return Ok(existing);
                }
            },
        };
        let definition = task
            .Definition()
            .map_err(|e| format!("the task '{TASK_NAME}' cannot be read ({e})"))?;
        let actions = definition
            .Actions()
            .map_err(|e| format!("the task '{TASK_NAME}' has no readable action ({e})"))?;
        let mut count = 0i32;
        actions
            .Count(&mut count)
            .map_err(|e| format!("the task '{TASK_NAME}' has no readable action ({e})"))?;
        if count == 0 {
            return Ok(Existing::Foreign {
                path: String::new(),
            });
        }
        let action = actions
            .get_Item(1)
            .map_err(|e| format!("the task '{TASK_NAME}' has no readable action ({e})"))?;
        let exec: IExecAction = action
            .cast()
            .map_err(|_| format!("the task '{TASK_NAME}' does not run a program"))?;
        let mut path = BSTR::default();
        exec.Path(&mut path)
            .map_err(|e| format!("the task '{TASK_NAME}' has no readable action ({e})"))?;
        let mut arguments = BSTR::default();
        let _ = exec.Arguments(&mut arguments).ok();

        let path = path.to_string();
        let arguments = arguments.to_string();
        Ok(match our_executable() {
            Some(exe) if same_path(&path, &exe) => Existing::Ours { arguments },
            _ => Existing::Foreign { path },
        })
    }
}

/// What the two failed ways of asking for the task mean.
///
/// Only "there is no such task" on both of them means the name is free. Any other
/// answer (the task is there and cannot be read - access denied, a task that will not
/// open) means there is a task under that name, and it is not ours to touch: it is
/// somebody else's, and it is never registered over.
fn missing_or_unreadable(first: HRESULT, second: HRESULT) -> Existing {
    if first == TASK_NOT_FOUND && second == TASK_NOT_FOUND {
        Existing::None
    } else {
        Existing::Foreign {
            path: String::new(),
        }
    }
}

/// Register the task for this executable: the same one `deploy/setup.ps1` registers.
pub fn register(exe: &Path) -> Result<(), String> {
    let (service, folder) = service()?;
    let work_dir = exe.parent().unwrap_or(Path::new("."));
    let exe_text = exe.to_string_lossy().to_string();
    let dir_text = work_dir.to_string_lossy().to_string();
    let user = current_user();
    let sid = crate::daemon::current_user_sid();

    unsafe {
        let definition = service
            .NewTask(0)
            .map_err(|e| format!("the task definition cannot be created ({e})"))?;

        // The description says what the task is. The installer adds the hash of the
        // executable to it — it is the one that knows it.
        if let Ok(info) = definition.RegistrationInfo() {
            let _ = info.SetDescription(&BSTR::from(
                "ArmouryDaemon autostart. The daemon registered this task for itself.",
            ));
        }

        // The action: this executable, with --autostart, in its own folder.
        let actions = definition
            .Actions()
            .map_err(|e| format!("the task action cannot be created ({e})"))?;
        let action = actions
            .Create(TASK_ACTION_EXEC)
            .map_err(|e| format!("the task action cannot be created ({e})"))?;
        let exec: IExecAction = action
            .cast()
            .map_err(|_| "the task action cannot be created".to_string())?;
        exec.SetPath(&BSTR::from(exe_text.as_str()))
            .map_err(|e| format!("the task action cannot be created ({e})"))?;
        exec.SetArguments(&BSTR::from(TASK_ARGUMENTS))
            .map_err(|e| format!("the task action cannot be created ({e})"))?;
        exec.SetWorkingDirectory(&BSTR::from(dir_text.as_str()))
            .map_err(|e| format!("the task action cannot be created ({e})"))?;

        // The triggers: every logon of this user, and every resume from sleep.
        let triggers = definition
            .Triggers()
            .map_err(|e| format!("the task triggers cannot be created ({e})"))?;
        let logon = triggers
            .Create(TASK_TRIGGER_LOGON)
            .map_err(|e| format!("the logon trigger cannot be created ({e})"))?;
        logon.SetEnabled(VARIANT_BOOL(-1))
            .map_err(|e| format!("the logon trigger cannot be created ({e})"))?;
        let logon: ILogonTrigger = logon
            .cast()
            .map_err(|_| "the logon trigger cannot be created".to_string())?;
        logon
            .SetUserId(&BSTR::from(user.as_str()))
            .map_err(|e| format!("the logon trigger cannot be created ({e})"))?;

        let resume = triggers
            .Create(TASK_TRIGGER_EVENT)
            .map_err(|e| format!("the resume trigger cannot be created ({e})"))?;
        resume.SetEnabled(VARIANT_BOOL(-1))
            .map_err(|e| format!("the resume trigger cannot be created ({e})"))?;
        let resume: IEventTrigger = resume
            .cast()
            .map_err(|_| "the resume trigger cannot be created".to_string())?;
        resume
            .SetSubscription(&BSTR::from(RESUME_SUBSCRIPTION))
            .map_err(|e| format!("the resume trigger cannot be created ({e})"))?;

        // The principal: this user, in the interactive session, with the highest
        // privileges — the daemon has to be able to do what the packs ask of it.
        let principal = definition
            .Principal()
            .map_err(|e| format!("the task principal cannot be set ({e})"))?;
        let who = sid.as_deref().unwrap_or(user.as_str());
        set_principal(&principal, who)?;

        // The settings: the same ones the installer uses.
        let settings = definition
            .Settings()
            .map_err(|e| format!("the task settings cannot be set ({e})"))?;
        set_settings(&settings)?;

        folder
            .RegisterTaskDefinition(
                &BSTR::from(TASK_NAME),
                &definition,
                TASK_CREATE_OR_UPDATE.0,
                &VARIANT::from(who),
                &VARIANT::default(),
                TASK_LOGON_INTERACTIVE_TOKEN,
                &VARIANT::default(),
            )
            .map_err(|e| format!("the task '{TASK_NAME}' cannot be registered ({e})"))?;
    }
    Ok(())
}

/// The principal of the task: this user, in the interactive session, with the highest
/// privileges — the daemon has to be able to do what the packs ask of it.
fn set_principal(principal: &IPrincipal, user: &str) -> Result<(), String> {
    unsafe {
        principal
            .SetUserId(&BSTR::from(user))
            .map_err(|e| format!("the task principal cannot be set ({e})"))?;
        principal
            .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
            .map_err(|e| format!("the task principal cannot be set ({e})"))?;
        principal
            .SetRunLevel(TASK_RUNLEVEL_HIGHEST)
            .map_err(|e| format!("the task principal cannot be set ({e})"))?;
    }
    Ok(())
}

/// The settings of the task: the same ones the installer uses (the machine may run on
/// battery, the task starts as soon as it can, a second instance is ignored, and it
/// never times out).
fn set_settings(settings: &ITaskSettings) -> Result<(), String> {
    unsafe {
        settings
            .SetDisallowStartIfOnBatteries(VARIANT_BOOL(0))
            .map_err(|e| format!("the task settings cannot be set ({e})"))?;
        settings
            .SetStopIfGoingOnBatteries(VARIANT_BOOL(0))
            .map_err(|e| format!("the task settings cannot be set ({e})"))?;
        settings
            .SetStartWhenAvailable(VARIANT_BOOL(-1))
            .map_err(|e| format!("the task settings cannot be set ({e})"))?;
        settings
            .SetExecutionTimeLimit(&BSTR::from("PT0S"))
            .map_err(|e| format!("the task settings cannot be set ({e})"))?;
        settings
            .SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)
            .map_err(|e| format!("the task settings cannot be set ({e})"))?;
    }
    Ok(())
}

/// Ask Windows for administrator rights and let an elevated copy of this executable
/// register the task. The call blocks while the consent prompt is on screen.
pub fn register_via_elevated_copy(exe: &Path) -> Result<(), String> {
    let verb: Vec<u16> = wide("runas");
    let file: Vec<u16> = wide(&exe.to_string_lossy().to_string());
    let arguments: Vec<u16> = wide(INSTALL_FLAG);
    let directory: Vec<u16> = wide(
        &exe
            .parent()
            .unwrap_or(Path::new("."))
            .to_string_lossy()
            .to_string(),
    );

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    // NOCLOSEPROCESS: the handle of the elevated copy, so it can be waited for.
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = PCWSTR(verb.as_ptr());
    info.lpFile = PCWSTR(file.as_ptr());
    info.lpParameters = PCWSTR(arguments.as_ptr());
    info.lpDirectory = PCWSTR(directory.as_ptr());
    info.nShow = SW_HIDE.0;

    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| {
            format!(
                "Windows did not start the elevated copy ({e}) — the administrator prompt was declined?"
            )
        })?;
        // Wait for the elevated copy: it registers the task and exits.
        let _ = WaitForSingleObject(info.hProcess, u32::MAX);
        let _ = CloseHandle(info.hProcess);
    }
    Ok(())
}

/// This executable's own path.
pub fn our_executable() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// The user this daemon runs as, the way the installer writes it: `DOMAIN\user`.
fn current_user() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return crate::daemon::current_user_name().unwrap_or_default();
    }
    if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    }
}

/// Two paths are the same file when they name the same file: the comparison is on the
/// canonical paths, so a short name (`PROGRA~1`) and a long one still match.
fn same_path(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

/// A null-terminated UTF-16 string for the Win32 calls.
fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect()
}

// ---------------------------------------------------------------------------
// The daemon's side
// ---------------------------------------------------------------------------

/// Look after the autostart task: register it when it is missing and this daemon may.
/// Never touches a task that belongs to something else, and does nothing at all when
/// the task is already there.
///
/// The log says which way it went: a daemon that is already elevated registers the task
/// itself, an unelevated one asks Windows for administrator rights and lets an elevated
/// copy do it.
pub fn check_and_register(exe: &Path) {
    let elevated = is_elevated();
    if elevated {
        info!(
            "autostart: this daemon is already elevated - it can hide the windows of other apps \
             and it can register the scheduled task itself"
        );
    } else {
        info!(
            "autostart: this daemon is NOT elevated - the killswitch cannot hide the windows \
             of apps that run as administrator, so it is not fully functional in this session. \
             The autostart task runs with the highest privileges, so the next logon is fully \
             elevated"
        );
    }

    let existing = match read_existing() {
        Ok(existing) => existing,
        Err(e) => {
            warn!("autostart: the scheduled task could not be read ({e})");
            return;
        }
    };
    let plan = decide(&existing, elevated);
    match plan {
        Plan::Nothing => {
            info!("autostart: the scheduled task '{TASK_NAME}' is already registered for this executable");
        }
        Plan::LeaveAlone => {
            // A task that could not be read has no path to name: it is simply "another
            // program's task".
            let other = match &existing {
                Existing::Foreign { path } if !path.is_empty() => path.as_str(),
                _ => "another program",
            };
            warn!(
                "autostart: a scheduled task called '{TASK_NAME}' exists but runs {other} — it was left \
                 alone, and this daemon will not start at logon"
            );
        }
        Plan::Register => match register(exe) {
            Ok(()) => info!(
                "autostart: the scheduled task '{TASK_NAME}' is registered by this elevated daemon \
                 (logon + resume from sleep, highest privileges, {TASK_ARGUMENTS})"
            ),
            Err(e) => warn!("autostart: the scheduled task could not be registered ({e})"),
        },
        Plan::Elevate => {
            info!(
                "autostart: there is no scheduled task for this executable yet - this daemon is not \
                 elevated (a build without the embedded manifest '{MANIFEST}'), so Windows is asked \
                 for administrator rights: a consent prompt appears and an elevated copy of this \
                 daemon registers the task. This session stays unelevated until the next logon"
            );
            let exe = exe.to_path_buf();
            // The consent prompt blocks until the user answers it, so it runs on its own
            // thread: the daemon comes up either way.
            std::thread::spawn(move || match register_via_elevated_copy(&exe) {
                Ok(()) => match read_existing() {
                    Ok(Existing::Ours { .. }) => info!(
                        "autostart: the scheduled task '{TASK_NAME}' is registered (logon + resume from \
                         sleep, highest privileges, {TASK_ARGUMENTS})"
                    ),
                    Ok(_) => warn!("autostart: the scheduled task was not registered"),
                    Err(e) => warn!("autostart: the scheduled task could not be read ({e})"),
                },
                Err(e) => warn!("autostart: the scheduled task was not registered ({e})"),
            });
        }
    }
}

/// The `--install` mode: the elevated copy of the daemon. It registers the task and
/// exits; it does not become the daemon.
pub fn install_self() {
    if let Err(e) = armoury_core::logging::init("daemon") {
        eprintln!("logging init failed: {e}");
    }
    if is_elevated() {
        info!("install: this copy is elevated - it registers the task and exits");
    } else {
        warn!(
            "install: this copy is NOT elevated - the task cannot be registered with the highest \
             privileges, and the daemon that started it stays unelevated"
        );
    }
    let Some(exe) = our_executable() else {
        error!("install: the path of this executable cannot be resolved");
        std::process::exit(1);
    };
    info!("install: registering the scheduled task '{TASK_NAME}' for {exe:?}");
    match register(&exe) {
        Ok(()) => {
            info!(
                "install: the scheduled task '{TASK_NAME}' is registered (logon + resume from sleep, \
                 highest privileges, {TASK_ARGUMENTS})"
            );
        }
        Err(e) => {
            error!("install: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The installer script is the reference for the task: the daemon and the script
    /// have to name the same task, with the same action and the same triggers.
    fn setup_script() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("deploy")
            .join("setup.ps1");
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// A file of the crate, by its path relative to the crate root.
    fn crate_file(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
    }

    #[test]
    fn the_manifest_asks_windows_for_administrator_rights() {
        // The manifest is what makes the executable elevated from the first double-click.
        // A bad edit of it has to fail loudly, not quietly build an unelevated daemon.
        let path = crate_file(MANIFEST);
        assert!(path.is_file(), "the manifest must exist: {}", path.display());
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            text.contains("requestedExecutionLevel"),
            "the manifest must ask for an execution level: {text}"
        );
        assert!(
            text.contains("level=\"requireAdministrator\""),
            "the executable must ask for administrator rights: {text}"
        );
        assert!(
            text.contains("uiAccess=\"false\""),
            "the daemon is not an input tool: {text}"
        );
        assert!(
            text.contains("manifestVersion=\"1.0\""),
            "the assembly must be a 1.0 manifest: {text}"
        );
        assert!(
            text.contains("urn:schemas-microsoft-com:asm.v1"),
            "the assembly element must be in the asmv1 namespace: {text}"
        );
    }

    #[test]
    fn the_build_embeds_the_manifest_beside_the_icon() {
        // The icon group is RT_GROUP_ICON (type 14) and the manifest is RT_MANIFEST (type
        // 24); the ids are counted per type, so both can sit under id 1 - and id 1 is where
        // the loader looks for the manifest.
        let path = crate_file("build.rs");
        let build = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(build.contains("1 ICON"), "the icon must stay in the resource script");
        assert!(
            build.contains("1 24"),
            "the manifest must be added as RT_MANIFEST (24) under id 1"
        );
        assert!(build.contains(MANIFEST), "the build must embed the manifest of the crate");
    }

    #[test]
    fn the_task_has_the_name_the_installer_uses() {
        assert_eq!(TASK_NAME, "ArmouryDaemon");
        let script = setup_script();
        assert!(
            script.contains(&format!("$taskName = '{TASK_NAME}'")),
            "the installer must register the same task name"
        );
    }

    #[test]
    fn the_action_is_the_one_the_installer_uses() {
        assert_eq!(TASK_ARGUMENTS, "--autostart");
        let script = setup_script();
        assert!(
            script.contains(&format!("-Argument '{TASK_ARGUMENTS}'")),
            "the installer must pass the same argument"
        );
        assert!(
            script.contains("-RunLevel Highest"),
            "the installer must run the task with the highest privileges"
        );
        assert!(
            script.contains("-LogonType Interactive"),
            "the installer must run the task in the interactive session"
        );
    }

    #[test]
    fn the_resume_trigger_is_the_one_the_installer_uses() {
        let script = setup_script();
        assert!(
            script.contains("Microsoft-Windows-Kernel-Power") && script.contains("EventID=107"),
            "the installer must resume on the Kernel-Power event 107"
        );
        assert!(
            RESUME_SUBSCRIPTION.contains("Microsoft-Windows-Kernel-Power")
                && RESUME_SUBSCRIPTION.contains("EventID=107"),
            "the daemon must resume on the same event"
        );
    }

    #[test]
    fn a_missing_task_is_registered_when_the_rights_are_there() {
        assert_eq!(decide(&Existing::None, true), Plan::Register);
    }

    #[test]
    fn a_missing_task_needs_administrator_rights_without_them() {
        assert_eq!(decide(&Existing::None, false), Plan::Elevate);
    }

    #[test]
    fn a_task_that_is_already_ours_is_left_alone() {
        let ours = Existing::Ours {
            arguments: TASK_ARGUMENTS.to_string(),
        };
        assert_eq!(decide(&ours, true), Plan::Nothing);
        assert_eq!(decide(&ours, false), Plan::Nothing);
    }

    #[test]
    fn a_task_of_ours_with_the_wrong_argument_is_fixed() {
        let ours = Existing::Ours {
            arguments: String::new(),
        };
        assert_eq!(decide(&ours, true), Plan::Register);
        assert_eq!(decide(&ours, false), Plan::Elevate);
    }

    #[test]
    fn a_task_that_belongs_to_someone_else_is_never_touched() {
        let foreign = Existing::Foreign {
            path: "C:\\other\\thing.exe".to_string(),
        };
        assert_eq!(decide(&foreign, true), Plan::LeaveAlone);
        assert_eq!(decide(&foreign, false), Plan::LeaveAlone);
    }

    #[test]
    fn a_task_that_cannot_be_read_is_never_taken_for_an_absent_one() {
        // Both ways of asking for the task say "there is no such task": the name is free.
        assert_eq!(
            missing_or_unreadable(TASK_NOT_FOUND, TASK_NOT_FOUND),
            Existing::None
        );
        // The task is there and cannot be read (access denied, a task that will not
        // open). It is somebody else's, and it is never registered over.
        let denied = HRESULT::from_win32(5);
        assert_eq!(
            missing_or_unreadable(TASK_NOT_FOUND, denied),
            Existing::Foreign {
                path: String::new()
            }
        );
        assert_eq!(
            missing_or_unreadable(denied, TASK_NOT_FOUND),
            Existing::Foreign {
                path: String::new()
            }
        );
        // And the plan for it is the same as for any other task of somebody else.
        let foreign = missing_or_unreadable(denied, denied);
        assert_eq!(decide(&foreign, true), Plan::LeaveAlone);
        assert_eq!(decide(&foreign, false), Plan::LeaveAlone);
    }

    #[test]
    fn the_first_run_marker_is_written_once() {
        let dir = std::env::temp_dir().join(format!(
            "armoury-first-run-{}-{}",
            std::process::id(),
            FIRST_RUN_MARKER
        ));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join(FIRST_RUN_MARKER);
        assert!(is_first_run(&marker), "a daemon that never ran is a first run");
        mark_first_run_done(&marker).expect("the marker is written");
        assert!(marker.exists());
        assert!(!is_first_run(&marker), "the marker ends the first run");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_file_is_the_same_path() {
        let dir = std::env::temp_dir().join(format!("armoury-same-path-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ArmouryDaemon.exe");
        fs::write(&file, b"x").unwrap();
        assert!(same_path(&file.to_string_lossy(), &file));
        assert!(same_path(
            &dir.join(".").join("ArmouryDaemon.exe").to_string_lossy(),
            &file
        ));
        assert!(!same_path(&dir.join("other.exe").to_string_lossy(), &file));
        let _ = fs::remove_dir_all(&dir);
    }
}
