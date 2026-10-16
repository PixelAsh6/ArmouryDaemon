use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

/// Initialize file logging to %APPDATA%\ArmouryDaemon\logs\<component>.log
/// with simple rotation (rename to .old when > 512 KiB).
/// Set ARMOURY_LOG_STDERR=1 to mirror all lines to stderr (dev).
pub fn init(component: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dir = crate::config::config_dir().join("logs");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{component}.log"));

    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > 512 * 1024 {
            let old = dir.join(format!("{component}.log.old"));
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&path, &old);
        }
    }

    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let logger = Logger {
        file: Mutex::new(file),
        component: component.to_string(),
        mirror_stderr: std::env::var_os("ARMOURY_LOG_STDERR").is_some(),
    };
    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(LevelFilter::Info);
    Ok(())
}

struct Logger {
    file: Mutex<File>,
    component: String,
    mirror_stderr: bool,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} [{}] {:<5} {}\n",
            local_timestamp(),
            self.component,
            record.level(),
            record.args()
        );
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
        if self.mirror_stderr || record.level() <= Level::Warn {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

#[cfg(windows)]
fn local_timestamp() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

#[cfg(not(windows))]
fn local_timestamp() -> String {
    "0000-00-00 00:00:00".to_string()
}
