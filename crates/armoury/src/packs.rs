//! Pack execution: loads data-only packs and runs their mechanisms.
//!
//! Runs on its own worker thread — the daemon's main loop must stay responsive, and
//! CLI/bang calls block for tens to hundreds of milliseconds.
//! Nothing here ever goes through a shell, and packs can only use whitelisted mechanisms.

use armoury_core::pack::{Pack, Step, StepBlock, TemplateCtx, substitute};
use crossbeam_channel::Receiver;
use log::{info, warn};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::script;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    /// Packs this job acted on. A pack whose block *started* is in here even if a step
    /// failed halfway: the machine was changed, so it must be journalled and reverted.
    pub applied_ids: Vec<String>,
    /// The dropdown values this job ran with (echoed so the journal can record them).
    pub values: HashMap<String, HashMap<String, String>>,
    /// Per-pack captured values (journal for the matching revert).
    pub captured: HashMap<String, HashMap<String, String>>,
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Text files written on Windows often start with a UTF-8 BOM; serde_json rejects it.
fn strip_bom(text: &str) -> &str {
    text.trim_start_matches('\u{feff}')
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
                    if seen.contains(&pack.id) {
                        continue; // a pack in an earlier search dir wins
                    }
                    info!(
                        "pack loaded: {} v{} (order {}, {} target(s))",
                        pack.name,
                        pack.version,
                        pack.order,
                        pack.targets.process.len()
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
    outcomes: crossbeam_channel::Sender<PackOutcome>,
    wake: usize,
) {
    std::thread::spawn(move || {
        info!("pack worker started with {} pack(s)", packs.len());
        while let Ok(job) = jobs.recv() {
            let outcome = run_job(&packs, &job);
            let _ = outcomes.send(outcome);
            if wake != 0 {
                unsafe { windows_sys::Win32::System::Threading::SetEvent(wake as _) };
            }
        }
        info!("pack worker stopped");
    });
}

fn run_job(packs: &[Installed], job: &PackJob) -> PackOutcome {
    let targets = snapshot_targets();
    let mut strings: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut applied_ids: Vec<String> = Vec::new();
    let mut captured_out: HashMap<String, HashMap<String, String>> = HashMap::new();

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

        // Apply captures live; a revert replays what the matching apply captured.
        if job.apply {
            for step in &pack.capture {
                let value = match step.mechanism.as_str() {
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
        let mut result = run_block(block, &ctx, setting_key);

        // Spec 2: the block's script runs after its steps, in the same context. A script may
        // capture values; they are journalled below together with the step captures.
        let script_src = if job.apply {
            pack.script.apply.as_deref()
        } else {
            pack.script.revert.as_deref()
        };
        if let Some(src) = script_src.filter(|s| !s.trim().is_empty()) {
            match script::run(src, &mut ctx) {
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
        if job.apply {
            captured_out.insert(pack.id.clone(), ctx.captured.clone());
            // "applied" means the block was started. A pack that failed halfway has already changed
            // the machine, so it must be journalled (and reverted) like any other.
            applied_ids.push(pack.id.clone());
        }
        match result {
            Ok(lines) => {
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

    PackOutcome {
        applied: job.apply,
        tag: job.tag.clone(),
        applied_ids,
        values: job.values.clone(),
        captured: captured_out,
        succeeded: strings,
        failed,
    }
}

fn target_name_or(pack: &Pack) -> String {
    pack.targets
        .process
        .first()
        .cloned()
        .unwrap_or_else(|| "target".to_string())
}

fn run_block(block: &StepBlock, ctx: &TemplateCtx, setting_key: Option<&str>) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    for step in &block.steps {
        if run_step(step, ctx, setting_key)? {
            done.push(describe_step(step, ctx));
        }
    }
    Ok(done)
}

fn describe_step(step: &Step, ctx: &TemplateCtx) -> String {
    match step.mechanism.as_str() {
        "cli" => {
            let exe = resolve_exe(step, ctx).unwrap_or_default();
            let args: Vec<String> = step.args.iter().map(|a| substitute(a, ctx).0).collect();
            format!("cli {exe} {}", args.join(" "))
        }
        "hotkey" => format!("hotkey {}", resolve_chord(step, ctx).unwrap_or_default()),
        "rainmeter_bang" => {
            format!("bang {}", substitute(step.bang.as_deref().unwrap_or(""), ctx).0)
        }
        "registry_set" => match &step.registry {
            Some(r) => format!("registry {}\\{} = {}", r.path, r.name, r.dword),
            None => "registry_set".to_string(),
        },
        other => other.to_string(),
    }
}

/// Fill dropdown values the user's config does not know yet with the pack's defaults, so a pack
/// that gains a setting keeps working on an older config (the Wallpaper Engine bug of 2026-10-18).
fn fill_defaults(settings: &mut HashMap<String, String>, pack: &Pack) {
    for setting in &pack.settings {
        if !settings.contains_key(&setting.key) {
            if let Some(default) = &setting.default {
                settings.insert(setting.key.clone(), default.clone());
            }
        }
    }
}

/// The exe a step runs: either its own `exe` template, or one picked from a dropdown via
/// `exe_from_setting` + `exe_map` (e.g. "auto / 64-bit / 32-bit" of a multi-binary app).
fn resolve_exe(step: &Step, ctx: &TemplateCtx) -> Result<String, String> {
    if let Some(key) = &step.exe_from_setting {
        let value = ctx.settings.get(key).cloned().unwrap_or_default();
        let template = step
            .exe_map
            .get(&value)
            .ok_or_else(|| format!("no exe mapped for {key} = '{value}'"))?;
        return Ok(substitute(template, ctx).0);
    }
    Ok(substitute(step.exe.as_deref().unwrap_or(""), ctx).0)
}

fn run_step(step: &Step, ctx: &TemplateCtx, setting_key: Option<&str>) -> Result<bool, String> {
    if let Some(cond) = &step.when {
        if !cond.matches(ctx, setting_key) {
            return Ok(false); // step does not apply to this configuration
        }
    }
    if let Some(ms) = step.sleep_ms {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms.min(10_000)));
        }
    }
    match step.mechanism.as_str() {
        "cli" => {
            let exe = resolve_exe(step, ctx)?;
            let args: Vec<String> = step.args.iter().map(|a| substitute(a, ctx).0).collect();
            run_cli(&exe, &args, step.expect_exit)
        }
        "process_start" => {
            let exe = resolve_exe(step, ctx)?;
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
                return Ok(false);
            }
            start_process(&exe)
        }
        "process_kill" => {
            let exe = resolve_exe(step, ctx)?;
            let name = exe
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&exe)
                .to_ascii_lowercase();
            if name.is_empty() {
                return Err("process_kill: no target".into());
            }
            kill_by_name(&name)
        }
        "registry_set" => {
            let reg = step.registry.as_ref().ok_or("registry_set: no registry block")?;
            set_registry_dword(&reg.path, &reg.name, reg.dword)
        }
        "hotkey" => {
            match resolve_chord(step, ctx) {
                Some(chord) => send_chord(&chord),
                // No mapping for this dropdown value (e.g. the "kill" option): nothing to send.
                None => Ok(()),
            }
        }
        "rainmeter_bang" => {
            let bang = substitute(step.bang.as_deref().unwrap_or(""), ctx).0;
            send_rainmeter_bang(&bang, &ctx.target_exe)
        }
        other => Err(format!("unsupported mechanism '{other}'")),
    }
    .map(|_| true)
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

fn resolve_chord(step: &Step, ctx: &TemplateCtx) -> Option<String> {
    if let Some(expr) = &step.keys_from_mode {
        let (key, _) = substitute(expr, ctx);
        return step.map.get(&key).cloned().or_else(|| {
            warn!("hotkey: no mapping for setting value '{key}'");
            None
        });
    }
    if let Some(key) = &step.keys_from_captured {
        let value = ctx.captured.get(key).cloned().unwrap_or_default();
        return step.map.get(&value).cloned().or_else(|| {
            warn!("hotkey: no mapping for captured {key}='{value}'");
            None
        });
    }
    None
}

fn run_cli(exe: &str, args: &[String], expect_exit: Option<i32>) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let output = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("spawn {exe}: {e}"))?;
    let code = output.status.code().unwrap_or(-1);
    match expect_exit {
        Some(want) if want != code => Err(format!("{exe} exited {code}, expected {want}")),
        _ if !output.status.success() && expect_exit.is_none() && code != 0 => {
            // Many tools exit non-zero without meaning failure; keep it as info only.
            Err(format!("{exe} exited {code}"))
        }
        _ => Ok(()),
    }
}

/// Rainmeter bangs. Preferred path is a WM_COPYDATA message to Rainmeter's hidden
/// `DummyRainWClass` window; the CLI is the documented fallback.
fn send_rainmeter_bang(bang: &str, target_exe: &str) -> Result<(), String> {
    if copy_data_bang(bang) {
        return Ok(());
    }
    let exe = if target_exe.is_empty() {
        "Rainmeter.exe".to_string()
    } else {
        target_exe.to_string()
    };
    let args = split_bang(bang);
    if args.is_empty() {
        return Err("empty bang".into());
    }
    run_cli(&exe, &args, None)
}

/// Split `!LoadLayout "Empty"` into argv while honouring quotes.
fn split_bang(bang: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in bang.chars() {
        match ch {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn copy_data_bang(bang: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageW, WM_COPYDATA};

    // Rainmeter's bang window: class DummyRainWClass, literally named "Rainmeter control window".
    let class: Vec<u16> = "DummyRainWClass\0".encode_utf16().collect();
    let name: Vec<u16> = "Rainmeter control window\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(class.as_ptr(), name.as_ptr()) };
    if hwnd.is_null() {
        // fall back to class-only in case a build changed the window name
        let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
        if hwnd.is_null() {
            return false;
        }
    }

    // Rainmeter expects dwData == 1 and a UTF-16 bang string.
    let mut wide: Vec<u16> = bang.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();
    let mut data = COPYDATASTRUCT {
        dwData: 1,
        cbData: bytes as u32,
        lpData: wide.as_mut_ptr() as *mut core::ffi::c_void,
    };
    unsafe {
        SendMessageW(
            hwnd,
            WM_COPYDATA,
            0 as _,
            &mut data as *mut COPYDATASTRUCT as LPARAM,
        );
    }
    true
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
        if let Some(rest) = lower.strip_prefix("active") {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                if value.trim().parse::<i32>().map(|v| v >= 1).unwrap_or(false) {
                    return false;
                }
            }
        }
    }
    true
}

fn decode_ini(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let words: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
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
    if let Some(name) = layouts.iter().find(|l| layout_is_empty(l)) {
        if !name.eq_ignore_ascii_case("@Backup") {
            return Some(name.clone());
        }
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
    let text = strip_bom(&text).to_string();
    let pack = Pack::parse(&text)?; // validates spec/settings/steps
    check_scripts(&pack)?;          // a scripted pack must compile before it lands on disk

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

fn is_safe_id(id: &str) -> bool {
    // A Windows folder name only: no separators, no drive prefix (`C:evil` escapes the root when
    // joined), no traversal, no leading/trailing dot (Windows strips those, so they would not round
    // trip through the file system).
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
// Host API for scripts (spec 3) and the load-time script check

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

/// Reject a pack whose scripts do not compile, before it is loaded or installed.
fn check_scripts(pack: &Pack) -> Result<(), String> {
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
            if !map.contains_key(&key) {
                if let Some(path) = crate::watch::process_path(pid) {
                    map.insert(key, (path, pid));
                }
            }
        }
        ok = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snap) };
    map
}

// ---------------------------------------------------------------------------
// Input synthesis

pub fn send_chord(chord: &str) -> Result<(), String> {    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
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

    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        return Err(format!("SendInput delivered {sent}/{} events", inputs.len()));
    }
    Ok(())
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

    #[test]
    fn bang_splitting() {
        assert_eq!(
            split_bang("!LoadLayout \"Empty\""),
            vec!["!LoadLayout".to_string(), "Empty".to_string()]
        );
        assert_eq!(split_bang("!ToggleMeter \"Some Meter\"").len(), 2);
    }

    #[test]
    fn env_expansion() {
        let expanded = expand_env("%APPDATA%\\GHelper\\config.json");
        assert!(!expanded.contains('%'), "should have expanded: {expanded}");
        assert!(expanded.ends_with("GHelper\\config.json"));
    }

    #[test]
    fn json_pointer_walk() {
        let dir = std::env::temp_dir().join("armoury-pack-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.json");
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
        // sorted by order: WE (10) before GH (20) before RM (30)
        assert_eq!(packs[0].pack.id, "wallpaper-engine");
        assert_eq!(packs.last().unwrap().pack.id, "rainmeter");
    }

    /// A scratch install layout: <tmp>/install/packs
    fn scratch_install(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("armoury-packs-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("packs")).unwrap();
        dir
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
            resolve_exe(&step, &ctx).unwrap(),
            "C:\\WE\\wallpaper64.exe",
            "auto must call the running binary"
        );
        ctx.settings.insert("binary".into(), "32-bit".into());
        assert_eq!(resolve_exe(&step, &ctx).unwrap(), "C:\\WE\\wallpaper32.exe");
        ctx.settings.insert("binary".into(), "bogus".into());
        assert!(resolve_exe(&step, &ctx).is_err());
        // no value at all must be an error, never a silent empty exe
        ctx.settings.remove("binary");
        assert!(resolve_exe(&step, &ctx).is_err());
        // a step without exe_from_setting still uses its own template
        let plain: Step =
            serde_json::from_str(r#"{"mechanism":"cli","exe":"{{target.name}}"}"#).unwrap();
        ctx.target_name = "app.exe".into();
        assert_eq!(resolve_exe(&plain, &ctx).unwrap(), "app.exe");
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
        let install = scratch_install("install");
        let source = install.join("incoming");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("pack.json"), SAMPLE).unwrap();

        let (id, updated) = install_from_path(&[install.clone()], &source).unwrap();
        assert_eq!(id, "sample");
        assert!(!updated);
        assert!(install.join("packs").join("sample").join("pack.json").is_file());

        // installing again is an update, not a second copy
        let (id, updated) = install_from_path(&[install.clone()], &source).unwrap();
        assert_eq!(id, "sample");
        assert!(updated);
        assert_eq!(load(&[install.clone()]).len(), 1);

        let removed = remove_id(&[install.clone()], "sample").unwrap();
        assert_eq!(removed, "sample");
        assert!(!install.join("packs").join("sample").exists());
        assert!(load(&[install.clone()]).is_empty());

        // nothing left to remove, and traversal is refused
        assert!(remove_id(&[install.clone()], "sample").is_err());
        assert!(remove_id(&[install.clone()], "../..").is_err());
        assert!(remove_id(&[install.clone()], "C:evil").is_err());
        let _ = std::fs::remove_dir_all(&install);
    }

    #[test]
    fn install_rejects_an_invalid_pack() {
        let install = scratch_install("invalid");
        let bad = install.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("pack.json"), r#"{"spec":1}"#).unwrap();
        assert!(install_from_path(&[install.clone()], &bad).is_err());
        assert!(!install.join("packs").join("x").exists());
        assert!(load(&[install.clone()]).is_empty());
        let _ = std::fs::remove_dir_all(&install);
    }

    #[test]
    fn journal_pending_revert_reads_the_last_applied_entry() {
        let dir = scratch_install("journal").join("cfg");
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
