//! Pack execution: loads spec 1 (data only) and spec 2 (data plus a script) packs and
//! runs their mechanisms and scripts, a script interpreted here against the host API.
//!
//! Runs on two worker threads of its own — the daemon's main loop must stay responsive, and
//! CLI calls block for tens to hundreds of milliseconds. Packs and modes each get a thread
//! and a channel of their own (A4), so a slow mode never delays a pack apply/revert.
//! Nothing here ever goes through a shell, and packs can only use whitelisted mechanisms.

use armoury_core::pack::{
        CapSet, Capability, Pack, PackKind, Step, StepBlock, TemplateCtx, substitute, version_at_least,
    };
use crossbeam_channel::Receiver;
use log::{info, warn};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::script;

use windows::Win32::Foundation::{GetLastError, HWND};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindow, IsWindowVisible, ShowWindow, SW_HIDE, SW_SHOW,
};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// This daemon's own version. A pack may ask for a newer one (`min_engine`), and this is
/// what it is measured against.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How soon after a revert an apply may reuse that revert's captured values instead of reading
/// them again from disk. The controlled apps write their config with a delay (G-Helper: ~2 s), so a
/// capture taken right after our own revert reads our own change back as "the state before".
const CARRY_WINDOW: Duration = Duration::from_secs(10);

type CaptureMap = HashMap<String, HashMap<String, String>>;

/// Feature 8: cut the capabilities the user unticked at install time out of the pack's grant
/// set. Done once, when the packs are loaded, so every step and script downstream sees the
/// trimmed set - the denial is structural, exactly like a capability never declared.
pub fn apply_denials(
    installed: &mut [Installed],
    denials: &HashMap<String, Vec<String>>,
) {
    for pack in installed.iter_mut() {
        let Some(denied) = denials.get(&pack.pack.id) else {
            continue;
        };
        for name in denied {
            if let Some(capability) = Capability::from_name(name) {
                pack.pack.capabilities = pack.pack.capabilities.without(capability);
            }
        }
    }
}

/// Feature 8: what a pack will do on apply, one line per effect, in the words of the install
/// preview - the effect, never the mechanism name. Anything the summary cannot name is said
/// plainly instead of hidden, so a preview never reads as safer than the pack is.
pub fn describe(pack: &Pack) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let target = if pack.targets.process.is_empty() {
        String::new()
    } else {
        pack.targets.process.join(", ")
    };
    if !target.is_empty() {
        lines.push(format!("Watches for {target}"));
    }
    for step in &pack.apply.steps {
        let exe = step
            .exe
            .clone()
            .or_else(|| step.exe_from_setting.clone())
            .unwrap_or_default();
        let line = match step.mechanism.as_str() {
            "cli" => {
                let args = step.args.join(" ");
                if args.is_empty() {
                    format!("Runs {exe}")
                } else {
                    format!("Runs {exe} {args}")
                }
            }
            "hotkey" => format!("Sends keystrokes to {exe}"),
            "process_kill" => format!("Stops {exe}"),
            "process_start" => format!("Starts {exe}"),
            "registry_set" => match &step.registry {
                Some(reg) => format!("Writes {} to {}", reg.name, reg.path),
                None => "Writes a registry value".to_string(),
            },
            other => format!("Runs a '{other}' step"),
        };
        lines.push(line);
    }
    for capture in &pack.capture {
        match capture.mechanism.as_str() {
            "json_file" => {
                let path = capture.path.clone().unwrap_or_default();
                let pointer = capture.pointer.clone().unwrap_or_default();
                lines.push(format!("Reads {pointer} from {path}"));
            }
            "process_running" => lines.push(format!("Checks whether {target} is running")),
            other => lines.push(format!("Runs a '{other}' check")),
        }
    }
    if !pack.script.is_empty() {
        // The script is not read here: a preview that described a program line by line
        // would promise more than the daemon checks. The capabilities below say what it
        // may reach.
        lines.push("Runs the pack's own script (it can only use the permissions below)".to_string());
    }
    for setting in &pack.settings {
        lines.push(format!(
            "Shows the '{}' dropdown ({})",
            setting.label,
            setting.values.join(" / ")
        ));
    }
    lines
}

pub struct Installed {
    pub pack: Pack,
}

/// One apply/revert request from the daemon.
pub struct PackJob {
    /// true = trigger (game mode on), false = revert.
    pub apply: bool,
    /// Set for a single-pack operation (removing an applied pack): the outcome is then
    /// merged for that pack only instead of replacing the whole applied state.
    pub tag: Option<String>,
    /// Per-pack enabled flags and dropdown values (from config).
    pub enabled: HashMap<String, bool>,
    pub values: HashMap<String, HashMap<String, String>>,
    /// Values captured during the matching apply, replayed on revert (the journal).
    pub captured: HashMap<String, HashMap<String, String>>,
}

/// What happened, for logging and the dashboard's "applied" flag.
pub struct PackOutcome {
    pub applied: bool,
    /// Echo of `PackJob::tag`.
    pub tag: Option<String>,
    /// The enabled ids this job was scoped to (the daemon's split of a whole-state job by
    /// kind): an outcome speaks only for these, so the two halves of one job merge
    /// order-independently on the daemon side (A4).
    pub scoped: Vec<String>,
    /// Packs this job acted on. A pack whose block *started* is in here even if a step
    /// failed halfway: the machine was changed, so it must be journalled and reverted.
    pub applied_ids: Vec<String>,
    /// The dropdown values this job ran with (echoed so the journal can record them).
    pub values: HashMap<String, HashMap<String, String>>,
    /// Per-pack captured values (journal for the matching revert).
    pub captured: HashMap<String, HashMap<String, String>>,
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
    /// What the apply found when it went back to look, per pack id (2026-10-27): a pack whose
    /// captured value reads exactly as it did before the block ran changed nothing, however
    /// cleanly its steps returned. The daemon shows this on the pack row instead of a silent
    /// "applied". Empty for a revert — the pack is back to the state it captured.
    pub notes: Vec<(String, String)>,
}

/// Text files written on Windows often start with a UTF-8 BOM; serde_json rejects it.
fn strip_bom(text: &str) -> &str {
    text.trim_start_matches('\u{feff}')
}

/// The search folders to use, in order.
///
/// The first one is the folder of the executable (the install folder) and is always used.
/// Every other one is a fallback for a development build (`<repo>\target\release\ArmouryDaemon.exe`
/// finds the repository's own `packs` folder), and it is only used when it really looks
/// like a source checkout: a lone executable in `C:\Users\Someone\` must not fall back to
/// `C:\` and use the packs of a folder that has nothing to do with it.
pub fn usable_search_dirs(search_dirs: &[PathBuf]) -> Vec<PathBuf> {
    search_dirs
        .iter()
        .enumerate()
        .filter(|(index, dir)| *index == 0 || looks_like_a_checkout(dir))
        .map(|(_, dir)| dir.clone())
        .collect()
}

/// Does this folder hold a source checkout of the project? A `Cargo.toml` next to it is
/// the mark: a development build in `target\release\` has to find the repository's own
/// `packs` folder, and nothing else looks like that.
fn looks_like_a_checkout(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file()
}

pub fn load(search_dirs: &[PathBuf]) -> Vec<Installed> {
    let mut found: Vec<Installed> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for dir in search_dirs {
        let root = dir.join("packs");
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let manifest = entry.path().join("pack.json");
            if !manifest.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                warn!("cannot read {}", manifest.display());
                continue;
            };
            match Pack::parse(strip_bom(&text)) {
                Ok(pack) => {
                    if let Err(e) = check_scripts(&pack) {
                        warn!("pack {} rejected: {e}", manifest.display());
                        continue;
                    }
                    if let Err(e) = require_engine(&pack) {
                        warn!("pack {} rejected: {e}", manifest.display());
                        continue;
                    }
                    if seen.contains(&pack.id) {
                        continue; // a pack in an earlier search dir wins
                    }
                    info!(
                        "pack loaded: {} v{} (order {}, {} target(s), kind {})",
                        pack.name,
                        pack.version,
                        pack.order,
                        pack.targets.process.len(),
                        pack.kind.name()
                    );
                    seen.push(pack.id.clone());
                    found.push(Installed { pack });
                }
                Err(e) => warn!("pack {} rejected: {e}", manifest.display()),
            }
        }
    }

    found.sort_by_key(|p| p.pack.order);
    found
}

pub fn start(
    packs: Vec<Installed>,
    jobs: Receiver<PackJob>,
    mode_jobs: Receiver<PackJob>,
    outcomes: crossbeam_channel::Sender<PackOutcome>,
    wake: usize,
) {
    // Two serial workers instead of one (A4): a slow mode must never delay a pack
    // apply/revert, so each kind gets its own thread and channel. Outcomes stay on one
    // channel — the daemon drains it the same way as before, and every outcome carries the
    // ids its job was scoped to (`PackOutcome::scoped`), so both halves merge.
    let mut pack_side = Vec::new();
    let mut mode_side = Vec::new();
    for installed in packs {
        if installed.pack.kind == PackKind::Mode {
            mode_side.push(installed);
        } else {
            pack_side.push(installed);
        }
    }
    spawn_worker("pack", pack_side, jobs, outcomes.clone(), wake);
    spawn_worker("mode", mode_side, mode_jobs, outcomes, wake);
}

/// One worker: the serial loop `packs::start` used to run on a single thread.
fn spawn_worker(
    label: &'static str,
    packs: Vec<Installed>,
    jobs: Receiver<PackJob>,
    outcomes: crossbeam_channel::Sender<PackOutcome>,
    wake: usize,
) {
    std::thread::spawn(move || {
        info!("{} worker started with {} pack(s)", label, packs.len());
        // COM for the audio interfaces: a pack may ask for the 'audio' capability, and
        // the audio sessions are COM objects. COM is per thread, so the apartment has to
        // be set up here and not only on the daemon's main thread.
        crate::killswitch::init_com();
        // The previous successful revert's captured values, kept for a short window: an apply that
        // follows it must restore *those*, not re-read a file the app has not rewritten yet.
        let mut carry: Option<(Instant, CaptureMap)> = None;
        while let Ok(job) = jobs.recv() {
            let outcome = run_job(&packs, &job, &mut carry);
            let _ = outcomes.send(outcome);
            if wake != 0 {
                unsafe { windows_sys::Win32::System::Threading::SetEvent(wake as _) };
            }
        }
        info!("{} worker stopped", label);
    });
}

