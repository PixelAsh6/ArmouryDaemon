//! ArmouryConsole — the pad-driven console launcher as its own executable (Phase C1).
//!
//! Its whole job is the band: `console_ui::run()`, the same code the daemon binary runs
//! under `--console`. It starts no daemon, listens to no pipe and takes no arguments —
//! the daemon decides when it runs (installed helper, or the `--console` fallback on the
//! daemon binary), and it talks to that daemon over the namespaced JSONL pipe itself.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The band carries whole shared modules (art.rs's fetcher surface, pad.rs's hotplug
// helpers) whose daemon-facing half is not reachable from a launcher-only `main`. The
// same sources are linted by the daemon binary's own build, so this allow is a statement
// about this binary's reachability, not a blanket pass on the files.
#![allow(dead_code)]

// The modules the band carries. They live beside the daemon's own modules and are
// declared here with `#[path]`, because a crate root that is not `src/main.rs` resolves
// its submodule paths from its own folder (`src/console_bin/art.rs`) otherwise.
#[path = "art.rs"]
mod art;
#[path = "console_ui.rs"]
mod console_ui;
#[path = "dpi.rs"]
mod dpi;
#[path = "fonts.rs"]
mod fonts;
#[path = "net.rs"]
mod net;
#[path = "pad.rs"]
mod pad;
#[path = "pick.rs"]
mod pick;

// art.rs's tests write scratch folders. `test = false` on the bin target keeps them out
// of this binary's test builds — they run in the daemon binary's test harness, which
// compiles the same files — but the module stays declared so the file is honest either
// way: flip `test` back on and the tests still compile.
#[cfg(test)]
#[path = "testutil.rs"]
pub(crate) mod testutil;

fn main() {
    // Per-Monitor-V2 awareness first: the band measures the screen before its window
    // exists, and Win32 only hands out physical pixels to a process that says so.
    dpi::set();

    console_ui::run();
}
