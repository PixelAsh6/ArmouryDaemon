//! Embeds the app icon and the application manifest in the executable as PE resources.
//!
//! The artwork is `assets/ArmouryDaemon.ico`, which `examples/make_icon.rs` generates from the
//! app's own drawing (`armoury_core::icon::rgba_icon`) - the same one the tray uses - so the
//! file icon and the tray icon cannot drift apart.
//!
//! The manifest is `assets/ArmouryDaemon.manifest`. It asks Windows for administrator rights,
//! so the executable is elevated from the first double-click - the killswitch can then hide the
//! windows of other apps, and the daemon registers its own autostart task without a consent
//! prompt. It is embedded as `RT_MANIFEST` (resource type 24) under id 1: that is the id the
//! loader looks for (`CREATEPROCESS_MANIFEST_RESOURCE_ID`). The icon group is
//! `RT_GROUP_ICON` (type 14) under id 1 as well - the ids are counted per resource type, so
//! the icon and the manifest do not collide.
//!
//! Both resources are compiled with Microsoft's resource compiler (`rc.exe`) and handed to the
//! linker as a `.res` file. The Windows SDK is not a build dependency: when `rc.exe` is not
//! installed the build says so and carries on without the resources. A missing icon is much
//! better than a build that fails on a machine without the SDK - and a build without the
//! manifest simply is not auto-elevated; everything else still works.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// The icon file, relative to the crate root.
const ICON: &str = "assets/ArmouryDaemon.ico";

/// The application manifest, relative to the crate root.
const MANIFEST: &str = "assets/ArmouryDaemon.manifest";

/// The resource script the resource compiler gets.
///
/// The icon group is `RT_GROUP_ICON` (type 14) under id 1 - the id the shell looks for. The
/// manifest is `RT_MANIFEST` (type 24) under id 1 - the id the loader looks for. The ids are
/// counted per resource type, so the two resources do not collide.
fn resource_script(icon: &str, manifest: Option<&str>) -> String {
    let mut script = format!("1 ICON \"{icon}\"\n");
    if let Some(manifest) = manifest {
        script.push_str(&format!("1 24 \"{manifest}\"\n"));
    }
    script
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    println!("cargo:rerun-if-changed={MANIFEST}");

    // Only the MSVC linker takes a `.res` file on its command line.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        println!("cargo:warning=the icon and the manifest are embedded in MSVC builds only; this one has none");
        return;
    }

    let Some(out) = env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let icon = root.join(ICON);

    // The manifest is what makes Windows ask for administrator rights. A build without it
    // still works - the daemon is simply not elevated.
    let manifest = root.join(MANIFEST);
    let manifest = if manifest.is_file() {
        Some(manifest)
    } else {
        println!("cargo:warning=the application manifest {MANIFEST} is missing, so the executable is built without it and Windows will not elevate it");
        None
    };

    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe (the Windows resource compiler) was not found, so the executable is built without an icon and without its manifest - Windows will not elevate it. Install the Windows SDK to get them.");
        return;
    };

    // The resource script: the icon group and the manifest, both under id 1.
    let rc_file = out.join("armoury_resources.rc");
    let res_file = out.join("armoury_resources.res");
    // The resource compiler takes forward slashes, and the path may hold spaces.
    let icon_path = icon.to_string_lossy().replace('\\', "/");
    let manifest_path = manifest
        .as_ref()
        .map(|m| m.to_string_lossy().replace('\\', "/"));
    let script = resource_script(&icon_path, manifest_path.as_deref());
    if let Err(e) = fs::write(&rc_file, script) {
        println!("cargo:warning=the resource script could not be written ({e}), so the executable is built without an icon and without its manifest");
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
            println!("cargo:warning=rc.exe failed, so the executable is built without an icon and without its manifest. {}", message.trim());
            return;
        }
        Err(e) => {
            println!("cargo:warning=rc.exe could not be run ({e}), so the executable is built without an icon and without its manifest");
            return;
        }
    }

    println!("cargo:rustc-link-arg-bins={}", res_file.display());
    println!("embedded {} as a PE resource", icon.display());
    if let Some(manifest) = &manifest {
        println!(
            "embedded {} as the PE manifest (RT_MANIFEST, resource id 1)",
            manifest.display()
        );
    }
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
