//! Spec 2 — scripted packs (Rhai).
//!
//! A pack may carry a script in `script.apply` / `script.revert`. The **pack never executes it**:
//! the daemon compiles the source and runs it in one Rhai engine per thread — built once and
//! kept between runs — with hard limits, and exposes only the functions registered below.
//! Nothing else exists inside a script — no filesystem, network or process API beyond these,
//! no `eval`, no imports.
//!
//! Every host call is appended to the run's log (which the daemon writes to the pack journal).
//! Writes are restricted to the usual config locations and the target's own folder (a write may
//! create the folders it needs inside them), and a script may only run programs from those same
//! places.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use armoury_core::pack::{Capability, CapSet, TemplateCtx};
use log::info;
use rhai::{Array, AST, Dynamic, Engine, EvalAltResult, ImmutableString, Scope};

/// What a script can see and do. Fields are private to the host functions.
#[derive(Debug, Clone)]
pub struct ScriptHost {
    target_name: String,
    target_exe: String,
    target_dir: String,
    settings: HashMap<String, String>,
    captured: HashMap<String, String>,
    /// Human-readable record of every host call (goes to the journal / log).
    log: Vec<String>,
}

impl ScriptHost {
    fn note(&mut self, line: impl Into<String>) {
        let line = line.into();
        info!("script: {line}");
        self.log.push(line);
    }
}

fn err(msg: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        msg.into().into(),
        rhai::Position::NONE,
    ))
}

/// How many operations a script may perform in one run.
const MAX_OPERATIONS: u64 = 500_000;

/// How long a script may run in one run: a pack may not hold the pack worker hostage.
const WALL_BUDGET: Duration = Duration::from_secs(10);

/// The budget as it is written down: "10 s" or "300 ms".
fn budget_text(budget: Duration) -> String {
    if budget.as_secs() >= 1 {
        format!("{} s", budget.as_secs())
    } else {
        format!("{} ms", budget.as_millis())
    }
}

/// The message the wall clock guard stops a script with.
fn budget_message(budget: Duration) -> String {
    format!("script exceeded its {} budget", budget_text(budget))
}

#[cfg(test)]
thread_local! {
    /// How many engines this thread has built. A run must not build one.
    static BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// Compiled scripts, keyed by their source. A pack's script never changes, but it is run
    /// again on every apply and every revert.
    static ASTS: RefCell<HashMap<String, Rc<AST>>> = RefCell::new(HashMap::new());
    /// **One engine per capability set.** A pack gets an engine on which only the functions
    /// it was granted are registered, so a call to a function it was not granted fails as
    /// an *unknown function* — a structural denial, not a check a call site could forget.
    ///
    /// The engines are built lazily and kept: building one and registering its host
    /// functions costs about half a millisecond, and the pack worker is a single serial
    /// thread, so no locking is needed. The map is capped like the AST cache.
    static ENGINES: RefCell<HashMap<CapSet, Sandbox>> = RefCell::new(HashMap::new());
}

/// How many engines this thread keeps. The AST cache has the same cap: a pack that was
/// replaced or removed starts over.
const MAX_ENGINES: usize = 32;

/// The sandbox: the engine, and the two budgets it enforces. The budgets are **per run**, so
/// they live next to the engine and are re-armed before every evaluation — never once, at
/// build time.
struct Sandbox {
    engine: Engine,
    /// The wall-clock deadline of the run in progress, in milliseconds since `base`. The
    /// progress callback reads it, so the engine and the run share it.
    deadline: Arc<AtomicU64>,
    /// The wall-clock budget of the run in progress, in milliseconds.
    budget: Arc<AtomicU64>,
    /// The zero point the deadline is counted from.
    base: Instant,
}

impl Sandbox {
    /// Re-arm both budgets for a run that is about to start: the operation budget is the
    /// engine's limit, the wall clock the deadline the progress callback watches.
    ///
    /// Re-arming the operation budget is belt and braces: Rhai builds its runtime state
    /// per evaluation, so the operation count resets on its own. Re-arming the wall clock
    /// is the load-bearing one: the deadline lives in a shared cell, so without it the
    /// next run would inherit the previous run's expired deadline and be killed on its
    /// first tick.
    fn arm(&mut self, budget: Duration) {
        self.engine.set_max_operations(MAX_OPERATIONS);
        let millis = budget.as_millis() as u64;
        self.budget.store(millis, Ordering::Relaxed);
        let now = self.base.elapsed().as_millis() as u64;
        self.deadline.store(now + millis, Ordering::Relaxed);
    }
}

/// The AST of a script, compiled once and kept for the next run.
///
/// The AST does not belong to an engine — the functions it calls are resolved when it
/// runs — so one cache serves every capability set.
fn compile_ast(source: &str, caps: CapSet) -> Result<Rc<AST>, String> {
    let cached = ASTS.with(|cache| cache.borrow().get(source).cloned());
    if let Some(ast) = cached {
        return Ok(ast);
    }
    let ast = Rc::new(
        with_engine(caps, |sandbox| sandbox.engine.compile(source))
            .map_err(|e| format!("script error: {e}"))?,
    );
    ASTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= 32 {
            cache.clear(); // a pack was replaced or removed: start over
        }
        cache.insert(source.to_string(), ast.clone());
    });
    Ok(ast)
}

/// The engine for a capability set: built once, kept, and re-armed before every run.
fn with_engine<T>(caps: CapSet, f: impl FnOnce(&mut Sandbox) -> T) -> T {
    ENGINES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !cache.contains_key(&caps) {
            if cache.len() >= MAX_ENGINES {
                cache.clear(); // a pack was replaced or removed: start over
            }
            cache.insert(caps, new_sandbox(caps));
        }
        let sandbox = cache
            .get_mut(&caps)
            .expect("the engine for this capability set was just built");
        f(sandbox)
    })
}

/// Compile a script once (used to reject broken packs at load/install time).
pub fn compile(source: &str) -> Result<(), String> {
    compile_ast(source, CapSet::ALL).map(|_| ())
}

/// Run one block script with the whole host API. Returns the host-call log; `ctx.captured`
/// receives anything the script captured (so a revert can replay it).
///
/// The daemon does not use this: a pack runs on the engine of its own capability set (see
/// [`run_with_capabilities`]). It is here for the tests, which run scripts without a
/// grant list.
#[cfg(test)]
pub fn run(source: &str, ctx: &mut TemplateCtx) -> Result<Vec<String>, String> {
    run_with_budget(source, ctx, WALL_BUDGET, CapSet::ALL)
}