fn run_job(packs: &[Installed], job: &PackJob, carry: &mut Option<(Instant, CaptureMap)>) -> PackOutcome {
    let targets = snapshot_targets();
    let mut strings: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut notes: Vec<(String, String)> = Vec::new();
    let mut applied_ids: Vec<String> = Vec::new();
    let mut captured_out: CaptureMap = HashMap::new();
    let mut reverted_captures: CaptureMap = HashMap::new();
    let mut reused_carry = false;

    // apply ascending order, revert descending
    let ordered: Vec<&Installed> = if job.apply {
        packs.iter().collect()
    } else {
        packs.iter().rev().collect()
    };

    for installed in ordered {
        let pack = &installed.pack;
        if !job.enabled.get(&pack.id).copied().unwrap_or(false) {
            continue;
        }

        // Resolve this pack's own target (the app it controls, not the tracked app).
        let mut target_name = String::new();
        let mut target_exe = String::new();
        for wanted in &pack.targets.process {
            let key = wanted.to_ascii_lowercase();
            if let Some((path, _pid)) = targets.get(&key) {
                target_name = wanted.clone();
                target_exe = path.clone();
                break;
            }
        }

        let block = if job.apply { &pack.apply } else { &pack.revert };
        let mut ctx = TemplateCtx {
            target_exe: target_exe.clone(),
            target_name: target_name.clone(),
            target_dir: std::path::Path::new(&target_exe)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            ..Default::default()
        };
        if let Some(values) = job.values.get(&pack.id) {
            ctx.settings = values.clone();
        }
        // A pack can gain a dropdown after the user's config was written (Wallpaper Engine got the
        // "binary" choice that way): fill anything the config does not know yet with its default,
        // or steps using it would see an empty value and refuse to run.
        fill_defaults(&mut ctx.settings, pack);

        // Apply captures live — except a file read right after our own revert (see CARRY_WINDOW):
        // that app writes its config with a delay, so the read would record our own change as the
        // state before. A revert replays what the matching apply captured.
        if job.apply {
            for step in &pack.capture {
                let carried = carry
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() <= CARRY_WINDOW)
                    .and_then(|(_, caps)| caps.get(&pack.id))
                    .and_then(|map| map.get(&step.key))
                    .cloned()
                    .filter(|_| step.mechanism == "json_file");
                let value = match carried {
                    Some(value) => {
                        reused_carry = true;
                        info!(
                            "{}: reusing the capture for {} from the revert just before (that app writes its config with a delay)",
                            pack.id, step.key
                        );
                        value
                    }
                    None => match step.mechanism.as_str() {
                        "process_running" => {
                            if target_exe.is_empty() { "0" } else { "1" }.to_string()
                        }
                        "target_path" => target_exe.clone(),
                        "json_file" => step
                            .path
                            .as_deref()
                            .map(expand_env)
                            .and_then(|p| json_pointer(&p, step.pointer.as_deref().unwrap_or("/")))
                            .unwrap_or_default(),
                        other => {
                            warn!("{}: unknown capture mechanism '{other}'", pack.id);
                            String::new()
                        }
                    },
                };
                ctx.captured.insert(step.key.clone(), value);
            }
            captured_out.insert(pack.id.clone(), ctx.captured.clone());
        } else if let Some(prev) = job.captured.get(&pack.id) {
            ctx.captured = prev.clone();
        }

        if block.guard.target_running && target_exe.is_empty() {
            info!(
                "{}: skipped ({} is not running)",
                pack.name, target_name_or(pack)
            );
            continue;
        }
        if block.guard.captured_running
            && ctx.captured.get("was_running").map(|v| v.as_str()) != Some("1")
        {
            info!(
                "{}: skipped ({} was not running before the trigger)",
                pack.name,
                target_name_or(pack)
            );
            continue;
        }

        let setting_key = pack.settings.first().map(|s| s.key.as_str());
        let block_name = if job.apply { "apply" } else { "revert" };
        let mut result = run_block(
            block,
            &ctx,
            setting_key,
            &pack.id,
            block_name,
            pack.capabilities,
        );

        // Spec 2: the block's script runs after its steps, in the same context. A script may
        // capture values; they are journalled below together with the step captures.
        let script_src = if job.apply {
            pack.script.apply.as_deref()
        } else {
            pack.script.revert.as_deref()
        };
        if let Some(src) = script_src.filter(|s| !s.trim().is_empty()) {
            // The pack's script runs on the engine of its own capability set: only the
            // functions it was granted are on it, so a call to anything else fails as an
            // unknown function.
            match script::run_with_capabilities(src, &mut ctx, pack.capabilities) {
                Ok(lines) => {
                    if let Ok(done) = &mut result {
                        done.extend(lines);
                    }
                    info!("{}: script {} finished", pack.id, if job.apply { "apply" } else { "revert" });
                }
                Err(e) => {
                    warn!("{}: script {} failed: {e}", pack.id, if job.apply { "apply" } else { "revert" });
                    match &mut result {
                        Ok(_) => result = Err(e),
                        Err(previous) => previous.push_str(&format!("; {e}")),
                    }
                }
            }
        }

        // Past the guards: this pack's captures (steps and/or script) belong in the journal.
        if job.apply && result.is_ok() {
            // Feature 2 (2026-10-27): did it stick? Re-read each JSON capture now that the
            // block has run and compare it with the value captured before it. A pack whose write
            // was rejected (the app rewrote its config, the path moved) reads back unchanged —
            // that is exactly the silent failure the dashboard used to call "applied".
            let mut unverified = Vec::new();
            for step in &pack.capture {
                if step.mechanism != "json_file" {
                    continue;
                }
                let Some(before) = ctx.captured.get(&step.key) else {
                    continue;
                };
                if before.is_empty() {
                    continue; // nothing was there to compare against
                }
                let pointer = step.pointer.as_deref().unwrap_or("/");
                let read = || {
                    step.path
                        .as_deref()
                        .map(expand_env)
                        .and_then(|p| json_pointer(&p, pointer))
                        .unwrap_or_default()
                };
                // The app writes its config once it has handled the request - G-Helper takes
                // a hotkey in its UI thread and saves afterwards - so a read on the same
                // millisecond as the keystroke is a read of the old value. Poll for the
                // change for three seconds before calling it unverified.
                // A second and a half, not three: this blocks the pack worker, and the
                // worker is the one that would carry the revert when the app closes.
                let mut after = read();
                let mut waited = 0;
                while (&after == before || after.is_empty()) && waited < 6 {
                    std::thread::sleep(Duration::from_millis(250));
                    waited += 1;
                    after = read();
                }
                let waited_ms = waited * 250;
                if after.is_empty() {
                    unverified.push(format!(
                        "could not read {pointer} back after the apply, so the change is unverified"
                    ));
                } else if &after == before {
                    // Softer than "the pack had no effect": the pack did everything it was
                    // asked to do. It is the app's config that still shows the old value, and
                    // some apps only write it when they close.
                    unverified.push(format!(
                        "{pointer} still reads {after} {waited_ms} ms after the apply - the pack \
                         ran, but the app has not written the change yet"
                    ));
                }
            }
            for line in unverified {
                warn!("{}: {line}", pack.id);
                notes.push((pack.id.clone(), line));
            }
        }
        if job.apply {
            captured_out.insert(pack.id.clone(), ctx.captured.clone());
            // "applied" means the block was started. A pack that failed halfway has already changed
            // the machine, so it must be journalled (and reverted) like any other.
            applied_ids.push(pack.id.clone());
        }
        match result {
            Ok(lines) => {
                if !job.apply {
                    // a successful revert leaves the machine in the state the revert restored;
                    // remember what that was for an apply that follows soon after
                    reverted_captures.insert(pack.id.clone(), ctx.captured.clone());
                }
                for l in lines {
                    strings.push(format!("{}: {l}", pack.id));
                }
                info!(
                    "{}: {} (target: {})",
                    pack.name,
                    if job.apply { "applied" } else { "reverted" },
                    if target_name.is_empty() { "n/a" } else { &target_name }
                );
            }
            Err(e) => {
                warn!("{}: {} failed: {e}", pack.name, if job.apply { "apply" } else { "revert" });
                failed.push((pack.id.clone(), e));
            }
        }
    }

    // Carry the revert's values forward for a short window; a used carry is spent (one-shot).
    if job.apply {
        if reused_carry {
            *carry = None;
        }
    } else if !reverted_captures.is_empty() {
        *carry = Some((Instant::now(), reverted_captures));
    }

    PackOutcome {
        applied: job.apply,
        tag: job.tag.clone(),
        scoped: job
            .enabled
            .iter()
            .filter(|(_, enabled)| **enabled)
            .map(|(id, _)| id.clone())
            .collect(),
        applied_ids,
        values: job.values.clone(),
        captured: captured_out,
        succeeded: strings,
        failed,
        notes,
    }
}

fn target_name_or(pack: &Pack) -> String {
    pack.targets
        .process
        .first()
        .cloned()
        .unwrap_or_else(|| "target".to_string())
}

fn run_block(
    block: &StepBlock,
    ctx: &TemplateCtx,
    setting_key: Option<&str>,
    pack_id: &str,
    block_name: &str,
    caps: CapSet,
) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    for (index, step) in block.steps.iter().enumerate() {
        let label = format!("{block_name} step {index}");
        if let Some(line) = run_step(step, ctx, setting_key, pack_id, &label, caps)? {
            done.push(line);
        }
    }
    Ok(done)
}

/// Substitute a template. A placeholder the pack got wrong (a typo) is warned about: it would
/// otherwise end up in an argument or in a path as literal text.
fn substitute_checked(template: &str, ctx: &TemplateCtx, pack_id: &str) -> String {
    let (text, unknown) = substitute(template, ctx);
    if !unknown.is_empty() {
        warn!(
            "{}: unknown placeholder(s) {}, left as written",
            pack_id,
            unknown.join(", ")
        );
    }
    text
}

/// Fill dropdown values the user's config does not know yet with the pack's defaults, so a pack
/// that gains a setting keeps working on an older config (the Wallpaper Engine bug of 2026-10-18).
fn fill_defaults(settings: &mut HashMap<String, String>, pack: &Pack) {
    for setting in &pack.settings {
        if !settings.contains_key(&setting.key)
            && let Some(default) = &setting.default
        {
            settings.insert(setting.key.clone(), default.clone());
        }
    }
}

/// The exe a step runs: either its own `exe` template, or one picked from a dropdown via
/// `exe_from_setting` + `exe_map` (e.g. "auto / 64-bit / 32-bit" of a multi-binary app).
fn resolve_exe(step: &Step, ctx: &TemplateCtx, pack_id: &str) -> Result<String, String> {
    if let Some(key) = &step.exe_from_setting {
        let value = ctx.settings.get(key).cloned().unwrap_or_default();
        let template = step
            .exe_map
            .get(&value)
            .ok_or_else(|| format!("no exe mapped for {key} = '{value}'"))?;
        return Ok(substitute_checked(template, ctx, pack_id));
    }
    Ok(substitute_checked(step.exe.as_deref().unwrap_or(""), ctx, pack_id))
}

/// Run one step. `Ok(None)` means the step did not apply (its `when` guard, or the app was
/// already running); `Ok(Some(line))` is the log line of the step that ran. The templates are
/// substituted once: the step and its log line use the same values. `label` names the step
/// in an error message ("apply step 0").
fn run_step(
    step: &Step,
    ctx: &TemplateCtx,
    setting_key: Option<&str>,
    pack_id: &str,
    label: &str,
    caps: CapSet,
) -> Result<Option<String>, String> {
    // A step is a host call written as data, so it is gated like one. `Pack::validate`
    // refuses such a pack when it is parsed; this is the same check where the mechanism is
    // actually run, so a pack that reached the worker by another path cannot reach the
    // machine either.
    if let Some(capability) = Capability::of_mechanism(&step.mechanism)
        && !caps.grants(capability)
    {
        return Err(format!(
            "{label}: {} REFUSED: it needs the '{}' capability, which this pack was not \
             granted (the pack asked for: {})",
            step.mechanism,
            capability.name(),
            caps.asked_for()
        ));
    }
    if let Some(cond) = &step.when
        && !cond.matches(ctx, setting_key)
    {
        return Ok(None); // step does not apply to this configuration
    }
    if let Some(ms) = step.sleep_ms
        && ms > 0
    {
        std::thread::sleep(std::time::Duration::from_millis(ms.min(10_000)));
    }
    match step.mechanism.as_str() {
        "cli" => {
            let exe = resolve_exe(step, ctx, pack_id)?;
            let args: Vec<String> = step
                .args
                .iter()
                .map(|a| substitute_checked(a, ctx, pack_id))
                .collect();
            run_cli(&exe, &args, step.expect_exit)?;
            Ok(Some(format!("cli {exe} {}", args.join(" "))))
        }
        "process_start" => {
            let exe = resolve_exe(step, ctx, pack_id)?;
            if exe.is_empty() {
                return Err("process_start: no exe".into());
            }
            // Never start a second copy: after a restart-revert the app may already be back.
            let base = exe
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&exe)
                .to_ascii_lowercase();
            if snapshot_targets().contains_key(&base) {
                info!("process_start: {base} is already running — nothing to start");
                return Ok(None);
            }
            start_process(&exe)?;
            Ok(Some("process_start".to_string()))
        }
        "process_kill" => {
            let exe = resolve_exe(step, ctx, pack_id)?;
            let name = exe
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&exe)
                .to_ascii_lowercase();
            if name.is_empty() {
                return Err("process_kill: no target".into());
            }
            kill_by_name(&name)?;
            Ok(Some("process_kill".to_string()))
        }
        "registry_set" => {
            let reg = step.registry.as_ref().ok_or("registry_set: no registry block")?;
            set_registry_dword(&reg.path, &reg.name, reg.dword)?;
            Ok(Some(format!(
                "registry {}\\{} = {}",
                reg.path, reg.name, reg.dword
            )))
        }
        "hotkey" => {
            // A value the step's map does not know is a failure, not a no-op: the step
            // would do nothing, and a pack that does nothing must not report success.
            let chord = resolve_chord(step, ctx, pack_id, label)?;
            send_chord(&chord)?;
            Ok(Some(format!("hotkey {chord}")))
        }
        other => Err(format!("unsupported mechanism '{other}'")),
    }
}

