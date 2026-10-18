//! Spec 3 — scripted packs (Rhai).
//!
//! A pack may carry a script in `script.apply` / `script.revert`. The **pack never executes it**:
//! the daemon compiles the source, runs it in a fresh Rhai engine with hard limits, and exposes only
//! the functions registered below. Nothing else exists inside a script — no filesystem, network or
//! process API beyond these, no `eval`, no imports.
//!
//! Every host call is appended to the run's log (which the daemon writes to the pack journal).
//! Writes are restricted to the usual config locations and the target's own folder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use armoury_core::pack::TemplateCtx;
use log::info;
use rhai::{Array, Dynamic, Engine, EvalAltResult, ImmutableString, Scope};

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

/// Compile a script once (used to reject broken packs at load/install time).
pub fn compile(source: &str) -> Result<(), String> {
    engine()
        .compile(source)
        .map(|_| ())
        .map_err(|e| format!("script error: {e}"))
}

/// Run one block script. Returns the host-call log; `ctx.captured` receives anything the script
/// captured (so a revert can replay it).
pub fn run(source: &str, ctx: &mut TemplateCtx) -> Result<Vec<String>, String> {
    let ast = engine()
        .compile(source)
        .map_err(|e| format!("script error: {e}"))?;

    let host = ScriptHost {
        target_name: ctx.target_name.clone(),
        target_exe: ctx.target_exe.clone(),
        target_dir: ctx.target_dir.clone(),
        settings: ctx.settings.clone(),
        captured: ctx.captured.clone(),
        log: Vec::new(),
    };

    let mut scope = Scope::new();
    scope.push("ctx", host.clone());
    engine()
        .eval_ast_with_scope::<()>(&mut scope, &ast)
        .map_err(|e| format!("script failed: {e}"))?;

    let host: ScriptHost = scope
        .get_value("ctx")
        .ok_or_else(|| "script lost its context".to_string())?;
    ctx.captured = host.captured.clone();
    Ok(host.log)
}

/// The engine: limits first, then only the whitelisted host API.
fn engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(500_000);
    engine.set_max_call_levels(32);
    engine.set_max_expr_depths(64, 32);
    engine.set_max_string_size(64 * 1024);
    engine.set_max_array_size(4096);
    engine.set_max_map_size(256);
    engine.set_max_modules(0);

    // Wall clock guard: a pack may not hold the pack worker hostage.
    let started = Instant::now();
    engine.on_progress(move |_| {
        if started.elapsed() > Duration::from_secs(10) {
            Some(Dynamic::from("script exceeded its 10 s budget"))
        } else {
            None
        }
    });
    engine.on_print(|line| info!("script print: {line}"));

    engine.register_type_with_name::<ScriptHost>("ctx");

    // ---- read-only context ----
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
    engine.register_fn("log", |host: &mut ScriptHost, msg: &str| {
        host.note(msg.to_string());
    });
    engine.register_fn("capture", |host: &mut ScriptHost, key: &str, value: &str| {
        host.captured
            .insert(key.to_string(), value.to_string());
        host.note(format!("capture {key} = {value}"));
    });

    // ---- actions (all routed through the same mechanisms the data packs use) ----
    engine.register_fn(
        "run_cli",
        |host: &mut ScriptHost, exe: &str, args: Array| -> Result<(), Box<EvalAltResult>> {
            let args: Vec<String> = args
                .iter()
                .map(|a| a.clone().into_string().unwrap_or_default())
                .collect();
            host.note(format!("run_cli {exe} {}", args.join(" ")));
            crate::packs::script_cli(exe, &args).map_err(err)
        },
    );
    engine.register_fn(
        "hotkey",
        |host: &mut ScriptHost, chord: &str| -> Result<(), Box<EvalAltResult>> {
            host.note(format!("hotkey {chord}"));
            crate::packs::send_chord(chord).map_err(err)
        },
    );
    engine.register_fn("sleep", |host: &mut ScriptHost, ms: i64| {
        let ms = ms.clamp(0, 10_000) as u64;
        host.note(format!("sleep {ms} ms"));
        std::thread::sleep(Duration::from_millis(ms));
    });
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
            host.note(format!("process_start {path}"));
            crate::packs::script_process_start(path).map_err(err)
        },
    );

    // ---- files (reads anywhere, writes only in config locations / the target folder) ----
    engine.register_fn("read_file", |host: &mut ScriptHost, path: &str| -> ImmutableString {
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
    });
    engine.register_fn(
        "write_file",
        |host: &mut ScriptHost, path: &str, text: &str| -> Result<(), Box<EvalAltResult>> {
            let target_dir = host.target_dir.clone();
            if !write_allowed(Path::new(path), &target_dir) {
                host.note(format!("write_file {path} REFUSED (outside the allowed folders)"));
                return Err(err(format!(
                    "write_file refused: {path} is outside %APPDATA%, %LOCALAPPDATA% and the target's folder"
                )));
            }
            host.note(format!("write_file {path} ({} bytes)", text.len()));
            std::fs::write(path, text).map_err(|e| err(format!("write_file {path}: {e}")))
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

    engine
}

/// Writes are limited to the places app configs actually live, plus the app's own folder.
fn write_allowed(path: &Path, target_dir: &str) -> bool {
    let roots: Vec<String> = allowed_roots(target_dir)
        .iter()
        .map(|p| p.to_string_lossy().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .collect();
    if let Some(parent) = path.parent() {
        if let Ok(canon) = parent.canonicalize() {
            let canon = canon.to_string_lossy().to_ascii_lowercase();
            return roots.iter().any(|root| canon.starts_with(root));
        }
    }
    // parent missing: fall back to a plain prefix check on the path as written
    let lowered = path.to_string_lossy().to_ascii_lowercase();
    roots.iter().any(|root| lowered.starts_with(root))
}

/// `[section]` + `key = value`, case-insensitive, UTF-16/BOM aware.
fn read_ini_value(path: &Path, section: &str, key: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        let words: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
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
        if in_section && found_section {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case(key) {
                    return Some(v.trim().to_string());
                }
            }
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
    fn ini_reading_handles_sections_and_case() {
        let dir = std::env::temp_dir().join(format!("armoury-ini-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowed_roots_include_the_target_folder() {
        let roots = allowed_roots("C:\\Games\\App");
        assert!(roots.iter().any(|r| r == Path::new("C:\\Games\\App")));
    }
}
