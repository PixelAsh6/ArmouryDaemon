pub mod config;
pub mod engine;
pub mod icon;
pub mod logging;
pub mod pack;
pub mod protocol;
pub mod status;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Base name of the daemon's named pipe.
pub const PIPE_NAME: &str = "armoury-daemon.pipe";

/// The pipe to use. `ARMOURY_TEST_NS` gives a throw-away namespace so the integration harness
/// (`tools/daemon-selftest.ps1`) can run a real daemon instance next to the installed one, with its
/// own pipe, config folder and single-instance guard. Unset in normal use.
pub fn pipe_name() -> String {
    match std::env::var("ARMOURY_TEST_NS") {
        Ok(ns) if !ns.trim().is_empty() => format!("{PIPE_NAME}-{}", ns.trim()),
        _ => PIPE_NAME.to_string(),
    }
}