/// Terminate every process with this exe file name (lowercase).
fn kill_by_name(name: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_TERMINATE, TerminateProcess,
    };
    let targets = snapshot_targets();
    let Some((path, pid)) = targets.get(name) else {
        return Err(format!("{name} is not running"));
    };
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, *pid) };
    if handle.is_null() {
        return Err(format!("cannot open {name} (pid {pid}) for termination"));
    }
    let ok = unsafe { TerminateProcess(handle, 0) } != 0;
    unsafe { CloseHandle(handle) };
    if ok {
        info!("killed {name} ({path}, pid {pid})");
        Ok(())
    } else {
        Err(format!("TerminateProcess failed for {name} (pid {pid})"))
    }
}

/// Write a REG_DWORD (used to clear a crash flag before relaunching an app).
fn set_registry_dword(path: &str, name: &str, value: u32) -> Result<(), String> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
        RegCloseKey, RegCreateKeyExW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };

    let (root, sub) = if let Some(rest) = path
        .strip_prefix("HKEY_CURRENT_USER\\")
        .or_else(|| path.strip_prefix("HKCU\\"))
    {
        (HKEY_CURRENT_USER, rest)
    } else if let Some(rest) = path
        .strip_prefix("HKEY_LOCAL_MACHINE\\")
        .or_else(|| path.strip_prefix("HKLM\\"))
    {
        (HKEY_LOCAL_MACHINE, rest)
    } else {
        return Err(format!(
            "registry_set: only HKCU/HKLM paths are allowed, got '{path}'"
        ));
    };

    unsafe {
        let sub_w = wide(sub);
        let name_w = wide(name);
        let mut key = HKEY::default();
        let rc = RegCreateKeyExW(
            root,
            PCWSTR(sub_w.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if rc.0 != 0 {
            return Err(format!("RegCreateKeyExW {path}: error {}", rc.0));
        }

        let bytes = value.to_le_bytes();
        let rc = RegSetValueExW(
            key,
            PCWSTR(name_w.as_ptr()),
            None,
            REG_DWORD,
            Some(&bytes),
        );
        let _ = RegCloseKey(key);
        if rc.0 != 0 {
            return Err(format!("RegSetValueExW {path}\\{name}: error {}", rc.0));
        }
        info!("registry_set: {path}\\{name} = {value}");
        Ok(())
    }
}

/// Start a process (used to bring back something we killed).
fn start_process(exe: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let path = expand_env(exe);
    Command::new(&path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|child| {
            info!("started {} (pid {})", path, child.id());
        })
        .map_err(|e| format!("start {path}: {e}"))
}

/// The key chord a hotkey step sends. The map key comes from a dropdown value
/// (`keys_from_mode`) or from a captured value (`keys_from_captured`).
///
/// A value the step's `map` does not know is a failure: the step would send nothing, and a
/// pack that does nothing must not report success. The error names the pack, the step and
/// the value, so the dashboard can say what went wrong.
fn resolve_chord(
    step: &Step,
    ctx: &TemplateCtx,
    pack_id: &str,
    label: &str,
) -> Result<String, String> {
    if let Some(expr) = &step.keys_from_mode {
        let key = substitute_checked(expr, ctx, pack_id);
        return step.map.get(&key).cloned().ok_or_else(|| {
            format!("{pack_id}: {label}: no hotkey is mapped for the setting value '{key}'")
        });
    }
    if let Some(key) = &step.keys_from_captured {
        let value = ctx.captured.get(key).cloned().unwrap_or_default();
        return step.map.get(&value).cloned().ok_or_else(|| {
            format!("{pack_id}: {label}: no hotkey is mapped for the captured {key}='{value}'")
        });
    }
    Err(format!(
        "{pack_id}: {label}: the hotkey step has neither keys_from_mode nor keys_from_captured"
    ))
}

/// How long a pack's CLI call may run. The pack worker is serial, so a child that never exits
/// would block every future apply and revert while the daemon still looks healthy.
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of a child's output is kept, per stream.
const CLI_OUTPUT_CAP: usize = 64 * 1024;

fn run_cli(exe: &str, args: &[String], expect_exit: Option<i32>) -> Result<(), String> {
    run_cli_within(exe, args, expect_exit, CLI_TIMEOUT)
}

/// Run a program and wait for it, but not longer than `limit`. A child that outlives the limit
/// is killed: the pack worker must never stay blocked on it.
fn run_cli_within(
    exe: &str,
    args: &[String],
    expect_exit: Option<i32>,
    limit: Duration,
) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    let mut child = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {exe}: {e}"))?;

    // both pipes are drained on their own threads: a full pipe buffer would block the child
    let out = child.stdout.take().map(spawn_drain);
    let err = child.stderr.take().map(spawn_drain);

    // Wait on the child's process handle instead of polling it: the pack worker is
    // serial, so a 10 ms poll was 100 wake-ups per call for nothing, and a slow call
    // held every later pack up for the whole limit.
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::WaitForSingleObject;

    let handle = HANDLE(child.as_raw_handle() as *mut core::ffi::c_void);
    let deadline = Instant::now() + limit;
    let status = loop {
        let now = Instant::now();
        if now >= deadline {
            break None;
        }
        // The wait is cut at the deadline, so the child can never outlive the limit.
        let millis = (deadline - now).as_millis().min(u32::MAX as u128 - 1) as u32 + 1;
        let wait = unsafe { WaitForSingleObject(handle, millis) };
        if wait == WAIT_OBJECT_0 {
            // The child has exited: take its exit code.
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                // The handle says it is done but the code is not there yet (it does not
                // happen in practice): look again.
                Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => return Err(format!("wait {exe}: {e}")),
            }
            continue;
        }
        if wait == WAIT_TIMEOUT {
            break None;
        }
        // The wait itself failed (WAIT_FAILED): do not spin on it.
        warn!(
            "WaitForSingleObject failed for {exe} (error {})",
            unsafe { GetLastError() }.0
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }

    // the readers end when the pipes close; a child that left a grandchild behind may keep them
    // open, so they are waited for briefly and then left to finish on their own
    let out = out
        .and_then(|rx| rx.recv_timeout(Duration::from_millis(500)).ok())
        .unwrap_or_default();
    let err = err
        .and_then(|rx| rx.recv_timeout(Duration::from_millis(500)).ok())
        .unwrap_or_default();
    log_output(exe, "stdout", &out);
    log_output(exe, "stderr", &err);

    let Some(status) = status else {
        return Err(format!(
            "{exe} timed out after {} s and was killed",
            limit.as_secs()
        ));
    };

    let code = status.code().unwrap_or(-1);
    match expect_exit {
        Some(want) if want != code => Err(format!("{exe} exited {code}, expected {want}")),
        Some(_) => Ok(()),
        None => {
            // Many tools exit non-zero without meaning failure (a warning code), so without
            // an explicit `expect_exit` the exit code is information, not a failure.
            if !status.success() {
                info!("{exe} exited {code} (no expect_exit set — information only)");
            }
            Ok(())
        }
    }
}

/// Drain a pipe on its own thread; the bytes arrive over the channel.
fn spawn_drain<R: std::io::Read + Send + 'static>(
    pipe: R,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(drain_pipe(pipe));
    });
    rx
}

/// Read a pipe to its end. At most `CLI_OUTPUT_CAP` bytes are kept, but the pipe keeps being
/// drained: a chatty child must not block on a full buffer.
fn drain_pipe(mut pipe: impl std::io::Read) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < CLI_OUTPUT_CAP {
                    let take = n.min(CLI_OUTPUT_CAP - out.len());
                    out.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    out
}

/// One line of a child's output, for the log (never more than 200 characters).
fn log_output(exe: &str, stream: &str, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let Some(line) = text.lines().find(|l| !l.trim().is_empty()) else {
        return;
    };
    let line: String = line.trim().chars().take(200).collect();
    info!("{exe} {stream}: {line}");
}

// ---------------------------------------------------------------------------
// Rainmeter layouts

/// Every layout folder name under %APPDATA%\Rainmeter\Layouts (content-unchecked).
pub fn rainmeter_layouts() -> Vec<String> {
    let Some(dir) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
        return Vec::new();
    };
    let root = dir.join("Rainmeter").join("Layouts");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    out.sort();
    out
}

/// A layout is empty when its Rainmeter.ini has no skin section with `Active >= 1`.
/// (Rainmeter's own "empty layout" save is a 0-byte file, so this is the real test.)
pub fn layout_is_empty(name: &str) -> bool {
    let Some(dir) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
        return false;
    };
    let ini = dir
        .join("Rainmeter")
        .join("Layouts")
        .join(name)
        .join("Rainmeter.ini");
    let Ok(bytes) = std::fs::read(&ini) else {
        return false; // missing file cannot be loaded at all
    };
    let text = decode_ini(&bytes);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("active")
            && let Some(value) = rest.trim_start().strip_prefix('=')
            && value.trim().parse::<i32>().map(|v| v >= 1).unwrap_or(false)
        {
            return false;
        }
    }
    true
}

fn decode_ini(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (chunks, _) = bytes[2..].as_chunks::<2>();
        let words: Vec<u16> = chunks.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&words)
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The user's rule: use a layout that is genuinely empty; if none exists create one called
/// `Empty`; if that name is taken *with skins in it*, create `Empty (ArmouryDaemon)` instead.
/// Never overwrites anything. Returns the layout name to use.
pub fn ensure_empty_layout() -> Option<String> {
    let layouts = rainmeter_layouts();

    // 1. an existing layout that is truly empty (prefer one literally called Empty)
    if let Some(name) = layouts
        .iter()
        .find(|n| n.eq_ignore_ascii_case("Empty") && layout_is_empty(n))
    {
        return Some(name.clone());
    }
    if let Some(name) = layouts.iter().find(|l| layout_is_empty(l))
        && !name.eq_ignore_ascii_case("@Backup")
    {
        return Some(name.clone());
    }

    // 2/3. create one, respecting an occupied name
    let dir = std::env::var_os("APPDATA").map(std::path::PathBuf::from)?;
    let root = dir.join("Rainmeter").join("Layouts");
    let taken = |n: &str| layouts.iter().any(|l| l.eq_ignore_ascii_case(n));
    let name = if !taken("Empty") {
        "Empty".to_string()
    } else if !taken("Empty (ArmouryDaemon)") {
        "Empty (ArmouryDaemon)".to_string()
    } else {
        return None; // both names taken by non-empty layouts; leave it to the user
    };

    let folder = root.join(&name);
    std::fs::create_dir_all(&folder).ok()?;
    // Rainmeter writes a 0-byte Rainmeter.ini for an empty layout; a UTF-16 BOM is equally valid.
    std::fs::write(folder.join("Rainmeter.ini"), [0xFF, 0xFE]).ok()?;
    info!("created an empty Rainmeter layout named '{name}'");
    Some(name)
}

