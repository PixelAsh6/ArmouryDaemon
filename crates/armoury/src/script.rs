//! Spec 3 — scripted packs (Rhai).
//!
//! A pack may carry a script in `script.apply` / `script.revert`. The **pack never executes it**:
//! the daemon compiles the source, runs it in a fresh Rhai engine with hard limits, and exposes only
//! the functions registered below. Nothing else exists inside a script — no filesystem, network or
//! process API beyond these, no `eval`, no imports.
//!
//! Every host call is appended to the run's log (which the daemon writes to the pack journal).
//! Writes are restricted to the usual config locations and the target's own folder (a write may
//! create the folders it needs inside them), and a script may only run programs from those same
//! places.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use armoury_core::pack::TemplateCtx;
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

thread_local! {
    /// Compiled scripts, keyed by their source. A pack's script never changes, but it is run
    /// again on every apply and every revert.
    static ASTS: RefCell<HashMap<String, Rc<AST>>> = RefCell::new(HashMap::new());
}

/// The AST of a script, compiled once and kept for the next run.
fn compile_ast(source: &str) -> Result<Rc<AST>, String> {
    let cached = ASTS.with(|cache| cache.borrow().get(source).cloned());
    if let Some(ast) = cached {
        return Ok(ast);
    }
    let ast = Rc::new(
        engine()
            .compile(source)
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

/// Compile a script once (used to reject broken packs at load/install time).
pub fn compile(source: &str) -> Result<(), String> {
    compile_ast(source).map(|_| ())
}

/// Run one block script. Returns the host-call log; `ctx.captured` receives anything the script
/// captured (so a revert can replay it).
pub fn run(source: &str, ctx: &mut TemplateCtx) -> Result<Vec<String>, String> {
    let ast = compile_ast(source)?;

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
    let result = engine().eval_ast_with_scope::<()>(&mut scope, &ast);

    // the captures are harvested even when the script failed: a script that captured and then
    // failed must still leave the journal something to replay
    let host: Option<ScriptHost> = scope.get_value("ctx");
    if let Some(host) = &host {
        ctx.captured = host.captured.clone();
    }
    result.map_err(|e| format!("script failed: {e}"))?;
    let host = host.ok_or_else(|| "script lost its context".to_string())?;
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
            std::fs::write(path, text).map_err(|e| err(format!("write_file {}: {e}", path.display())))
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
    fn the_write_guard_compares_path_components() {
        let dir = std::env::temp_dir().join(format!("armoury-guard-{}", std::process::id()));
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
        let _ = std::fs::remove_dir_all(&dir);
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
        let dir = std::env::temp_dir().join(format!("armoury-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_whose_closest_existing_folder_is_outside_the_roots_is_refused() {
        let dir = std::env::temp_dir().join(format!("armoury-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_through_a_symlink_is_refused() {
        let dir = std::env::temp_dir().join(format!("armoury-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "x").unwrap();
        let link = root.join("link.txt");
        if std::os::windows::fs::symlink_file(&outside, &link).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return; // this user may not create symlinks: nothing to check
        }
        let roots = vec![root.clone()];
        assert!(
            !write_allowed_in(&link, &roots),
            "a symlink must not be written through"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