/// Run one block script with the capabilities the pack was granted. Everything else is
/// missing from the engine, so a call to it fails as an unknown function.
pub fn run_with_capabilities(
    source: &str,
    ctx: &mut TemplateCtx,
    caps: CapSet,
) -> Result<Vec<String>, String> {
    run_with_budget(source, ctx, WALL_BUDGET, caps)
}

/// The run itself, with the wall-clock budget it gets. The budget is re-armed on **every**
/// run, so a script can never spend the allowance of the run before it.
fn run_with_budget(
    source: &str,
    ctx: &mut TemplateCtx,
    budget: Duration,
    caps: CapSet,
) -> Result<Vec<String>, String> {
    let ast = compile_ast(source, caps)?;

    let host = ScriptHost {
        target_name: ctx.target_name.clone(),
        target_exe: ctx.target_exe.clone(),
        target_dir: ctx.target_dir.clone(),
        settings: ctx.settings.clone(),
        captured: ctx.captured.clone(),
        log: Vec::new(),
    };

    // The scope is the per-run state: a fresh one every time, so two scripts never see each
    // other's variables. The engine itself keeps no state between evaluations, so there
    // is nothing else to clear.
    let mut scope = Scope::new();
    scope.push("ctx", host.clone());
    let result = with_engine(caps, |sandbox| {
        // A run must never inherit the allowance of the run before it.
        sandbox.arm(budget);
        sandbox.engine.eval_ast_with_scope::<()>(&mut scope, &ast)
    });

    // the captures are harvested even when the script failed: a script that captured and then
    // failed must still leave the journal something to replay
    let mut host: Option<ScriptHost> = scope.get_value("ctx");
    if let Some(host) = &host {
        ctx.captured = host.captured.clone();
    }
    // Rhai drops the token of a terminated script, so the daemon writes the message
    // itself, from the budget it gave the run.
    if let Err(e) = result {
        if matches!(*e, EvalAltResult::ErrorTerminated(..)) {
            return Err(format!(
                "script failed: it ran into its {} budget",
                budget_text(budget)
            ));
        }
        // A call to a function the pack was not granted comes back as "unknown function".
        // The pack is told which capability it needs and what it asked for, and the
        // denial is written to its log like every other host call.
        if let Some(denial) = denial(caps, &e) {
            if let Some(host) = host.as_mut() {
                host.note(denial.clone());
            }
            return Err(format!("script failed: {denial}"));
        }
        return Err(format!("script failed: {e}"));
    }
    let host = host.ok_or_else(|| "script lost its context".to_string())?;
    Ok(host.log)
}

/// Is this error a call to a host function the pack was not granted?
///
/// The engine of a capability set carries only the granted functions, so an ungranted
/// call comes back as an unknown function. A function the pack *was* granted but called
/// wrongly (a wrong argument type) comes back the same way — that is not a denial, and
/// it must not be reported as one.
///
/// The line that comes back is the one the pack's log gets: the denial is a host call
/// like every other one, and it says which capability it needs and what the pack asked
/// for.
fn denial(caps: CapSet, error: &EvalAltResult) -> Option<String> {
    let EvalAltResult::ErrorFunctionNotFound(signature, _) = error else {
        return None;
    };
    // Rhai names the call by its signature: "run_cli (ScriptHost, &str | ...)".
    let name = signature.split(' ').next().unwrap_or(signature);
    let capability = Capability::of_function(name)?;
    if caps.grants(capability) {
        return None; // the function is there: the call itself is wrong
    }
    Some(format!(
        "{name} REFUSED: it needs the '{}' capability, which this pack was not granted (the pack asked for: {})",
        capability.name(),
        asked_for(caps)
    ))
}

/// The capabilities a pack asked for, as a sentence: what a denial message lists.
fn asked_for(caps: CapSet) -> String {
    let names = caps.names();
    if names.is_empty() {
        return "nothing".to_string();
    }
    names.join(", ")
}