// ---------------------------------------------------------------------------
// Dropdown discovery

/// Fill every dropdown that declares a `discover` block from the machine it runs on. Pure
/// per pack: a pack that declares nothing keeps the values in its manifest, so this is the
/// only place that knows a dropdown can be dynamic at all.
pub fn apply_discovery(installed: &mut [Installed]) {
    for p in installed.iter_mut() {
        let id = p.pack.id.clone();
        for setting in p.pack.settings.iter_mut() {
            let Some(rule) = setting.discover.clone() else {
                continue;
            };
            let mut values = if rule.from == "folder" {
                rule.path.as_deref().map(folder_names).unwrap_or_default()
            } else {
                // The schema refuses an unknown `from`; a pack that reached here was edited
                // on disk after it was parsed. Say so rather than silently dropping values.
                warn!(
                    "{id}: dropdown '{}' uses unknown discovery '{}' — left as declared",
                    setting.key, rule.from
                );
                continue;
            };
            values.retain(|v| !rule.exclude.iter().any(|x| x.eq_ignore_ascii_case(v)));
            let ensured = rule.ensure.as_deref().and_then(ensure_value);
            if let Some(name) = &ensured
                && !values.iter().any(|v| v.eq_ignore_ascii_case(name))
            {
                values.push(name.clone());
            }
            values.extend(rule.append.iter().cloned());
            // Nothing on this machine to offer: the declared list is the fallback, not an
            // empty dropdown with no way back.
            if values.is_empty() {
                info!(
                    "{id}: nothing to discover for '{}' — keeping the declared values",
                    setting.key
                );
                continue;
            }
            info!(
                "{id}: dropdown '{}' offers {}",
                setting.key,
                values.join(", ")
            );
            setting.values = values;
            if setting.default.is_none() {
                setting.default = ensured;
            }
        }
    }
}

/// The sub-folder names under `path` (content-unchecked), sorted. `%NAME%` expands from the
/// environment, so a manifest can name `%APPDATA%\Rainmeter\Layouts` without hardcoding a
/// user profile. A path that cannot be read yields nothing: the caller falls back.
fn folder_names(path: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(expand_env(path)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    out.sort();
    out
}

/// Run a preparation hook a pack named in its `discover` block. The name is validated when
/// the pack is parsed, so this match is exhaustive over the schema: adding a preparation
/// means adding it here and to `ALLOWED_ENSURE`.
fn ensure_value(hook: &str) -> Option<String> {
    match hook {
        "rainmeter_empty_layout" => ensure_empty_layout(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Install / remove / restart reconciliation

/// The `packs` folder the daemon reads and writes: the first search dir that already has
/// one, otherwise `packs` beside the executable (the install folder).
pub fn packs_root(search_dirs: &[PathBuf]) -> PathBuf {
    for dir in search_dirs {
        let root = dir.join("packs");
        if root.is_dir() {
            return root;
        }
    }
    search_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("packs")
}

/// The `packs` folder, created when it is missing. A fresh install has no packs until
/// the user installs some, but the folder has to exist for them to land in, and the
/// daemon starts with an empty list instead of failing.
///
/// The folder that is already there wins: a dev build keeps using the repository's own
/// `packs` folder and never grows a second one next to the executable. Returns the
/// folder that was created, if one was.
pub fn ensure_packs_dir(search_dirs: &[PathBuf]) -> Option<PathBuf> {
    let root = packs_root(search_dirs);
    if root.is_dir() {
        return None;
    }
    match std::fs::create_dir_all(&root) {
        Ok(()) => Some(root),
        Err(e) => {
            warn!("cannot create the packs folder {}: {e}", root.display());
            None
        }
    }
}

/// Install (or update) a pack from a local `pack.json`, or from a folder holding one.
/// Returns `(id, updated?)`. Only `pack.json` is copied — packs carry no code.
pub fn install_from_path(search_dirs: &[PathBuf], source: &Path) -> Result<(String, bool), String> {
    let manifest = if source.is_dir() {
        source.join("pack.json")
    } else {
        source.to_path_buf()
    };
    if !manifest.is_file() {
        return Err(format!(
            "{} is not a pack.json or a folder containing one",
            source.display()
        ));
    }
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("cannot read {}: {e}", manifest.display()))?;
    install_text(search_dirs, &text)
}

/// Install a pack from raw JSON bytes (the catalog path — the bytes were verified against the
/// manifest just before). Validation and writing are identical to a local file.
pub fn install_from_bytes(install_dirs: &[PathBuf], bytes: &[u8]) -> Result<(String, bool), String> {
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|e| format!("the pack is not UTF-8 text ({e})"))?;
    install_text(install_dirs, &text)
}

/// Install a pack from its JSON text.
pub fn install_text(search_dirs: &[PathBuf], text: &str) -> Result<(String, bool), String> {
    let text = strip_bom(text).to_string();
    let pack = Pack::parse(&text)?; // validates spec/settings/steps
    check_scripts(&pack)?;          // a scripted pack must compile before it lands on disk
    require_engine(&pack)?;         // a pack this daemon is too old to run never lands on disk

    let root = packs_root(search_dirs);
    let target = root.join(&pack.id);
    if !is_safe_id(&pack.id) {
        return Err(format!("pack id '{}' is not a safe folder name", pack.id));
    }
    let existed = target.is_dir();
    std::fs::create_dir_all(&target)
        .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    std::fs::write(target.join("pack.json"), &text)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    info!(
        "pack {} {} into {}",
        pack.id,
        if existed { "updated" } else { "installed" },
        target.display()
    );
    Ok((pack.id, existed))
}

/// Refuse a pack this daemon is too old to run. A pack may need a host function the daemon
/// does not have (`min_engine`); installing it anyway only moves the failure to apply time.
/// The message names the pack, the version it needs and the version of the daemon that was
/// asked to install it.
fn require_engine(pack: &Pack) -> Result<(), String> {
    let Some(required) = pack.min_engine.as_deref() else {
        return Ok(()); // no minimum: every daemon may run it
    };
    match version_at_least(DAEMON_VERSION, required) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "pack '{}' ({}) needs daemon {required} or newer; this daemon is {DAEMON_VERSION}",
            pack.id, pack.name
        )),
        Err(e) => Err(format!("pack '{}' has an unreadable min_engine: {e}", pack.id)),
    }
}

/// Remove an installed pack folder (only when it really holds that pack).
pub fn remove_id(search_dirs: &[PathBuf], id: &str) -> Result<String, String> {
    if !is_safe_id(id) {
        return Err(format!("invalid pack id '{id}'"));
    }
    let root = packs_root(search_dirs);
    let dir = root.join(id);
    if !dir.join("pack.json").is_file() {
        return Err(format!("pack '{id}' is not installed in {}", root.display()));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
    info!("pack {id} removed ({})", dir.display());
    Ok(id.to_string())
}

/// A Windows folder name only: no separators, no drive prefix (`C:evil` escapes the root when
/// joined), no traversal, no leading/trailing dot (Windows strips those, so they would not round
/// trip through the file system). Shared with `helpers`, whose module folders take the same
/// shape.
pub(crate) fn is_safe_id(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() || id.len() > 64 || id.starts_with('.') || id.ends_with(['.', ' ']) {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
}

/// The revert a fresh daemon start must run: a restart counts as "revert everything"
/// (decision A3). The journal's last entry carries the applied ids, the dropdown values and
/// the captured values from that apply, so the revert can be replayed exactly. `None` when
/// nothing is pending.
pub fn journal_pending_revert() -> Option<PackJob> {
    journal_pending_revert_in(&armoury_core::config::config_dir())
}

/// Same, against an explicit folder (so it can be tested without touching the live config).
pub fn journal_pending_revert_in(config_dir: &Path) -> Option<PackJob> {
    let path = config_dir.join("journal.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let log: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;
    let last = log.last()?;
    if last.get("applied").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let ids: Vec<String> = last
        .get("packs")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if ids.is_empty() {
        return None;
    }

    // { pack: { key: value } } out of the journal entry
    let nested = |key: &str| -> HashMap<String, HashMap<String, String>> {
        last.get(key)
            .and_then(|v| v.as_object())
            .map(|outer| {
                outer
                    .iter()
                    .filter_map(|(pack, inner)| {
                        let inner = inner.as_object()?;
                        let map: HashMap<String, String> = inner
                            .iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                            .collect();
                        Some((pack.clone(), map))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    Some(PackJob {
        apply: false,
        tag: None,
        enabled: ids.iter().map(|id| (id.clone(), true)).collect(),
        values: nested("values"),
        captured: nested("captured"),
    })
}

/// Expand %VARS% in a pack path.
fn expand_env(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Read a value out of a JSON file with a simple `/a/b` pointer.
fn json_pointer(path: &str, pointer: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut cur = &value;
    for part in pointer.split('/').filter(|p| !p.is_empty()) {
        cur = cur.get(part)?;
    }
    Some(match cur {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Host API for scripts (spec 2) and the load-time script check

/// `ctx.run_cli(exe, args)` — same rules as a `cli` step: direct spawn, no shell.
pub(crate) fn script_cli(exe: &str, args: &[String]) -> Result<(), String> {
    run_cli(exe, args, None)
}

pub(crate) fn script_process_running(name: &str) -> bool {
    snapshot_targets().contains_key(&name.to_ascii_lowercase())
}

pub(crate) fn script_process_kill(name: &str) -> Result<(), String> {
    kill_by_name(&name.to_ascii_lowercase())
}

pub(crate) fn script_process_start(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("process_start: empty path".into());
    }
    start_process(path)
}

pub(crate) fn script_json_pointer(path: &str, pointer: &str) -> Option<String> {
    json_pointer(path, pointer)
}

/// `ctx.windows_of(pid)` — the visible, unowned, non-tool windows of a process.
pub(crate) fn script_windows_of(pid: u32) -> Vec<isize> {
    crate::killswitch::windows_of(pid)
}

/// The windows a host call hid, with the visibility they had before: the exact state a
/// revert has to put back. A window that is hidden twice keeps the state it had before
/// the first hide.
static HIDDEN_WINDOWS: Mutex<Vec<(isize, bool)>> = Mutex::new(Vec::new());

/// `ctx.window_hide(hwnd)` — hide a window and report whether it was visible before.
pub(crate) fn script_window_hide(hwnd: isize) -> bool {
    let mut hidden = HIDDEN_WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    // A handle that no longer names a window is stale: the window was closed, and the
    // handle may belong to another window now. The state we recorded is then about a
    // window that is not there any more, so it must not be reported.
    hidden.retain(|(w, _)| *w != hwnd || window_exists(*w));
    if let Some((_, was)) = hidden.iter().find(|(w, _)| *w == hwnd) {
        return *was; // already hidden by us: the state we found is the one that counts
    }
    let was = window_visible(hwnd);
    unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
    }
    hidden.push((hwnd, was));
    was
}

/// `ctx.window_show(hwnd)` — show a window and report whether it was visible before.
pub(crate) fn script_window_show(hwnd: isize) -> bool {
    let was = window_visible(hwnd);
    unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut _), SW_SHOW);
    }
    HIDDEN_WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(w, _)| *w != hwnd);
    was
}

/// Is the window visible right now?
fn window_visible(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindowVisible(HWND(hwnd as *mut _)).as_bool() }
}

/// Does this handle still name a window? A window that was closed leaves the handle
/// behind, and Windows may hand the same handle to another window later.
fn window_exists(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindow(Some(HWND(hwnd as *mut _))).as_bool() }
}

/// `ctx.audio_sessions()` — the audio sessions of the machine, as "pid:state" strings.
pub(crate) fn script_audio_sessions() -> Vec<String> {
    crate::killswitch::session_states()
        .into_iter()
        .map(|(pid, state)| format!("{pid}:{state}"))
        .collect()
}

/// `ctx.audio_mute(pid, on)` — mute (or unmute) the audio sessions of a process.
pub(crate) fn script_audio_mute(pid: u32, muted: bool) -> String {
    crate::killswitch::mute_pid(pid, muted)
}

/// `ctx.audio_mute_all(on)` — mute (or unmute) every output device.
pub(crate) fn script_audio_mute_all(muted: bool) -> Result<String, String> {
    crate::overlay::mute_all_host(muted)
}

/// What a pack asked the daemon to do with the idle overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayRequest {
    /// Show the overlay — the daemon's idle mode.
    Show,
    /// Take the overlay away again.
    Hide,
}

/// The overlay request the pack worker made, waiting for the daemon's main loop.
///
/// The pack worker cannot show the overlay itself: the black windows belong to the thread
/// that pumps their messages, and that is the daemon's main loop. The request is carried
/// over to that loop, which does the work and keeps owning the crash-recovery ledger and
/// the keypress dismissal.
///
/// **One pending request, not a queue.** Every request the daemon carries out costs a black
/// window per monitor, a write and a delete of `idle.marker`, and a pack job; a script with a
/// thousand `overlay_show`/`overlay_hide` pairs in it would keep the daemon thrashing for
/// minutes. Only the last request is still worth carrying out — the ones before it are
/// asking for a state the machine is about to leave anyway.
static OVERLAY_REQUESTS: Mutex<Option<OverlayRequest>> = Mutex::new(None);

/// Ask the daemon to show or take away the idle overlay. The request replaces the one that
/// was still waiting: the daemon acts on the newest wish, never on a backlog of them.
pub fn request_overlay(request: OverlayRequest) {
    let mut pending = OVERLAY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    *pending = Some(request);
}

/// The overlay request that is waiting, taken away so the daemon does it once.
pub fn take_overlay_request() -> Option<OverlayRequest> {
    let mut pending = OVERLAY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    pending.take()
}

/// Reject a pack whose scripts do not compile, before it is loaded or installed.
pub(crate) fn check_scripts(pack: &Pack) -> Result<(), String> {
    for (block, src) in [("apply", &pack.script.apply), ("revert", &pack.script.revert)] {
        if let Some(src) = src.as_deref().filter(|s| !s.trim().is_empty()) {
            script::compile(src).map_err(|e| format!("{block} {e}"))?;
        }
    }
    Ok(())
}

/// exe file name (lowercase) -> (full path, pid) for every process we can query.
fn snapshot_targets() -> HashMap<String, (String, u32)> {
    let mut map = HashMap::new();
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() || snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return map;
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
            let key = name.to_ascii_lowercase();
            if let std::collections::hash_map::Entry::Vacant(slot) = map.entry(key)
                && let Some(path) = crate::watch::process_path(pid)
            {
                slot.insert((path, pid));
            }
        }
        ok = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snap) };
    map
}

