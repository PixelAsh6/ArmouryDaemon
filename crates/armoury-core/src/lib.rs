pub mod config;
pub mod engine;
pub mod icon;
pub mod logging;
pub mod protocol;
pub mod status;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PIPE_NAME: &str = "armoury-daemon.pipe";