/// The sandbox: limits first, then only the host API the pack was granted. It is built
/// once per capability set and kept between runs — see [`ENGINES`].
///
/// The host API is registered per capability: a function of a capability that was not
/// granted is simply **not there**, so a call to it fails as an unknown function. That is
/// the whole point of the gate: the denial cannot be forgotten at a call site, because
/// there is no call site to forget it at.
fn new_sandbox(caps: CapSet) -> Sandbox {
    #[cfg(test)]
    BUILDS.with(|c| c.set(c.get() + 1));

    let mut engine = Engine::new();
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_call_levels(32);
    engine.set_max_expr_depths(64, 32);
    engine.set_max_string_size(64 * 1024);
    engine.set_max_array_size(4096);
    engine.set_max_map_size(256);
    engine.set_max_modules(0);

    // Wall clock guard: a pack may not hold the pack worker hostage. The deadline is read
    // from the shared cell, so it can be re-armed for every run.
    let base = Instant::now();
    // The operation budget is armed here, at build time. The wall clock is not: nothing
    // is armed until a run arms it, and an unarmed wall clock never stops a script.
    let deadline = Arc::new(AtomicU64::new(u64::MAX));
    let budget = Arc::new(AtomicU64::new(0));
    let watch_deadline = Arc::clone(&deadline);
    let watch_budget = Arc::clone(&budget);
    engine.on_progress(move |_ops| {
        let now = base.elapsed().as_millis() as u64;
        if now >= watch_deadline.load(Ordering::Relaxed) {
            let budget = Duration::from_millis(watch_budget.load(Ordering::Relaxed));
            Some(Dynamic::from(ImmutableString::from(budget_message(budget))))
        } else {
            None
        }
    });
    engine.on_print(|line| info!("script print: {line}"));

    engine.register_type_with_name::<ScriptHost>("ctx");

    // ---- the host API, capability by capability ----
    //
    // Only the functions of a granted capability are registered. A function of a
    // capability that was not granted is not on the engine at all, so a call to it fails
    // as an unknown function.

    // ---- read-only context ----
    if caps.grants(Capability::Context) {
        engine.register_fn("target_name", |host: &mut ScriptHost| -> ImmutableString {
            host.target_name.clone().into()
        });
        engine.register_fn("target_exe", |host: &mut ScriptHost| -> ImmutableString {
            host.target_exe.clone().into()
        });
        engine.register_fn("target_dir", |host: &mut ScriptHost| -> ImmutableString {
            host.target_dir.clone().into()
        });
        engine.register_fn("setting", |host: &mut ScriptHost, key: &str| -> ImmutableString {
            host.settings.get(key).cloned().unwrap_or_default().into()
        });
        engine.register_fn("captured", |host: &mut ScriptHost, key: &str| -> ImmutableString {
            host.captured.get(key).cloned().unwrap_or_default().into()
        });
    }

    // ---- the pack's own journal ----
    if caps.grants(Capability::Journal) {
        engine.register_fn("log", |host: &mut ScriptHost, msg: &str| {
            host.note(msg.to_string());
        });
        engine.register_fn("capture", |host: &mut ScriptHost, key: &str, value: &str| {
            host.captured
                .insert(key.to_string(), value.to_string());
            host.note(format!("capture {key} = {value}"));
        });
    }

    // ---- actions (all routed through the same mechanisms the data packs use) ----
    if caps.grants(Capability::Exec) {
        engine.register_fn(
            "run_cli",
            |host: &mut ScriptHost, exe: &str, args: Array| -> Result<(), Box<EvalAltResult>> {
                let args: Vec<String> = args
                    .iter()
                    .map(|a| a.clone().into_string().unwrap_or_default())
                    .collect();
                // a script may only run programs from the same places it may write files: an
                // unrestricted spawn would hand it the whole machine (cmd.exe lives in System32)
                let target_dir = host.target_dir.clone();
                if !exe_allowed(exe, &target_dir) {
                    host.note(format!("run_cli {exe} REFUSED (outside the allowed folders)"));
                    return Err(err(format!(
                        "run_cli refused: {exe} is outside %APPDATA%, %LOCALAPPDATA%, %PROGRAMDATA% and the target's folder"
                    )));
                }
                host.note(format!("run_cli {exe} {}", args.join(" ")));
                crate::packs::script_cli(exe, &args).map_err(err)
            },
        );
    }
    if caps.grants(Capability::Input) {
        engine.register_fn(
            "hotkey",
            |host: &mut ScriptHost, chord: &str| -> Result<(), Box<EvalAltResult>> {
                host.note(format!("hotkey {chord}"));
                crate::packs::send_chord(chord).map_err(err)
            },
        );
    }
    if caps.grants(Capability::Ipc) {
        engine.register_fn(
            "window_message",
            |host: &mut ScriptHost,
             class: &str,
             title: &str,
             payload: &str|
             -> Result<(), Box<EvalAltResult>> {
                host.note(format!("window_message {class} / {title}: {payload}"));
                send_window_message(class, title, payload).map_err(|e| {
                    host.note(format!("window_message {class} REFUSED: {e}"));
                    err(e)
                })
            },
        );
    }
    if caps.grants(Capability::Time) {
        engine.register_fn("sleep", |host: &mut ScriptHost, ms: i64| {
            let ms = ms.clamp(0, 10_000) as u64;
            host.note(format!("sleep {ms} ms"));
            std::thread::sleep(Duration::from_millis(ms));
        });
    }
    if caps.grants(Capability::Process) {
        engine.register_fn("process_running", |host: &mut ScriptHost, name: &str| -> bool {
            let running = crate::packs::script_process_running(name);
            host.note(format!("process_running {name} -> {running}"));
            running
        });
        engine.register_fn(
            "process_kill",
            |host: &mut ScriptHost, name: &str| -> Result<(), Box<EvalAltResult>> {
                host.note(format!("process_kill {name}"));
                crate::packs::script_process_kill(name).map_err(err)
            },
        );
        engine.register_fn(
            "process_start",
            |host: &mut ScriptHost, path: &str| -> Result<(), Box<EvalAltResult>> {
                // a script may only start programs from the same places it may write files: an
                // unrestricted spawn would hand it the whole machine
                let target_dir = host.target_dir.clone();
                if !exe_allowed(path, &target_dir) {
                    host.note(format!("process_start {path} REFUSED (outside the allowed folders)"));
                    return Err(err(format!(
                        "process_start refused: {path} is outside %APPDATA%, %LOCALAPPDATA%, %PROGRAMDATA% and the target's folder"
                    )));
                }
                host.note(format!("process_start {path}"));
                crate::packs::script_process_start(path).map_err(err)
            },
        );
    }

    // ---- files (reads anywhere, writes only in config locations / the target folder) ----
    if caps.grants(Capability::Files) {
        engine.register_fn(
            "read_file",
            |host: &mut ScriptHost, path: &str| -> ImmutableString {
                match std::fs::read_to_string(path) {
                    Ok(text) => {
                        host.note(format!("read_file {path} ({} bytes)", text.len()));
                        text.into()
                    }
                    Err(e) => {
                        host.note(format!("read_file {path} FAILED: {e}"));
                        "".into()
                    }
                }
            },
        );
        engine.register_fn(
            "read_json",
            |host: &mut ScriptHost, path: &str, pointer: &str| -> ImmutableString {
                let value = crate::packs::script_json_pointer(path, pointer).unwrap_or_default();
                host.note(format!("read_json {path} {pointer} -> {value}"));
                value.into()
            },
        );
        engine.register_fn(
            "read_ini",
            |host: &mut ScriptHost, path: &str, section: &str, key: &str| -> ImmutableString {
                let value = read_ini_value(Path::new(path), section, key)
                    .map(|v| {
                        host.note(format!("read_ini {path} [{section}] {key} -> {v}"));
                        v
                    })
                    .unwrap_or_default();
                value.into()
            },
        );
    }
    if caps.grants(Capability::FilesWrite) {
        engine.register_fn(
            "write_file",
            |host: &mut ScriptHost, path: &str, text: &str| -> Result<(), Box<EvalAltResult>> {
                let target_dir = host.target_dir.clone();
                let path = Path::new(path);
                if !write_allowed(path, &target_dir) {
                    host.note(format!(
                        "write_file {} REFUSED (outside the allowed folders)",
                        path.display()
                    ));
                    return Err(err(format!(
                        "write_file refused: {} is outside %APPDATA%, %LOCALAPPDATA%, %PROGRAMDATA% and the target's folder",
                        path.display()
                    )));
                }
                // the folders the pack needs do not have to exist yet
                if let Some(parent) = path.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    return Err(err(format!("write_file {}: {e}", path.display())));
                }
                host.note(format!("write_file {} ({} bytes)", path.display(), text.len()));
                std::fs::write(path, text)
                    .map_err(|e| err(format!("write_file {}: {e}", path.display())))
            },
        );
    }

    // ---- the windows of other apps ----
    if caps.grants(Capability::Windows) {
        engine.register_fn("windows_of", |host: &mut ScriptHost, pid: i64| -> Array {
            let windows = crate::packs::script_windows_of(pid as u32);
            let list: Array = windows.into_iter().map(|w| Dynamic::from(w as i64)).collect();
            host.note(format!("windows_of {pid} -> {} window(s)", list.len()));
            list
        });
        engine.register_fn("window_hide", |host: &mut ScriptHost, hwnd: i64| -> bool {
            let was = crate::packs::script_window_hide(hwnd as isize);
            host.note(format!("window_hide {hwnd} (was visible: {was})"));
            was
        });
        engine.register_fn("window_show", |host: &mut ScriptHost, hwnd: i64| -> bool {
            let was = crate::packs::script_window_show(hwnd as isize);
            host.note(format!("window_show {hwnd} (was visible: {was})"));
            was
        });
    }

    // ---- audio ----
    if caps.grants(Capability::Audio) {
        engine.register_fn("audio_sessions", |host: &mut ScriptHost| -> Array {
            let sessions = crate::packs::script_audio_sessions();
            let list: Array = sessions.into_iter().map(Dynamic::from).collect();
            host.note(format!("audio_sessions -> {} session(s)", list.len()));
            list
        });
        engine.register_fn(
            "audio_mute",
            |host: &mut ScriptHost, pid: i64, muted: bool| -> ImmutableString {
                let report = crate::packs::script_audio_mute(pid as u32, muted);
                host.note(format!("audio_mute {pid} {muted}: {report}"));
                report.into()
            },
        );
        engine.register_fn(
            "audio_mute_all",
            |host: &mut ScriptHost, muted: bool| -> Result<ImmutableString, Box<EvalAltResult>> {
                match crate::packs::script_audio_mute_all(muted) {
                    Ok(report) => {
                        host.note(format!("audio_mute_all {muted}: {report}"));
                        Ok(report.into())
                    }
                    Err(e) => {
                        host.note(format!("audio_mute_all REFUSED: {e}"));
                        Err(err(e))
                    }
                }
            },
        );
    }

    // ---- the idle overlay ----
    if caps.grants(Capability::Overlay) {
        engine.register_fn("overlay_show", |host: &mut ScriptHost| {
            host.note("overlay_show (the daemon was asked to show the idle overlay)");
            crate::packs::request_overlay(crate::packs::OverlayRequest::Show);
        });
        engine.register_fn("overlay_hide", |host: &mut ScriptHost| {
            host.note("overlay_hide (the daemon was asked to take the idle overlay away)");
            crate::packs::request_overlay(crate::packs::OverlayRequest::Hide);
        });
    }

    Sandbox {
        engine,
        deadline,
        budget,
        base,
    }
}