// ---------------------------------------------------------------------------
// Input synthesis

/// How long to wait before the second attempt at an injection.
const SEND_RETRY_DELAY: Duration = Duration::from_millis(250);

/// One injection attempt: how many events the OS took, and the Win32 error it left
/// behind (meaningful only when it took fewer than it was given).
#[derive(Clone, Copy)]
struct Injection {
    sent: u32,
    total: u32,
    error: u32,
}

pub fn send_chord(chord: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    };

    let keys = parse_chord(chord)?;
    let mut inputs: Vec<INPUT> = Vec::with_capacity(keys.len() * 2);
    let make = |vk: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    for vk in &keys {
        inputs.push(make(*vk, 0));
    }
    for vk in keys.iter().rev() {
        inputs.push(make(*vk, KEYEVENTF_KEYUP));
    }

    let total = inputs.len() as u32;
    let mut send = || {
        let sent =
            unsafe { SendInput(total, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if sent == total {
            return Injection {
                sent,
                total,
                error: 0,
            };
        }
        // The error is read here and nowhere else: any other Win32 call in between
        // would overwrite it, and then the message would name the wrong error.
        let error = unsafe { GetLastError() }.0;
        Injection { sent, total, error }
    };
    inject_with_retry(&mut send, SEND_RETRY_DELAY)
}

/// Send the chord's events, and try once more if the OS refused them.
///
/// A refused injection is not a lost step: the input desktop may not be ready yet (the
/// daemon had just started and the hotkey hook did not exist yet). So the first failure
/// is reported with the Win32 error it carried, and the second attempt decides the step.
///
/// `send` is the injection itself, so the retry can be tested without a desktop.
fn inject_with_retry(send: &mut dyn FnMut() -> Injection, delay: Duration) -> Result<(), String> {
    let first = send();
    if first.sent == first.total {
        return Ok(());
    }
    let first_error = describe_win32_error(first.error);
    // The input desktop may not be ready yet: give it a moment and try once more.
    std::thread::sleep(delay);
    let second = send();
    if second.sent == second.total {
        warn!(
            "SendInput delivered {}/{} events ({}); the retry worked",
            first.sent, first.total, first_error
        );
        return Ok(());
    }
    Err(format!(
        "SendInput delivered {}/{} events ({}); the retry delivered {}/{} events ({})",
        first.sent,
        first.total,
        first_error,
        second.sent,
        second.total,
        describe_win32_error(second.error)
    ))
}

/// The Win32 error of a refused injection, in words: "error 5: access denied".
fn describe_win32_error(code: u32) -> String {
    match win32_error_text(code) {
        Some(text) => format!("error {code}: {text}"),
        None => format!("error {code}"),
    }
}

/// A short name for the Win32 errors an injection can be refused with, so the log says
/// what happened and not only a number.
fn win32_error_text(code: u32) -> Option<&'static str> {
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
        ERROR_NOT_ENOUGH_MEMORY, ERROR_SHARING_VIOLATION, WIN32_ERROR,
    };
    match WIN32_ERROR(code) {
        c if c == ERROR_ACCESS_DENIED => Some("access denied"),
        c if c == ERROR_BUSY => Some("the device is busy"),
        c if c == ERROR_INVALID_HANDLE => Some("invalid handle"),
        c if c == ERROR_INVALID_PARAMETER => Some("invalid parameter"),
        c if c == ERROR_NOT_ENOUGH_MEMORY => Some("not enough memory"),
        c if c == ERROR_SHARING_VIOLATION => Some("the file is in use"),
        _ => None,
    }
}

