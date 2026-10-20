//! Embeds the app icon in the executable as a PE resource.
//!
//! The artwork is `assets/ArmouryDaemon.ico`, which `examples/make_icon.rs` generates from the
//! app's own drawing (`armoury_core::icon::rgba_icon`) - the same one the tray uses - so the
//! file icon and the tray icon cannot drift apart.
//!
//! The resource is compiled with Microsoft's resource compiler (`rc.exe`) and handed to the
//! linker as a `.res` file. The Windows SDK is not a build dependency: when `rc.exe` is not
//! installed the build says so and carries on without the icon. A missing icon is much better
//! than a build that fails on a machine without the SDK.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// The icon file, relative to the crate root.
const ICON: &str = "assets/ArmouryDaemon.ico";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");

    // Only the MSVC linker takes a `.res` file on its command line.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        println!("cargo:warning=the icon is embedded in MSVC builds only; this one has none");
        return;
    }

    let Some(out) = env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(ICON);

    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe (the Windows resource compiler) was not found, so the executable is built without an icon. Install the Windows SDK to get one.");
        return;
    };

    // The resource script: one icon group under id 1 - the id the shell looks for.
    let rc_file = out.join("armoury_icon.rc");
    let res_file = out.join("armoury_icon.res");
    // The resource compiler takes forward slashes, and the path may hold spaces.
    let icon_path = icon.to_string_lossy().replace('\\', "/");
    let script = format!("1 ICON \"{icon_path}\"\n");
    if let Err(e) = fs::write(&rc_file, script) {
        println!("cargo:warning=the resource script could not be written ({e}), so the executable is built without an icon");
        return;
    }

    match Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .output()
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let message = String::from_utf8_lossy(&out.stderr);
            println!("cargo:warning=rc.exe failed, so the executable is built without an icon. {}", message.trim());
            return;
        }
        Err(e) => {
            println!("cargo:warning=rc.exe could not be run ({e}), so the executable is built without an icon");
            return;
        }
    }

    println!("cargo:rustc-link-arg-bins={}", res_file.display());
    println!("embedded {} as a PE resource", icon.display());
}

/// The newest `rc.exe` of the installed Windows SDKs, if there is one.
///
/// The SDKs live in `Windows Kits\10\bin\<version>\x64`; the newest version wins. The `RC`
/// environment variable overrides the search and points at `rc.exe` itself.
fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC") {
        let rc = PathBuf::from(rc);
        if rc.is_file() {
            return Some(rc);
        }
    }

    let mut newest: Option<(Vec<u32>, PathBuf)> = None;
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Some(base) = env::var_os(var) else {
            continue;
        };
        let bin = PathBuf::from(base).join("Windows Kits").join("10").join("bin");
        let Ok(versions) = fs::read_dir(&bin) else {
            continue;
        };
        for entry in versions.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let version: Vec<u32> = name.split('.').filter_map(|part| part.parse().ok()).collect();
            if version.is_empty() {
                continue;
            }
            let rc = dir.join("x64").join("rc.exe");
            if !rc.is_file() {
                continue;
            }
            let newer = match &newest {
                Some((found, _)) => version > *found,
                None => true,
            };
            if newer {
                newest = Some((version, rc));
            }
        }
    }
    newest.map(|(_, rc)| rc)
}