// ---------------------------------------------------------------------------
// Talking to another app's window

/// How long a pack's message may wait for the other app. A round trip to a live app takes
/// well under a millisecond; an app that has not answered in this long is not going to.
const MESSAGE_TIMEOUT_MS: u32 = 2000;

/// Send a `WM_COPYDATA` message to a window of another app. `class` and `title` are matched
/// exactly; an empty string means "any" for that field, so a class alone finds the window.
/// The payload travels as a UTF-16 string. This is how a pack reaches an app the daemon knows
/// nothing about.
///
/// The send is synchronous but bounded: it gives up after [`MESSAGE_TIMEOUT_MS`], so an app that
/// has stopped pumping its queue cannot hold the pack worker. The window gets its own copy of
/// the payload, so this buffer is safe to drop either way.
pub fn send_window_message(class: &str, title: &str, payload: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{GetLastError, LPARAM};
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SendMessageTimeoutW, SMTO_BLOCK, WM_COPYDATA,
    };

    // an empty field matches any window: FindWindowW wants a null pointer for that
    let class_w = wide(class);
    let title_w = wide(title);
    let class_p = if class.is_empty() { std::ptr::null() } else { class_w.as_ptr() };
    let title_p = if title.is_empty() { std::ptr::null() } else { title_w.as_ptr() };

    let hwnd = unsafe { FindWindowW(class_p, title_p) };
    if hwnd.is_null() {
        return Err(format!("no window with class '{class}' and title '{title}'"));
    }

    // the payload is a UTF-16 string, dwData 1 (what the receiving app expects)
    let mut data_w: Vec<u16> = payload.encode_utf16().collect();
    data_w.push(0);
    let bytes = data_w.len() * std::mem::size_of::<u16>();
    let mut data = COPYDATASTRUCT {
        dwData: 1,
        cbData: bytes as u32,
        lpData: data_w.as_mut_ptr() as *mut core::ffi::c_void,
    };

    // SMTO_BLOCK keeps the send synchronous (as SendMessageW is) but gives up after
    // MESSAGE_TIMEOUT_MS: a hung app must not block the pack worker forever. PostMessage is
    // not an option here: the system cannot post a marshalled WM_COPYDATA.
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            0 as _,
            &mut data as *mut COPYDATASTRUCT as LPARAM,
            SMTO_BLOCK,
            MESSAGE_TIMEOUT_MS,
            std::ptr::null_mut(),
        )
    };
    if sent == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!(
            "the app did not answer the message to class '{class}' and title '{title}' within {MESSAGE_TIMEOUT_MS} ms (error {code})"
        ));
    }
    Ok(())
}

/// A NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Writes are limited to the places app configs actually live, plus the app's own folder.
fn write_allowed(path: &Path, target_dir: &str) -> bool {
    write_allowed_in(path, &allowed_roots(target_dir))
}

/// The same check against a fixed set of roots. A folder that does not exist yet is fine: the
/// closest folder that *does* exist is the one that has to be inside a root. That is what stops
/// `..` and a symlink that points out of the roots, and what lets a pack create a new folder
/// inside a root.
fn write_allowed_in(path: &Path, roots: &[PathBuf]) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if is_link(path) {
        return false; // the file itself is a link: writing it would land outside the roots
    }
    let Some(ancestor) = nearest_existing(parent) else {
        return false; // nothing on the way up exists
    };
    let Ok(canon) = ancestor.canonicalize() else {
        return false;
    };
    allowed_in(&canon, roots)
}

