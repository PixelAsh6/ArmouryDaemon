//! Pack execution: loads data-only packs and runs their mechanisms.
//!
//! Runs on its own worker thread — the daemon's main loop must stay responsive, and
//! CLI/bang calls block for tens to hundreds of milliseconds.
//! Nothing here ever goes through a shell, and packs can only use whitelisted mechanisms.

use armoury_core::pack::{Pack, Step, StepBlock, TemplateCtx, substitute};
use crossbeam_channel::Receiver;
use log::{info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

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
    /// Per-pack enabled flags and dropdown values (from config).
    pub enabled: HashMap<String, bool>,
    pub values: HashMap<String, HashMap<String, String>>,
    /// Values captured during the matching apply, replayed on revert (the journal).
    pub captured: HashMap<String, HashMap<String, String>>,
}

/// What happened, for logging and the dashboard's "applied" flag.
pub struct PackOutcome {
    pub applied: bool,
    /// Pack ids that are now in the applied state.
    pub applied_ids: Vec<String>,
    /// Per-pack captured values (journal for the matching revert).
    pub captured: HashMap<String, HashMap<String, String>>,
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
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
            match Pack::parse(&text) {
                Ok(pack) => {
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
            ..Default::default()
        };
        if let Some(values) = job.values.get(&pack.id) {
            ctx.settings = values.clone();
        }

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
        match run_block(block, &ctx, setting_key) {
            Ok(lines) => {
                for l in lines {
                    strings.push(format!("{}: {l}", pack.id));
                }
                if job.apply {
                    applied_ids.push(pack.id.clone());
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
        applied_ids,
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
            let exe = substitute(step.exe.as_deref().unwrap_or(""), ctx).0;
            let args: Vec<String> = step.args.iter().map(|a| substitute(a, ctx).0).collect();
            format!("cli {exe} {}", args.join(" "))
        }
        "hotkey" => format!("hotkey {}", resolve_chord(step, ctx).unwrap_or_default()),
        "rainmeter_bang" => {
            format!("bang {}", substitute(step.bang.as_deref().unwrap_or(""), ctx).0)
        }
        other => other.to_string(),
    }
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
            let exe = substitute(step.exe.as_deref().unwrap_or(""), ctx).0;
            let args: Vec<String> = step.args.iter().map(|a| substitute(a, ctx).0).collect();
            run_cli(&exe, &args, step.expect_exit)
        }
        "process_start" => {
            let exe = substitute(step.exe.as_deref().unwrap_or(""), ctx).0;
            if exe.is_empty() {
                return Err("process_start: no exe".into());
            }
            start_process(&exe)
        }
        "process_kill" => {
            let exe = substitute(step.exe.as_deref().unwrap_or(""), ctx).0;
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

fn send_chord(chord: &str) -> Result<(), String> {
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
}