/// "ctrl+shift+alt+f18" -> virtual key codes in order.
fn parse_chord(chord: &str) -> Result<Vec<u16>, String> {
    let mut keys = Vec::new();
    for part in chord.split('+') {
        let p = part.trim().to_ascii_lowercase();
        let vk: u16 = match p.as_str() {
            // left-specific modifier VKs: RegisterHotKey (G-Helper) matches these reliably
            "ctrl" | "control" => 0xA2, // VK_LCONTROL
            "shift" => 0xA0,            // VK_LSHIFT
            "alt" | "menu" => 0xA4,     // VK_LMENU
            "win" | "lwin" => 0x5B,
            "scrolllock" | "scroll" => 0x91,
            _ if p.len() >= 2 && p.starts_with('f') => {
                let n: u16 = p[1..].parse().map_err(|_| format!("bad key '{p}'"))?;
                if !(1..=24).contains(&n) {
                    return Err(format!("bad function key '{p}'"));
                }
                0x70 + n - 1
            }
            _ if p.len() == 1 => {
                let c = p.chars().next().unwrap();
                if c.is_ascii_alphabetic() {
                    0x41 + (c.to_ascii_uppercase() as u16 - 'A' as u16)
                } else if c.is_ascii_digit() {
                    0x30 + (c as u16 - '0' as u16)
                } else {
                    return Err(format!("bad key '{p}'"));
                }
            }
            _ => return Err(format!("unknown key '{p}'")),
        };
        keys.push(vk);
    }
    if keys.is_empty() {
        return Err("empty chord".into());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    #[test]
    fn the_packs_folder_is_created_when_it_is_missing() {
        let scratch = Scratch::dir("packs-dir");
        let dir = scratch.root().to_path_buf();
        let search = vec![dir.clone()];
        let created = ensure_packs_dir(&search).expect("the folder is created");
        assert_eq!(created, dir.join("packs"));
        assert!(dir.join("packs").is_dir(), "the folder is there now");
        // the second call finds the folder and does nothing
        assert!(ensure_packs_dir(&search).is_none());
        // the scratch folder goes away with the guard
    }

    #[test]
    fn a_packs_folder_that_is_already_there_wins() {
        // the dev build: the repository's own packs folder must not be shadowed by a
        // second one next to the executable
        let scratch = Scratch::dir("packs-dir-2");
        let root = scratch.root().to_path_buf();
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(second.join("packs")).unwrap();
        let search = vec![first.clone(), second.clone()];
        assert!(
            ensure_packs_dir(&search).is_none(),
            "nothing is created when a folder is already there"
        );
        assert!(
            !first.join("packs").exists(),
            "the first folder of the search order is left alone"
        );
        assert_eq!(packs_root(&search), second.join("packs"));
        // the scratch folder goes away with the guard
    }

    /// A development build: the executable in `target\<profile>\`, the source checkout two
    /// levels up. The checkout is what tells the two apart.
    ///
    /// The scratch folder is returned as well: the guard that owns it removes the whole tree
    /// when the test ends, failed or not.
    fn dev_layout(label: &str) -> (Scratch, PathBuf, PathBuf) {
        let scratch = Scratch::dir(&format!("search-{label}"));
        let checkout = scratch.root().join("repo");
        let exe_dir = checkout.join("target").join("debug");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[package]\n").unwrap();
        (scratch, checkout, exe_dir)
    }

    /// A pack in a `packs` folder, so the folder is not empty.
    fn put_pack(packs: &Path, id: &str) {
        let folder = packs.join(id);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("pack.json"), SAMPLE).unwrap();
    }

    #[test]
    fn a_development_build_finds_the_packs_of_the_repository() {
        // The search order is the folder of the executable first and the source checkout
        // second: a build in `target\debug\` has to find the repository's own packs.
        let (_scratch, checkout, exe_dir) = dev_layout("dev");
        put_pack(&checkout.join("packs"), "sample");
        let search = usable_search_dirs(&[exe_dir.clone(), checkout.clone()]);

        assert_eq!(
            search,
            vec![exe_dir.clone(), checkout.clone()],
            "the folder of the executable and the checkout are both searched"
        );
        assert_eq!(
            packs_root(&search),
            checkout.join("packs"),
            "the repository's packs folder is the one a dev build uses"
        );
        let loaded = load(&search);
        assert_eq!(loaded.len(), 1, "the pack of the repository is loaded");
        assert_eq!(loaded[0].pack.id, "sample");

        // the scratch folder goes away with the guard
    }

    #[test]
    fn a_lone_executable_does_not_use_the_packs_of_its_grandparent() {
        // `C:\Users\Someone\ArmouryDaemon.exe` must not fall back to `C:\packs`: the
        // grandparent is only there for a development build, and a development build is a
        // source checkout. A folder that is not one is nobody's business.
        let scratch = Scratch::dir("search-lone");
        let root = scratch.root().to_path_buf();
        let exe_dir = root.join("Users").join("Someone");
        std::fs::create_dir_all(&exe_dir).unwrap();
        // the grandparent has a packs folder, and it is not a checkout
        put_pack(&root.join("packs"), "stray");
        let search = usable_search_dirs(&[exe_dir.clone(), root.clone()]);

        assert_eq!(
            search,
            vec![exe_dir.clone()],
            "the grandparent is not a checkout, so it is not searched"
        );
        assert_ne!(
            packs_root(&search),
            root.join("packs"),
            "the packs of the grandparent must not be used"
        );
        assert_eq!(
            packs_root(&search),
            exe_dir.join("packs"),
            "the folder of the executable is used instead"
        );
        assert!(
            load(&search).is_empty(),
            "the packs of the grandparent must not be loaded"
        );
        // and the folder that is used is the one next to the executable
        assert_eq!(ensure_packs_dir(&search), Some(exe_dir.join("packs")));

        // the scratch folder goes away with the guard
    }

    #[test]
    fn a_checkout_next_to_the_executable_is_used() {
        // The rule is about what the folder is, not about where it is: a folder with a
        // `Cargo.toml` in it is a source checkout, and a source checkout holds the packs.
        let scratch = Scratch::dir("search-checkout");
        let root = scratch.root().to_path_buf();
        let exe_dir = root.join("Apps").join("ArmouryDaemon");
        let checkout = root.join("repo");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[package]\n").unwrap();
        put_pack(&checkout.join("packs"), "sample");
        let search = usable_search_dirs(&[exe_dir.clone(), checkout.clone()]);

        assert_eq!(search, vec![exe_dir.clone(), checkout.clone()]);
        assert_eq!(packs_root(&search), checkout.join("packs"));
        assert_eq!(load(&search).len(), 1);

        // the scratch folder goes away with the guard
    }

    #[test]
    fn chord_parsing() {
        assert_eq!(
            parse_chord("ctrl+shift+alt+f18").unwrap(),
            vec![0xA2, 0xA0, 0xA4, 0x81]
        );
        assert_eq!(parse_chord("f16").unwrap(), vec![0x7F]);
        assert_eq!(parse_chord("a").unwrap(), vec![0x41]);
        assert!(parse_chord("ctrl+nope").is_err());
    }

    /// An injection that always fails the same way: (events taken, events asked for, error).
    fn refused_injection(error: u32) -> Injection {
        Injection {
            sent: 0,
            total: 8,
            error,
        }
    }

    #[test]
    fn a_refused_injection_names_the_win32_error() {
        // The message says which Win32 error happened: a bare "delivered 0/8 events"
        // leaves the log nothing to work with (2026-10-20: the G-Helper revert).
        let mut calls = 0;
        let mut send = || {
            calls += 1;
            refused_injection(5)
        };
        let err = inject_with_retry(&mut send, Duration::ZERO)
            .expect_err("a refused injection must fail the step");
        assert!(err.contains("SendInput delivered 0/8 events"), "{err}");
        assert!(err.contains("error 5"), "the code is in the message: {err}");
        assert!(err.contains("access denied"), "the code is in words: {err}");
    }

    #[test]
    fn a_refused_injection_is_tried_once_more() {
        // The first attempt is refused, the second one gets through: the step must not
        // be lost over a desktop that was not ready yet.
        let mut calls = 0;
        let mut send = || {
            calls += 1;
            if calls == 1 {
                refused_injection(5)
            } else {
                Injection {
                    sent: 8,
                    total: 8,
                    error: 0,
                }
            }
        };
        assert!(
            inject_with_retry(&mut send, Duration::ZERO).is_ok(),
            "the retry saves the step"
        );
        assert_eq!(calls, 2, "the injection is tried exactly twice");
    }

    #[test]
    fn the_second_attempt_is_what_gets_reported() {
        // Both attempts fail: the message carries both, so the log shows how far it got.
        let mut calls = 0;
        let mut send = || {
            calls += 1;
            if calls == 1 {
                Injection {
                    sent: 4,
                    total: 8,
                    error: 5,
                }
            } else {
                Injection {
                    sent: 2,
                    total: 8,
                    error: 87,
                }
            }
        };
        let err = inject_with_retry(&mut send, Duration::ZERO)
            .expect_err("two refused injections must fail the step");
        assert!(err.contains("4/8 events"), "the first attempt: {err}");
        assert!(err.contains("2/8 events"), "the retry: {err}");
        assert!(err.contains("error 5"), "the first error: {err}");
        assert!(err.contains("error 87"), "the retry's error: {err}");
        assert_eq!(calls, 2, "the injection is tried exactly twice");
    }

    #[test]
    fn a_delivered_injection_is_not_tried_again() {
        let mut calls = 0;
        let mut send = || {
            calls += 1;
            Injection {
                sent: 8,
                total: 8,
                error: 0,
            }
        };
        assert!(inject_with_retry(&mut send, Duration::ZERO).is_ok());
        assert_eq!(calls, 1, "a chord that got through is never sent twice");
    }

    #[test]
    fn an_unknown_win32_error_is_still_named() {
        // A code with no name in the table still reaches the log: the number is the point.
        let text = describe_win32_error(1234);
        assert!(text.contains("1234"), "{text}");
        assert_eq!(describe_win32_error(5), "error 5: access denied");
    }

    /// A child that never finishes on its own, for the deadline test.
    fn a_child_that_never_finishes() -> (String, Vec<String>) {
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let ps = PathBuf::from(&system)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if ps.is_file() {
            return (
                ps.display().to_string(),
                vec![
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    "Start-Sleep -Seconds 30".to_string(),
                ],
            );
        }
        let cmd = PathBuf::from(&system).join("System32").join("cmd.exe");
        (
            cmd.display().to_string(),
            vec!["/c".to_string(), "ping -n 30 127.0.0.1 > nul".to_string()],
        )
    }

    #[test]
    fn a_cli_call_has_a_deadline() {
        // a child that never exits must be killed: the pack worker is serial and would
        // otherwise stay blocked on it forever
        let (exe, args) = a_child_that_never_finishes();
        let started = Instant::now();
        let err = run_cli_within(&exe, &args, None, Duration::from_secs(2))
            .expect_err("a child that never exits must fail");
        assert!(err.contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the child was not killed in time"
        );
    }

    #[test]
    fn the_cli_wait_stops_at_the_deadline() {
        // The wait is a real wait on the child's process handle, but it must not wait
        // longer than the limit: the pack worker is serial and every later pack waits.
        let (exe, args) = a_child_that_never_finishes();
        let started = Instant::now();
        let err = run_cli_within(&exe, &args, None, Duration::from_millis(700))
            .expect_err("a child that never exits must fail");
        let elapsed = started.elapsed();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            elapsed >= Duration::from_millis(600),
            "the wait gave up before the deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the wait went past the deadline: {elapsed:?}"
        );
    }

    #[test]
    fn a_cli_call_reports_its_exit_code_only_when_the_step_asks_for_it() {
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let exe = PathBuf::from(system)
            .join("System32")
            .join("cmd.exe")
            .display()
            .to_string();
        assert!(run_cli(&exe, &["/c".to_string(), "exit 0".to_string()], None).is_ok());
        // Many tools exit non-zero without meaning failure: without an `expect_exit` that is
        // information, not a failure — the step must not be reported as broken.
        assert!(run_cli(&exe, &["/c".to_string(), "exit 3".to_string()], None).is_ok());
        // With `expect_exit` the code is the contract: a different one fails the step.
        let err = run_cli(
            &exe,
            &["/c".to_string(), "exit 3".to_string()],
            Some(0),
        )
        .unwrap_err();
        assert!(err.contains("expected 0"), "{err}");
        assert!(run_cli(&exe, &["/c".to_string(), "exit 0".to_string()], Some(0)).is_ok());
    }

    #[test]
    fn env_expansion() {
        let expanded = expand_env("%APPDATA%\\GHelper\\config.json");
        assert!(!expanded.contains('%'), "should have expanded: {expanded}");
        assert!(expanded.ends_with("GHelper\\config.json"));
    }

    #[test]
    fn json_pointer_walk() {
        // a folder of our own: a fixed name would be shared by every run of the suite
        let scratch = Scratch::dir("pack-test");
        let file = scratch.root().join("t.json");
        std::fs::write(&file, r#"{"performance_mode":1,"nested":{"x":"y"}}"#).unwrap();
        assert_eq!(
            json_pointer(file.to_str().unwrap(), "/performance_mode").unwrap(),
            "1"
        );
        assert_eq!(
            json_pointer(file.to_str().unwrap(), "/nested/x").unwrap(),
            "y"
        );
        assert!(json_pointer(file.to_str().unwrap(), "/nope").is_none());
    }

    #[test]
    fn loads_the_shipped_packs() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let packs = load(&[root]);
        assert!(
            packs.len() >= 3,
            "expected the three shipped packs, found {}",
            packs.len()
        );
        // Sorted by order: WE (10) before GH (20) before RM (30). The dev checkout can
        // hold packs the user installed on top (the console's helpers are not here), so
        // only the shipped three are looked at, in their relative order.
        let ids: Vec<&str> = packs.iter().map(|p| p.pack.id.as_str()).collect();
        let pos = |id: &str| {
            ids.iter()
                .position(|x| *x == id)
                .unwrap_or_else(|| panic!("the shipped pack {id} is missing: {ids:?}"))
        };
        assert!(pos("wallpaper-engine") < pos("g-helper"));
        assert!(pos("g-helper") < pos("rainmeter"));
    }

    /// A scratch install layout: <tmp>/armoury-packs-{label}-{pid}-{n}/packs
    ///
    /// The guard owns the folder: it goes away when the test ends, failed or not.
    fn scratch_install(label: &str) -> Scratch {
        let scratch = Scratch::dir(&format!("packs-{label}"));
        std::fs::create_dir_all(scratch.root().join("packs")).unwrap();
        scratch
    }

    /// A minimal valid pack, used by the install/remove tests.
    const SAMPLE: &str = r#"{
        "spec": 1,
        "id": "sample",
        "name": "Sample",
        "version": "1.0.0",
        "targets": { "process": ["sample.exe"] },
        "settings": [
            { "key": "action", "label": "Action", "type": "enum",
              "values": ["stop", "pause"], "default": "stop" }
        ],
        "capture": [ { "key": "was_running", "mechanism": "process_running" } ],
        "apply": { "steps": [ { "mechanism": "cli", "exe": "sample.exe", "args": ["-stop"] } ] },
        "revert": { "steps": [ { "mechanism": "cli", "exe": "sample.exe", "args": ["-play"] } ] }
    }"#;

    #[test]
    fn a_denied_capability_is_cut_from_the_grant_set() {
        let pack = Pack::parse(SAMPLE).expect("the sample parses");
        let before = pack.capabilities;
        let mut installed = vec![Installed { pack }];
        let mut denials: HashMap<String, Vec<String>> = HashMap::new();
        denials.insert("sample".into(), vec!["exec".into(), "not-a-capability".into()]);
        denials.insert("some-other-pack".into(), vec!["files".into()]);
        apply_denials(&mut installed, &denials);
        let after = installed[0].pack.capabilities;
        assert!(!after.grants(Capability::Exec), "exec was refused");
        assert!(
            after.grants(Capability::Files),
            "only the pack that asked for it loses files"
        );
        assert!(
            before.grants(Capability::Exec) && before.grants(Capability::Files),
            "the sample declares both: the test would pass on a pack that has neither"
        );
        assert!(
            before.grants(Capability::Process) && after.grants(Capability::Process),
            "a capability nobody unticked is untouched"
        );
    }

    #[test]
    fn the_preview_says_what_the_pack_does() {
        let pack = Pack::parse(SAMPLE).expect("the sample parses");
        let lines = describe(&pack);
        assert!(lines.iter().any(|l| l == "Watches for sample.exe"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l == "Runs sample.exe -stop"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Checks whether")),
            "{lines:?}"
        );
        // The dropdown is part of what the pack will show, and it is named by its label.
        assert!(lines.iter().any(|l| l.contains("'Action'")), "{lines:?}");
    }

    #[test]
    fn the_rainmeter_pack_scripts_compile() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = std::fs::read_to_string(root.join("packs/rainmeter/pack.json"))
            .expect("the shipped Rainmeter pack must exist");
        let pack = Pack::parse(&text).expect("the Rainmeter pack must be valid");
        check_scripts(&pack).expect("the Rainmeter pack's scripts must compile");
    }

    #[test]
    fn exe_from_setting_picks_the_mapped_binary() {
        let step: Step = serde_json::from_str(
            r#"{"mechanism":"cli","exe_from_setting":"binary",
                "exe_map":{"auto":"{{captured.target_path}}",
                           "64-bit":"{{target.dir}}\\wallpaper64.exe",
                           "32-bit":"{{target.dir}}\\wallpaper32.exe"},
                "args":["-control","stop"]}"#,
        )
        .unwrap();
        let mut ctx = TemplateCtx {
            target_dir: "C:\\WE".into(),
            ..Default::default()
        };
        ctx.captured
            .insert("target_path".into(), "C:\\WE\\wallpaper64.exe".into());
        ctx.settings.insert("binary".into(), "auto".into());
        assert_eq!(
            resolve_exe(&step, &ctx, "we").unwrap(),
            "C:\\WE\\wallpaper64.exe",
            "auto must call the running binary"
        );
        ctx.settings.insert("binary".into(), "32-bit".into());
        assert_eq!(resolve_exe(&step, &ctx, "we").unwrap(), "C:\\WE\\wallpaper32.exe");
        ctx.settings.insert("binary".into(), "bogus".into());
        assert!(resolve_exe(&step, &ctx, "we").is_err());
        // no value at all must be an error, never a silent empty exe
        ctx.settings.remove("binary");
        assert!(resolve_exe(&step, &ctx, "we").is_err());
        // a step without exe_from_setting still uses its own template
        let plain: Step =
            serde_json::from_str(r#"{"mechanism":"cli","exe":"{{target.name}}"}"#).unwrap();
        ctx.target_name = "app.exe".into();
        assert_eq!(resolve_exe(&plain, &ctx, "we").unwrap(), "app.exe");
    }

    #[test]
    fn a_hotkey_step_without_a_mapping_is_a_failure() {
        // A dropdown value the step's map does not know: the step would send nothing, and a
        // pack that does nothing must not report success. This is the G-Helper revert path.
        let step: Step = serde_json::from_str(
            r#"{"mechanism":"hotkey","keys_from_mode":"{{settings.mode}}",
                "map":{"Turbo":"ctrl+shift+alt+f18"}}"#,
        )
        .unwrap();
        let mut ctx = TemplateCtx::default();
        ctx.settings.insert("mode".into(), "Silent".into());
        let err = run_step(
            &step,
            &ctx,
            Some("mode"),
            "g-helper",
            "revert step 0",
            CapSet::ALL,
        )
        .expect_err("an unmapped value must fail the step");
        assert!(err.contains("g-helper"), "{err}");
        assert!(err.contains("revert step 0"), "{err}");
        assert!(err.contains("Silent"), "{err}");
    }

    #[test]
    fn an_unmapped_captured_value_is_a_failure() {
        let step: Step = serde_json::from_str(
            r#"{"mechanism":"hotkey","keys_from_captured":"prev_mode",
                "map":{"0":"ctrl+shift+alt+f17"}}"#,
        )
        .unwrap();
        let mut ctx = TemplateCtx::default();
        ctx.captured.insert("prev_mode".into(), "3".into());
        let err = run_step(
            &step,
            &ctx,
            None,
            "g-helper",
            "revert step 0",
            CapSet::ALL,
        )
        .expect_err("an unmapped captured value must fail the step");
        assert!(err.contains("g-helper"), "{err}");
        assert!(err.contains("revert step 0"), "{err}");
        assert!(err.contains("prev_mode"), "{err}");
        assert!(err.contains("3"), "{err}");
    }

    #[test]
    fn a_step_of_an_ungranted_capability_is_refused_before_it_runs() {
        // The load gate refuses such a pack (`Pack::validate`); this is the same check where
        // the mechanism would run, so the worker cannot be reached around it. Nothing of the
        // step may happen: the refusal comes before the exe is even resolved.
        let step: Step =
            serde_json::from_str(r#"{"mechanism":"cli","exe":"C:\\Windows\\System32\\cmd.exe"}"#)
                .unwrap();
        let ctx = TemplateCtx::default();
        let files = CapSet::default().with(Capability::Files);
        let err = run_step(&step, &ctx, None, "caps", "apply step 0", files)
            .expect_err("a cli step needs 'exec'");
        assert!(err.contains("apply step 0"), "{err}");
        assert!(err.contains("cli REFUSED"), "{err}");
        assert!(err.contains("it needs the 'exec' capability"), "{err}");
        assert!(err.contains("the pack asked for: files"), "{err}");
    }

    #[test]
    fn a_pack_with_an_unmapped_hotkey_reports_the_failure() {
        // The whole job: the pack runs, the hotkey has no chord, and the outcome says so —
        // this is what reaches the dashboard's error line.
        let json = r#"{
  "spec": 1, "id": "hotkey-test", "name": "Hotkey test", "version": "1.0.0", "order": 1,
  "targets": { "process": ["hotkey-test-target.exe"] },
  "settings": [
    { "key": "mode", "label": "Mode", "type": "enum", "values": ["Turbo", "Silent"], "default": "Turbo" }
  ],
  "apply": { "guard": { "targetRunning": false },
    "steps": [ { "mechanism": "hotkey", "keys_from_mode": "{{settings.mode}}",
                 "map": { "Turbo": "ctrl+shift+alt+f18" } } ] },
  "revert": { "guard": { "targetRunning": false },
    "steps": [ { "mechanism": "cli", "exe": "cmd.exe", "args": ["/c", "exit 0"] } ] }
}"#;
        let pack = Pack::parse(json).expect("the test pack must be valid");
        let job = PackJob {
            apply: true,
            tag: None,
            enabled: HashMap::from([("hotkey-test".to_string(), true)]),
            values: HashMap::from([(
                "hotkey-test".to_string(),
                HashMap::from([("mode".to_string(), "Silent".to_string())]),
            )]),
            captured: HashMap::new(),
        };
        let outcome = run_job(&[Installed { pack }], &job, &mut None);
        assert!(outcome.failed.len() == 1, "the pack must report a failure");
        let (id, message) = &outcome.failed[0];
        assert_eq!(id, "hotkey-test");
        assert!(message.contains("hotkey-test"), "{message}");
        assert!(message.contains("apply step 0"), "{message}");
        assert!(message.contains("Silent"), "{message}");
        assert!(outcome.succeeded.is_empty(), "{:?}", outcome.succeeded);
    }

    #[test]
    fn missing_dropdown_values_are_filled_from_defaults() {
        // exactly the 2026-10-18 bug: an old config only knows "action", the pack now also has
        // "binary"; without the default the WE steps refused to run.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = std::fs::read_to_string(root.join("packs/wallpaper-engine/pack.json"))
            .expect("the shipped WE pack must exist");
        let pack = Pack::parse(&text).unwrap();
        let mut settings: HashMap<String, String> = HashMap::new();
        settings.insert("action".into(), "pause".into());
        fill_defaults(&mut settings, &pack);
        assert_eq!(settings.get("action").unwrap(), "pause", "the user's value wins");
        assert_eq!(
            settings.get("binary").unwrap(),
            "auto",
            "the new dropdown gets its default"
        );
    }

    #[test]
    fn pack_ids_must_be_plain_folder_names() {
        assert!(is_safe_id("wallpaper-engine"));
        assert!(is_safe_id("my pack.1"));
        for bad in ["", " ", "..", ".", "../..", "a/b", "a\\b", "C:evil", "evil.", ".hidden", "."] {
            assert!(!is_safe_id(bad), "'{bad}' must be refused");
        }
        assert!(!is_safe_id(&"x".repeat(65)));
    }

    #[test]
    fn install_then_remove_a_pack() {
        let scratch = scratch_install("install");
        let install = scratch.root().to_path_buf();
        let source = install.join("incoming");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("pack.json"), SAMPLE).unwrap();

        let (id, updated) = install_from_path(std::slice::from_ref(&install), &source).unwrap();
        assert_eq!(id, "sample");
        assert!(!updated);
        assert!(install.join("packs").join("sample").join("pack.json").is_file());

        // installing again is an update, not a second copy
        let (id, updated) = install_from_path(std::slice::from_ref(&install), &source).unwrap();
        assert_eq!(id, "sample");
        assert!(updated);
        assert_eq!(load(std::slice::from_ref(&install)).len(), 1);

        let removed = remove_id(std::slice::from_ref(&install), "sample").unwrap();
        assert_eq!(removed, "sample");
        assert!(!install.join("packs").join("sample").exists());
        assert!(load(std::slice::from_ref(&install)).is_empty());

        // nothing left to remove, and traversal is refused
        assert!(remove_id(std::slice::from_ref(&install), "sample").is_err());
        assert!(remove_id(std::slice::from_ref(&install), "../..").is_err());
        assert!(remove_id(std::slice::from_ref(&install), "C:evil").is_err());
        // the scratch folder goes away with the guard
    }

    #[test]
    fn install_rejects_an_invalid_pack() {
        let scratch = scratch_install("invalid");
        let install = scratch.root().to_path_buf();
        let bad = install.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("pack.json"), r#"{"spec":1}"#).unwrap();
        assert!(install_from_path(std::slice::from_ref(&install), &bad).is_err());
        assert!(!install.join("packs").join("x").exists());
        assert!(load(std::slice::from_ref(&install)).is_empty());
        // the scratch folder goes away with the guard
    }

    /// A dropdown with a `discover` block gets the machine's values, not the manifest's.
    #[test]
    fn a_declared_discovery_fills_the_dropdown_from_the_machine() {
        let scratch = Scratch::dir("discovery");
        let root = scratch.root().to_path_buf();
        for name in ["Beta", "Alpha", "@Backup"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let mut pack = Pack::parse(SAMPLE).expect("the sample pack is valid");
        assert!(!pack.settings.is_empty(), "the sample has a dropdown");
        pack.settings[0].discover = Some(armoury_core::pack::Discover {
            from: "folder".into(),
            path: Some(root.to_string_lossy().into_owned()),
            exclude: vec!["@Backup".into()],
            append: vec!["kill".into()],
            ensure: None,
        });
        let mut installed = vec![Installed { pack }];
        apply_discovery(&mut installed);
        assert_eq!(
            installed[0].pack.settings[0].values,
            vec!["Alpha", "Beta", "kill"],
            "sorted, the excluded folder left out, the literal appended"
        );
    }

    /// Nothing to discover is not an empty dropdown: the declared list is the way back.
    #[test]
    fn a_dropdown_with_nothing_to_discover_keeps_the_declared_values() {
        let mut pack = Pack::parse(SAMPLE).expect("the sample pack is valid");
        let declared = pack.settings[0].values.clone();
        pack.settings[0].discover = Some(armoury_core::pack::Discover {
            from: "folder".into(),
            path: Some("Z:\\armoury-no-such-folder".into()),
            exclude: Vec::new(),
            append: Vec::new(),
            ensure: None,
        });
        let mut installed = vec![Installed { pack }];
        apply_discovery(&mut installed);
        assert_eq!(installed[0].pack.settings[0].values, declared);
    }

    /// The sample pack with a `min_engine` line.
    fn sample_with_min_engine(min: &str) -> String {
        SAMPLE.replace(
            "\"version\": \"1.0.0\",",
            &format!("\"version\": \"1.0.0\",\n        \"min_engine\": \"{min}\","),
        )
    }

    #[test]
    fn install_refuses_a_pack_this_daemon_is_too_old_for() {
        let scratch = scratch_install("too-old");
        let install = scratch.root().to_path_buf();
        let source = install.join("incoming");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("pack.json"), sample_with_min_engine("99.0.0")).unwrap();

        let err = install_from_path(std::slice::from_ref(&install), &source)
            .expect_err("a pack that needs a newer daemon must be refused");
        assert!(err.contains("sample"), "the message names the pack: {err}");
        assert!(err.contains("99.0.0"), "the message names the version it needs: {err}");
        assert!(err.contains(DAEMON_VERSION), "the message names this daemon: {err}");
        assert!(
            !install.join("packs").join("sample").exists(),
            "a refused pack must not land on disk"
        );
        assert!(load(std::slice::from_ref(&install)).is_empty());
        // the scratch folder goes away with the guard
    }

    #[test]
    fn install_accepts_a_pack_with_no_minimum_and_with_an_older_one() {
        let scratch = scratch_install("older");
        let install = scratch.root().to_path_buf();
        let source = install.join("incoming");
        std::fs::create_dir_all(&source).unwrap();

        // no minimum at all
        std::fs::write(source.join("pack.json"), SAMPLE).unwrap();
        assert!(install_from_path(std::slice::from_ref(&install), &source).is_ok());

        // older than this daemon
        std::fs::write(source.join("pack.json"), sample_with_min_engine("0.0.1")).unwrap();
        assert!(install_from_path(std::slice::from_ref(&install), &source).is_ok());

        // exactly this daemon
        std::fs::write(source.join("pack.json"), sample_with_min_engine(DAEMON_VERSION)).unwrap();
        assert!(install_from_path(std::slice::from_ref(&install), &source).is_ok());
        // the scratch folder goes away with the guard
    }

    #[test]
    fn journal_pending_revert_reads_the_last_applied_entry() {
        let scratch = scratch_install("journal");
        // the config folder inside the scratch install: the guard removes the whole scratch
        // folder, this one included
        let dir = scratch.root().join("cfg");
        std::fs::create_dir_all(&dir).unwrap();

        // nothing journalled yet
        assert!(journal_pending_revert_in(&dir).is_none());

        let entry = |applied: bool, ids: Vec<&str>| {
            serde_json::json!({
                "at": 1,
                "applied": applied,
                "trigger": "game.exe",
                "packs": ids,
                "values": { "g-helper": { "mode": "Turbo" } },
                "captured": { "rainmeter": { "was_running": "1" } },
                "failed": [],
            })
        };
        std::fs::write(
            dir.join("journal.json"),
            serde_json::to_string(&vec![entry(true, vec!["rainmeter"])]).unwrap(),
        )
        .unwrap();

        let job = journal_pending_revert_in(&dir).unwrap();
        assert!(!job.apply);
        assert_eq!(job.enabled.get("rainmeter"), Some(&true));
        assert_eq!(job.values.get("g-helper").unwrap().get("mode").unwrap(), "Turbo");
        assert_eq!(
            job.captured.get("rainmeter").unwrap().get("was_running").unwrap(),
            "1"
        );

        // a reverted last entry means nothing is pending
        std::fs::write(
            dir.join("journal.json"),
            serde_json::to_string(&vec![entry(true, vec!["rainmeter"]), entry(false, vec![])]).unwrap(),
        )
        .unwrap();
        assert!(journal_pending_revert_in(&dir).is_none());
        // the scratch folder goes away with the guard
    }

    /// A pack whose "before" state lives in a file (like G-Helper's config), plus a harmless step.
    fn carry_test_packs(dir: &Path, config_text: &str) -> Vec<Installed> {
        let cfg = dir.join("ghelper.json");
        std::fs::write(&cfg, config_text).unwrap();
        // JSON strings cannot carry Windows backslashes unescaped; forward slashes work fine here
        let cfg_path = cfg.display().to_string().replace('\\', "/");
        let json = format!(
            r#"{{
  "spec": 1, "id": "carry-test", "name": "Carry test", "version": "1.0.0", "order": 1,
  "targets": {{ "process": ["carry-test-target.exe"] }},
  "settings": [
    {{ "key": "mode", "label": "Mode", "type": "enum", "values": ["Turbo", "Silent"], "default": "Turbo" }}
  ],
  "capture": [
    {{ "key": "prev_mode", "mechanism": "json_file", "path": "{cfg_path}", "pointer": "/performance_mode" }},
    {{ "key": "was_running", "mechanism": "process_running" }}
  ],
  "apply": {{ "guard": {{ "targetRunning": true }},
              "steps": [ {{ "mechanism": "cli", "exe": "cmd.exe", "args": ["/c", "exit 0"] }} ] }},
  "revert": {{ "guard": {{ "targetRunning": false }},
               "steps": [ {{ "mechanism": "cli", "exe": "cmd.exe", "args": ["/c", "exit 0"] }} ] }}
}}"#
        );
        let pack = Pack::parse(&json).expect("the carry-test pack must be valid");
        vec![Installed { pack }]
    }

    fn carry_job(apply: bool) -> PackJob {
        PackJob {
            apply,
            tag: None,
            enabled: HashMap::from([("carry-test".to_string(), true)]),
            values: HashMap::new(),
            captured: if apply {
                HashMap::new()
            } else {
                HashMap::from([(
                    "carry-test".to_string(),
                    HashMap::from([
                        ("prev_mode".to_string(), "2".to_string()),
                        ("was_running".to_string(), "1".to_string()),
                    ]),
                )])
            },
        }
    }

    #[test]
    fn an_outcome_scopes_itself_to_the_enabled_ids_of_the_job() {
        // A4: the daemon splits a whole-state job into two halves and merges the outcomes
        // by scope, so the scope must be exactly the ids the job had switched on — a
        // disabled id is in the map as `false` and must not appear.
        let scratch = scratch_install("scoped");
        let dir = scratch.root().to_path_buf();
        let packs = carry_test_packs(&dir, r#"{"performance_mode": 1}"#);
        let job = PackJob {
            apply: true,
            tag: None,
            enabled: HashMap::from([
                ("carry-test".to_string(), true),
                ("ghost-test".to_string(), false),
            ]),
            values: HashMap::new(),
            captured: HashMap::new(),
        };
        let outcome = run_job(&packs, &job, &mut None);
        assert_eq!(outcome.scoped, vec!["carry-test".to_string()]);
        // the scratch folder goes away with the guard
    }

    #[test]
    fn a_slow_mode_worker_never_delays_the_pack_worker() {
        // A4 in one picture: the daemon's two halves land on two threads, so the pack
        // outcome shows up while the mode's job is still sleeping — and each outcome
        // carries only its own half's scope. The threads are started the way the daemon
        // starts them, one job per channel.
        let slow = r#"
{
  "spec": 1, "id": "slowmode-test", "name": "Slow test", "version": "1.0.0", "order": 1,
  "kind": "mode",
  "triggers": [ { "kind": "tracked_app" } ],
  "targets": { "process": ["slowmode-test-target.exe"] },
  "apply": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "cmd.exe", "args": ["/c", "exit 0"], "sleep_ms": 5000 } ] },
  "revert": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "cmd.exe", "args": ["/c", "exit 0"], "sleep_ms": 5000 } ] }
}"#;
        let fast = r#"
{
  "spec": 1, "id": "fastpack-test", "name": "Fast test", "version": "1.0.0", "order": 2,
  "targets": { "process": ["fastpack-test-target.exe"] },
  "apply": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "cmd.exe", "args": ["/c", "exit 0"] } ] },
  "revert": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "cmd.exe", "args": ["/c", "exit 0"] } ] }
}"#;
        let packs = vec![
            Installed { pack: Pack::parse(slow).expect("the slow mode must be valid") },
            Installed { pack: Pack::parse(fast).expect("the fast pack must be valid") },
        ];
        let (pack_tx, pack_rx) = crossbeam_channel::unbounded::<PackJob>();
        let (mode_tx, mode_rx) = crossbeam_channel::unbounded::<PackJob>();
        let (out_tx, out_rx) = crossbeam_channel::unbounded::<PackOutcome>();
        start(packs, pack_rx, mode_rx, out_tx, 0);

        // the daemon's split by hand: pack half on the pack channel, mode half on the mode one
        pack_tx
            .send(PackJob {
                apply: true,
                tag: None,
                enabled: HashMap::from([("fastpack-test".to_string(), true)]),
                values: HashMap::new(),
                captured: HashMap::new(),
            })
            .unwrap();
        mode_tx
            .send(PackJob {
                apply: true,
                tag: None,
                enabled: HashMap::from([("slowmode-test".to_string(), true)]),
                values: HashMap::new(),
                captured: HashMap::new(),
            })
            .unwrap();

        // The pack outcome lands while the mode worker is still sleeping: that is the
        // whole point of A4. Both must still put their own half in the scope.
        let started = Instant::now();
        let mut fast_outcome = None;
        let mut mode_outcome = None;
        for _ in 0..2 {
            let outcome = out_rx
                .recv_timeout(Duration::from_secs(15))
                .expect("both outcomes arrive");
            if outcome.scoped.contains(&"fastpack-test".to_string()) {
                fast_outcome = Some((outcome, started.elapsed()));
            } else {
                mode_outcome = Some((outcome, started.elapsed()));
            }
        }
        let (fast, fast_at) = fast_outcome.expect("the pack half answered");
        let (mode, mode_at) = mode_outcome.expect("the mode half answered");
        assert_eq!(fast.scoped, vec!["fastpack-test".to_string()]);
        assert_eq!(mode.scoped, vec!["slowmode-test".to_string()]);
        assert_eq!(fast.applied_ids, vec!["fastpack-test".to_string()]);
        assert_eq!(mode.applied_ids, vec!["slowmode-test".to_string()]);
        assert!(
            fast_at < Duration::from_secs(3) && mode_at >= Duration::from_secs(4),
            "the pack half must land while the mode half still sleeps: fast {fast_at:?}, mode {mode_at:?}"
        );
    }

    #[test]
    fn an_apply_right_after_a_revert_reuses_the_reverts_capture() {
        let scratch = scratch_install("carry");
        let dir = scratch.root().to_path_buf();
        // the file already shows OUR change - in real life the app writes it with a ~2 s delay
        let packs = carry_test_packs(&dir, r#"{"performance_mode": 1}"#);
        let mut carry: Option<(Instant, CaptureMap)> = None;

        // 1. the revert (as at startup) restores Silent; that is the truth to carry forward
        let outcome = run_job(&packs, &carry_job(false), &mut carry);
        assert!(carry.is_some(), "a successful revert must be kept for a following apply");
        assert!(!outcome.applied);

        // 2. an apply a moment later must journal Silent ("2"), not the file's own echo ("1")
        let outcome = run_job(&packs, &carry_job(true), &mut carry);
        let captured = outcome
            .captured
            .get("carry-test")
            .expect("apply captures are journalled");
        assert_eq!(captured.get("prev_mode").map(String::as_str), Some("2"));
        // process state is a live fact, not something to carry
        assert_eq!(captured.get("was_running").map(String::as_str), Some("0"));
        assert!(carry.is_none(), "a used carry is spent");

        // the scratch folder goes away with the guard
    }

    #[test]
    fn an_expired_carry_is_ignored() {
        let scratch = scratch_install("carry-old");
        let dir = scratch.root().to_path_buf();
        let packs = carry_test_packs(&dir, r#"{"performance_mode": 1}"#);
        // a carry from one second past the window must not be believed
        let mut carry: Option<(Instant, CaptureMap)> = Some((
            Instant::now() - CARRY_WINDOW - Duration::from_secs(1),
            HashMap::from([(
                "carry-test".to_string(),
                HashMap::from([("prev_mode".to_string(), "2".to_string())]),
            )]),
        ));

        let outcome = run_job(&packs, &carry_job(true), &mut carry);
        let captured = outcome.captured.get("carry-test").unwrap();
        assert_eq!(
            captured.get("prev_mode").map(String::as_str),
            Some("1"),
            "outside the window the file is the only truth"
        );
        // the scratch folder goes away with the guard
    }
}