/// The closest ancestor of `path` that exists as a folder. The OS resolves `..` on the way, so
/// a path that walks out of a root ends up on an ancestor that is not inside one.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.is_dir() {
            return Some(current);
        }
        // `file_name` is None for a path that ends in `..`: the OS resolves that itself and
        // there is no folder to build on
        current.file_name()?;
        current = current.parent()?.to_path_buf();
    }
}

/// True when the path is a symlink or another reparse point (a junction): writing through one
/// would land outside the allowed roots.
fn is_link(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false; // it does not exist yet: a new file cannot be a link
    };
    meta.file_type().is_symlink() || is_reparse_point(&meta)
}

/// Windows: a junction is a reparse point but not a symlink, and it leaves the roots just the
/// same.
#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
}

#[cfg(not(windows))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
}

/// A script may run programs from the same places it may write files: the target app's own
/// folder, %APPDATA%, %LOCALAPPDATA% and %PROGRAMDATA%. `cmd.exe` and `powershell.exe` live in
/// System32 and are out of reach.
fn exe_allowed(exe: &str, target_dir: &str) -> bool {
    let path = Path::new(exe);
    if !path.is_absolute() {
        return false; // a bare name would be looked up in PATH (System32)
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    allowed_in(parent, &allowed_roots(target_dir))
}

/// True when the folder is inside one of the roots. The roots and the folder are canonicalised
/// first, so a `..` segment cannot walk out of a root; a folder that cannot be resolved is
/// refused.
fn allowed_in(dir: &Path, roots: &[PathBuf]) -> bool {
    let Ok(canon) = dir.canonicalize() else {
        return false; // an unresolved path is never allowed
    };
    roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| under_root(&canon, &root))
}

/// Component-wise containment: the root must be followed by a separator, so `...\RoamingEvil`
/// is not inside `...\Roaming`.
fn under_root(path: &Path, root: &Path) -> bool {
    let path = parts(path);
    let root = parts(root);
    path.len() >= root.len() && path.iter().zip(root.iter()).all(|(a, b)| a == b)
}

/// The path components, lowercased: Windows paths are case-insensitive.
fn parts(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect()
}

/// `[section]` + `key = value`, case-insensitive, UTF-16/BOM aware.
fn read_ini_value(path: &Path, section: &str, key: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        let (chunks, _) = bytes[2..].as_chunks::<2>();
        let words: Vec<u16> = chunks.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&words)
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let mut in_section = false;
    let mut found_section = section.is_empty();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = true;
            found_section = name.trim().eq_ignore_ascii_case(section);
            continue;
        }
        if in_section && found_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Directories a script may write to (exposed for the tests).
pub(crate) fn allowed_roots(target_dir: &str) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(target_dir)];
    for var in ["APPDATA", "LOCALAPPDATA", "PROGRAMDATA"] {
        if let Ok(value) = std::env::var(var) {
            roots.push(PathBuf::from(value));
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostMessageW,
        RegisterClassW, TranslateMessage, WNDCLASSW, WM_COPYDATA, WM_QUIT,
    };

    fn ctx() -> TemplateCtx {
        let mut ctx = TemplateCtx {
            target_name: "app.exe".into(),
            target_exe: "C:\\Apps\\app.exe".into(),
            target_dir: "C:\\Apps".into(),
            ..Default::default()
        };
        ctx.settings.insert("mode".into(), "turbo".into());
        ctx
    }

    #[test]
    fn a_script_reads_the_context_and_captures() {
        let mut ctx = ctx();
        let lines = run(
            r#"
            ctx.log("hello " + ctx.target_name());
            ctx.capture("prev", ctx.setting("mode"));
            "#,
            &mut ctx,
        )
        .expect("script runs");
        assert_eq!(ctx.captured.get("prev").unwrap(), "turbo");
        assert!(lines.iter().any(|l| l.contains("hello app.exe")));
    }

    #[test]
    fn syntax_errors_are_reported() {
        assert!(compile("fn broken(").is_err());
        assert!(compile("ctx.log(\"ok\");").is_ok());
    }

    #[test]
    fn limits_stop_a_runaway_script() {
        // an infinite loop must be cut off by the operation budget, not hang the caller
        let mut ctx = ctx();
        let started = Instant::now();
        let result = run("let i = 0; while true { i += 1; }", &mut ctx);
        assert!(result.is_err(), "runaway script must fail");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the guard took too long"
        );
    }

    #[test]
    fn one_engine_serves_every_run() {
        // The engine and its host functions are built once and kept: building them again
        // for every run is what made a run cost half a millisecond more than it should.
        let mut ctx = ctx();
        let before = BUILDS.with(|c| c.get());
        for _ in 0..5 {
            run("ctx.log(\"hello\");", &mut ctx).expect("the script runs");
        }
        let after = BUILDS.with(|c| c.get());
        assert!(
            after - before <= 1,
            "the engine must be built once and kept, not built per run ({before} -> {after})"
        );
    }

    // -----------------------------------------------------------------------
    // The capability gate

    /// The capabilities of the acceptance case: a pack that asks for the files and
    /// nothing else.
    fn files_only() -> CapSet {
        CapSet::default().with(Capability::Files)
    }

    #[test]
    fn a_pack_that_asks_for_files_cannot_run_a_program() {
        // The acceptance of the phase: the call fails, the message names the missing
        // capability and what the pack asked for, and the denial is in the pack's log.
        let mut ctx = ctx();
        let err = run_with_capabilities(
            r#"ctx.run_cli("C:\\Windows\\System32\\cmd.exe", ["/c", "echo hi"]);"#,
            &mut ctx,
            files_only(),
        )
        .expect_err("a pack that was not granted 'exec' must not run a program");
        assert!(err.contains("run_cli"), "{err}");
        assert!(err.contains("exec"), "the missing capability: {err}");
        assert!(err.contains("files"), "what the pack asked for: {err}");
        assert!(
            err.contains("REFUSED"),
            "the denial is written to the pack's log: {err}"
        );
    }

    #[test]
    fn only_the_granted_functions_are_on_the_engine() {
        let mut ctx = ctx();
        let context = CapSet::default().with(Capability::Context);
        // the granted function is there
        run_with_capabilities(
            r#"if ctx.target_name() != "app.exe" { throw "wrong target" }"#,
            &mut ctx,
            context,
        )
        .expect("the granted function must be there");
        // a function of another capability is not
        let err = run_with_capabilities(r#"ctx.log("hi");"#, &mut ctx, context)
            .expect_err("the journal capability was not granted");
        assert!(err.contains("journal"), "{err}");
        assert!(err.contains("context"), "{err}");
    }

    #[test]
    fn one_engine_per_capability_set() {
        // Each distinct capability set gets its own engine, built once and kept.
        let mut ctx = ctx();
        let files = files_only();
        let exec = CapSet::default().with(Capability::Exec);
        let before = BUILDS.with(|c| c.get());
        run_with_capabilities(r#"let t = ctx.read_file("nope.txt");"#, &mut ctx, files)
            .expect("the script runs");
        run_with_capabilities(r#"let t = ctx.read_file("nope.txt");"#, &mut ctx, files)
            .expect("the script runs");
        // the second capability set: its own engine, even though the script is the same
        let _ = run_with_capabilities(r#"ctx.run_cli("x", []);"#, &mut ctx, exec);
        let after = BUILDS.with(|c| c.get());
        assert_eq!(
            after - before,
            2,
            "one engine per capability set, built once ({before} -> {after})"
        );
    }

    #[test]
    fn the_budget_is_rearmed_on_every_engine() {
        // Every engine carries its own budgets, and they are re-armed before every run:
        // a run cannot inherit the allowance of the run before it, not even the run
        // before it on another engine.
        let mut ctx = ctx();
        let one = CapSet::default().with(Capability::Time);
        let two = one.with(Capability::Journal);
        let busy = "let i = 0; while true { i += 1; ctx.sleep(50); }";
        let budget = Duration::from_millis(300);

        let err = run_with_budget(busy, &mut ctx, budget, one).expect_err("the run must be cut off");
        assert!(err.contains("budget"), "{err}");

        // the first run on the *second* engine: it must get a budget of its own, not the
        // expired deadline of the first engine
        let started = Instant::now();
        let err = run_with_budget(busy, &mut ctx, budget, two).expect_err("must be cut off too");
        assert!(err.contains("budget"), "{err}");
        assert!(
            started.elapsed() >= budget,
            "the second engine must get a budget of its own: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_new_capabilities_are_reachable_and_gated() {
        // COM for the audio sessions: the audio interfaces are COM objects and COM is
        // per thread.
        crate::killswitch::init_com();
        let mut ctx = ctx();

        let windows = CapSet::default().with(Capability::Windows);
        run_with_capabilities(r#"let w = ctx.windows_of(1);"#, &mut ctx, windows)
            .expect("the windows capability must be there");

        let audio = CapSet::default().with(Capability::Audio);
        run_with_capabilities(r#"let s = ctx.audio_sessions();"#, &mut ctx, audio)
            .expect("the audio capability must be there");

        let overlay = CapSet::default().with(Capability::Overlay);
        run_with_capabilities(r#"ctx.overlay_show();"#, &mut ctx, overlay)
            .expect("the overlay capability must be there");

        // and each of them is refused without its capability
        let err = run_with_capabilities(r#"let w = ctx.windows_of(1);"#, &mut ctx, CapSet::default())
            .expect_err("the windows capability was not granted");
        assert!(err.contains("windows"), "{err}");
    }

    #[test]
    fn a_capability_the_pack_asked_for_nothing_says_so() {
        let mut ctx = ctx();
        let err = run_with_capabilities(r#"ctx.log("hi");"#, &mut ctx, CapSet::default())
            .expect_err("nothing was granted");
        assert!(err.contains("nothing"), "{err}");
    }

    #[test]
    fn the_operation_budget_is_rearmed_on_every_run() {
        // A script that never ends. The operation budget must stop it — and it must stop
        // the *second* run too, with a budget of its own: the script reports how far it
        // got, so the test can see that the second run was not left with an empty budget.
        let runaway = r#"
            let i = 0;
            while true {
                i += 1;
                if i % 1000 == 0 { ctx.capture("progress", i.to_string()); }
            }
        "#;
        let mut ctx = ctx();
        let first = run(runaway, &mut ctx).expect_err("the runaway script must be stopped");
        assert!(first.contains("Too many operations"), "{first}");
        let progress = ctx
            .captured
            .get("progress")
            .and_then(|v| v.parse::<u64>().ok())
            .expect("the first run must have made progress");
        assert!(progress > 0, "the first run must have made progress");

        // the second run starts with an empty capture: a run that gets no budget at all
        // leaves nothing behind
        ctx.captured.remove("progress");
        let second = run(runaway, &mut ctx).expect_err("the second run must be stopped too");
        assert!(second.contains("Too many operations"), "{second}");
        let again = ctx
            .captured
            .get("progress")
            .and_then(|v| v.parse::<u64>().ok())
            .expect("the second run must have got a budget of its own");
        assert!(
            again >= progress / 2,
            "the second run must get a fresh budget: {progress} then {again}"
        );
    }

    #[test]
    fn the_wall_clock_budget_is_rearmed_on_every_run() {
        // A script that spends its time in a host call, so that the operation budget never
        // gets near it: only the wall clock can stop this one. The second run must get a
        // budget of its own — if the deadline were left over from the first run, the second
        // one would be cut off at once instead of running into its own budget.
        let busy = "let i = 0; while true { i += 1; ctx.sleep(50); }";
        let budget = Duration::from_millis(300);
        let mut ctx = ctx();

        let started = Instant::now();
        let first = run_with_budget(busy, &mut ctx, budget, CapSet::ALL)
            .expect_err("the first run must be cut off");
        let first_took = started.elapsed();
        assert!(first.contains("budget"), "{first}");
        assert!(
            first_took >= budget,
            "the first run must run into its budget: {first_took:?}"
        );

        let started = Instant::now();
        let second = run_with_budget(busy, &mut ctx, budget, CapSet::ALL)
            .expect_err("the second run must be cut off too");
        let second_took = started.elapsed();
        assert!(second.contains("budget"), "{second}");
        assert!(
            second_took >= budget,
            "the second run must get a budget of its own, not the first one's: {second_took:?}"
        );
    }

    #[test]
    fn two_scripts_do_not_see_each_others_state() {
        // The scope is the per-run state: the second script must not see what the first one
        // left behind, even though both run on the same engine.
        let mut ctx = ctx();
        run(
            "let secret = \"first\"; fn helper() { 42 } ctx.log(secret);",
            &mut ctx,
        )
        .expect("the first script runs");
        let err = run("ctx.log(secret);", &mut ctx)
            .expect_err("the second script must not see the first one's variables");
        assert!(err.contains("secret"), "{err}");
        let err = run("let x = helper();", &mut ctx)
            .expect_err("the second script must not see the first one's functions");
        assert!(err.contains("helper"), "{err}");
    }

    #[test]
    fn the_sandbox_has_no_ambient_powers() {
        let mut ctx = ctx();
        // Rhai without extra packages has no file/process API of its own
        assert!(run("let f = open(\"C:\\\\x.txt\");", &mut ctx).is_err());
        assert!(run("eval(\"1 + 1\");", &mut ctx).is_err());
    }

    #[test]
    fn writes_outside_the_allowed_roots_are_refused() {
        let mut ctx = ctx();
        let result = run(
            r#"ctx.write_file("C:\\Windows\\System32\\drivers\\etc\\hosts", "x");"#,
            &mut ctx,
        );
        assert!(result.is_err(), "must refuse a system path");
        assert!(result.unwrap_err().contains("refused"));
    }

    #[test]
    fn the_write_guard_compares_path_components() {
        let dir = Scratch::dir("guard");
        let root = dir.join("Roaming");
        let evil = dir.join("RoamingEvil");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&evil).unwrap();
        let roots = vec![root.clone()];

        // the root itself and a folder inside it
        assert!(allowed_in(&root, &roots));
        assert!(allowed_in(&root.join("sub"), &roots));
        // a sibling whose name only starts with the root's name
        assert!(!allowed_in(&evil, &roots));
        // a traversal that walks out of the root
        assert!(!allowed_in(
            &root.join("sub").join("..").join("..").join("RoamingEvil"),
            &roots
        ));
        // a folder that cannot be resolved is refused, not guessed
        assert!(!allowed_in(&root.join("nope"), &roots));
    }

    #[test]
    fn a_traversal_cannot_walk_out_of_the_allowed_folders() {
        let appdata = std::env::var("APPDATA").expect("APPDATA");
        // a file directly in %APPDATA% is fine
        assert!(write_allowed(&Path::new(&appdata).join("file.txt"), "C:\\Apps"));
        // .. must not walk out of it
        assert!(!write_allowed(
            &Path::new(&appdata).join("..").join("file.txt"),
            "C:\\Apps"
        ));
    }

    #[test]
    fn a_write_may_create_a_new_folder_inside_the_allowed_roots() {
        let dir = Scratch::dir("write");
        let mut ctx = ctx();
        ctx.target_dir = dir.display().to_string(); // the target's own folder is an allowed root
        // the folder does not exist yet: the pack has to be able to create it
        let file = dir.join("SomeApp").join("config.json");
        let path = file.display().to_string().replace('\\', "\\\\");
        let result = run(&format!("ctx.write_file(\"{path}\", \"{{}}\");"), &mut ctx);
        assert!(
            result.is_ok(),
            "a new folder inside the target's folder must be writable"
        );
        assert!(file.exists(), "the folder must have been created");
    }

    #[test]
    fn a_write_whose_closest_existing_folder_is_outside_the_roots_is_refused() {
        let dir = Scratch::dir("outside");
        let root = dir.join("root");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let roots = vec![root.clone()];
        // the closest folder that exists is outside the root: refused
        assert!(!write_allowed_in(
            &outside.join("deep").join("file.txt"),
            &roots
        ));
        // the same shape inside the root is fine
        assert!(write_allowed_in(
            &root.join("deep").join("file.txt"),
            &roots
        ));
    }

    #[test]
    fn a_write_through_a_symlink_is_refused() {
        let dir = Scratch::dir("link");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "x").unwrap();
        let link = root.join("link.txt");
        if std::os::windows::fs::symlink_file(&outside, &link).is_err() {
            return; // this user may not create symlinks: nothing to check
        }
        let roots = vec![root.clone()];
        assert!(
            !write_allowed_in(&link, &roots),
            "a symlink must not be written through"
        );
    }

    #[test]
    fn a_script_may_only_run_programs_from_the_allowed_folders() {
        let appdata = std::env::var("APPDATA").expect("APPDATA");
        // a program in %APPDATA% is fine
        assert!(exe_allowed(&format!("{}\\helper.exe", appdata), "C:\\Apps"));
        // a bare name would be looked up in PATH
        assert!(!exe_allowed("cmd.exe", "C:\\Apps"));
        // System32 is out of reach
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        assert!(!exe_allowed(&cmd.display().to_string(), "C:\\Apps"));
    }

    #[test]
    fn a_script_cannot_shell_out_through_cmd() {
        let mut ctx = ctx();
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        let exe = cmd.display().to_string().replace('\\', "\\\\");
        let result = run(
            &format!("ctx.run_cli(\"{exe}\", [\"/c\", \"echo hi\"]);"),
            &mut ctx,
        );
        assert!(result.is_err(), "a script must not be able to run cmd.exe");
        assert!(result.unwrap_err().contains("refused"));
    }

    #[test]
    fn a_script_cannot_start_a_program_outside_the_allowed_folders() {
        let mut ctx = ctx();
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        let exe = cmd.display().to_string().replace('\\', "\\\\");
        let result = run(&format!("ctx.process_start(\"{exe}\");"), &mut ctx);
        assert!(
            result.is_err(),
            "a script must not be able to start cmd.exe"
        );
        assert!(result.unwrap_err().contains("refused"));
    }

    #[test]
    fn a_failing_script_keeps_its_captures() {
        let mut ctx = ctx();
        let result = run(
            r#"ctx.capture("prev", "kept"); throw "boom";"#,
            &mut ctx,
        );
        assert!(result.is_err(), "the script must fail");
        assert_eq!(
            ctx.captured.get("prev").map(String::as_str),
            Some("kept"),
            "a capture taken before the failure must survive it"
        );
    }

    #[test]
    fn ini_reading_handles_sections_and_case() {
        let dir = Scratch::dir("ini");
        let file = dir.join("Rainmeter.ini");
        std::fs::write(
            &file,
            "[Rainmeter]\r\nActiveLayout=illustro default\r\n[Other]\r\nActiveLayout=nope\r\n",
        )
        .unwrap();
        assert_eq!(
            read_ini_value(&file, "rainmeter", "activelayout").as_deref(),
            Some("illustro default")
        );
        assert_eq!(read_ini_value(&file, "Other", "ActiveLayout").as_deref(), Some("nope"));
        assert_eq!(read_ini_value(&file, "Missing", "x"), None);
    }

    #[test]
    fn allowed_roots_include_the_target_folder() {
        let roots = allowed_roots("C:\\Games\\App");
        assert!(roots.iter().any(|r| r == Path::new("C:\\Games\\App")));
    }

    #[test]
    fn a_message_to_a_window_that_does_not_exist_fails_clearly() {
        let mut ctx = ctx();
        // the shape a pack uses: a conditional call with a quoted payload
        let result = run(
            r#"let layout = ctx.setting("mode");
               if layout != "kill" {
                   ctx.window_message("ArmouryDaemonNoSuchWindowClass", "", "!LoadLayout \"" + layout + "\"");
               }"#,
            &mut ctx,
        );
        let err = result.expect_err("a window that does not exist must fail");
        assert!(err.contains("ArmouryDaemonNoSuchWindowClass"), "{err}");
        assert!(err.contains("title"), "the error must name the title: {err}");
    }

    #[test]
    fn the_rainmeter_pack_script_compiles_against_the_host_api() {
        // the shipped pack reaches another app through ctx.window_message: the script compiles
        // and the pack passes the schema, and the host API is a closed list
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = std::fs::read_to_string(root.join("packs/rainmeter/pack.json"))
            .expect("the shipped Rainmeter pack must exist");
        let pack = armoury_core::pack::Pack::parse(&text).expect("the Rainmeter pack must be valid");
        let errors = pack.validate();
        assert!(errors.is_empty(), "the Rainmeter pack must pass validation: {errors:?}");
        let apply = pack
            .script
            .apply
            .as_deref()
            .expect("the pack must carry an apply script");
        assert!(
            apply.contains("window_message"),
            "the apply script must use the host function"
        );
        crate::packs::check_scripts(&pack).expect("the Rainmeter pack's scripts must compile");
        // a host function that does not exist must fail: the host API is a closed list
        let mut ctx = ctx();
        let err = run("ctx.no_such_host_function();", &mut ctx)
            .expect_err("a host function that does not exist must fail");
        assert!(err.contains("no_such_host_function"), "{err}");
    }

    /// The payload the test window received.
    static RECEIVED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    /// Creates a hidden window of `class` on this thread and returns its handle.
    fn create_window(
        class: &str,
        wndproc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    ) -> usize {
        let class_w: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
        let mut wc: WNDCLASSW = unsafe { std::mem::zeroed() };
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = std::ptr::null_mut();
        wc.lpszClassName = class_w.as_ptr();
        let atom = unsafe { RegisterClassW(&wc) };
        assert_ne!(atom, 0, "RegisterClassW failed: {}", unsafe { GetLastError() });
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_w.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert!(!hwnd.is_null(), "CreateWindowExW failed: {}", unsafe { GetLastError() });
        hwnd as usize
    }

    /// A hidden window of `class` on a thread of its own, like an app that owns one. The thread
    /// pumps its messages, so a message sent to the window is answered. It reports the window
    /// through `ready` and the end of its pump through `done`.
    fn pumping_window(
        class: &'static str,
        wndproc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
        ready: std::sync::mpsc::Sender<usize>,
        done: std::sync::mpsc::Sender<()>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let hwnd = create_window(class, wndproc);
            let _ = ready.send(hwnd);
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let _ = done.send(());
        })
    }

    /// A hidden window of `class` whose thread never answers: an app that stopped pumping.
    /// The thread is given a bounded life so that it cannot outlive the suite.
    fn hung_window(
        class: &'static str,
        wndproc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
        ready: std::sync::mpsc::Sender<usize>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let hwnd = create_window(class, wndproc);
            let _ = ready.send(hwnd);
            std::thread::sleep(Duration::from_secs(30));
        })
    }

    /// The round trip on a worker thread: the window is pumped by a thread of its own, the
    /// message goes out, the payload comes back and the window is shut down. Every step is
    /// bounded, so this always returns.
    fn exchange_with_a_window(
        class: &'static str,
        wndproc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    ) -> Result<String, String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<usize>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let thread = pumping_window(class, wndproc, ready_tx, done_tx);
        let hwnd = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the window must be created".to_string())?;

        let payload = "!LoadLayout \"Empty\"";
        send_window_message(class, "", payload)?;

        let mut got = None;
        for _ in 0..100 {
            if let Some(text) = RECEIVED.lock().unwrap().clone() {
                got = Some(text);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let got = got.ok_or("the payload must arrive".to_string())?;

        // shut the window down: the thread says it is done, and only then is it joined
        unsafe { PostMessageW(hwnd as HWND, WM_QUIT, 0, 0) };
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the window thread must stop".to_string())?;
        thread.join().map_err(|_| "the window thread panicked".to_string())?;
        Ok(got)
    }

    #[test]
    fn a_message_reaches_a_real_window() {
        const CLASS: &str = "ArmouryDaemonWindowMessageTest";

        unsafe extern "system" fn wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe {
                if msg == WM_COPYDATA {
                    let cds = &*(lparam as *const COPYDATASTRUCT);
                    let words = std::slice::from_raw_parts(
                        cds.lpData as *const u16,
                        cds.cbData as usize / std::mem::size_of::<u16>(),
                    );
                    *RECEIVED.lock().unwrap() = Some(String::from_utf16_lossy(words));
                    return 1;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }

        // The whole exchange runs on a thread of its own and every wait is a timeout: a window
        // that never answers can fail this test but can never wedge the suite.
        let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
        std::thread::spawn(move || {
            let _ = tx.send(exchange_with_a_window(CLASS, wndproc));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the exchange must finish")
            .expect("the exchange must succeed");
        assert_eq!(got.trim_end_matches('\0'), "!LoadLayout \"Empty\"");
    }

    #[test]
    fn a_hung_window_gives_up() {
        const CLASS: &str = "ArmouryDaemonHungWindowTest";

        unsafe extern "system" fn wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        // a window whose thread never answers: the send must give up instead of hanging
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<usize>();
        let thread = hung_window(CLASS, wndproc, ready_tx);
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the window must be created");

        // the send runs on a thread of its own, so that a send that does not give up fails this
        // test instead of wedging the suite
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        std::thread::spawn(move || {
            let _ = tx.send(send_window_message(CLASS, "", "hello"));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the send must give up instead of hanging");
        let err = result.expect_err("a window that never answers must fail");
        assert!(err.contains(CLASS), "{err}");
        // the window's thread is asleep and detached: it must not hold the test up
        drop(thread);
    }
}
