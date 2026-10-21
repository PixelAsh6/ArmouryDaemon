use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The current log file is rolled as soon as it grows past this.
const MAX_BYTES: u64 = 512 * 1024;
/// How many generations are kept: <component>.log, <component>.log.1, <component>.log.2.
const GENERATIONS: usize = 3;

/// Initialize file logging to %APPDATA%\ArmouryDaemon\logs\<component>.log
/// with rotation: the file is rolled to <component>.log.1 (and .log.2) as soon as it
/// grows past 512 KiB, during the session and not only at startup.
/// Set ARMOURY_LOG_STDERR=1 to mirror all lines to stderr (dev).
pub fn init(component: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dir = crate::config::config_dir().join("logs");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{component}.log"));

    // The single-generation file of the old scheme: the generation chain replaces it.
    let _ = fs::remove_file(dir.join(format!("{component}.log.old")));

    let mut sink = Sink::open(&path)?;
    if sink.size > MAX_BYTES {
        // A file that was already oversized when we started is rolled before we append.
        if let Err(e) = sink.roll() {
            eprintln!("{component}: the log file could not be rolled: {e}");
        }
    }

    let logger = Logger {
        sink: Mutex::new(sink),
        component: component.to_string(),
        mirror_stderr: std::env::var_os("ARMOURY_LOG_STDERR").is_some(),
    };
    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(LevelFilter::Info);
    Ok(())
}

/// The current log file plus what we know about its size.
struct Sink {
    file: File,
    /// Size of the current file: what was there when we opened it plus what we wrote.
    size: u64,
    /// Size at which the next roll is attempted (after a failed one).
    retry_at: u64,
    path: PathBuf,
}

impl Sink {
    fn open(path: &Path) -> io::Result<Sink> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Sink {
            file,
            size,
            retry_at: 0,
            path: path.to_path_buf(),
        })
    }

    /// Write one line. The file is rolled first when it grew past the limit, so the
    /// line is never lost: it lands in the fresh file.
    fn write(&mut self, line: &str) {
        if self.size > MAX_BYTES && self.size >= self.retry_at && self.roll().is_err() {
            // Do not hammer a failing roll on every line: try again after another
            // MAX_BYTES of writing.
            self.retry_at = self.size + MAX_BYTES;
        }
        if self.file.write_all(line.as_bytes()).is_err() {
            return;
        }
        self.size += line.len() as u64;
    }

    /// Roll the current file to the next generation and start a new one.
    fn roll(&mut self) -> io::Result<()> {
        let _ = self.file.flush();
        roll_generations(&self.path)?;
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.file = file;
                self.size = 0;
                self.retry_at = 0;
                Ok(())
            }
            Err(e) => {
                // The new file could not be opened: put the old one back and keep
                // writing to it (the open handle still points at it).
                let _ = fs::rename(generation(&self.path, 1), &self.path);
                Err(e)
            }
        }
    }
}

/// `<component>.log` -> `<component>.log.1`
fn generation(path: &Path, generation: usize) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{generation}"));
    path.with_file_name(name)
}

/// Shift the generations: the current file becomes `.1`, `.1` becomes `.2` and the
/// oldest generation is dropped. The folder cannot grow without bound.
fn roll_generations(path: &Path) -> io::Result<()> {
    let _ = fs::remove_file(generation(path, GENERATIONS - 1));
    for step in (1..GENERATIONS - 1).rev() {
        let from = generation(path, step);
        if from.exists() {
            fs::rename(&from, generation(path, step + 1))?;
        }
    }
    fs::rename(path, generation(path, 1))?;
    Ok(())
}

struct Logger {
    sink: Mutex<Sink>,
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
        if let Ok(mut sink) = self.sink.lock() {
            sink.write(&line);
        }
        if self.mirror_stderr || record.level() <= Level::Warn {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut sink) = self.sink.lock() {
            let _ = sink.file.flush();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    /// A throw-away directory that removes itself when the test ends.
    ///
    /// The name carries the process id and a counter, so two concurrent test runs never
    /// share one.
    fn temp_dir(name: &str) -> Scratch {
        Scratch::dir(&format!("logging-{name}"))
    }

    #[test]
    fn the_file_is_rolled_when_it_grows_past_the_limit() {
        let dir = temp_dir("roll");
        let path = dir.join("ui.log");
        let mut sink = Sink::open(&path).unwrap();
        let chunk = "a".repeat(4096);
        for _ in 0..600 {
            sink.write(&format!("{chunk}\n"));
        }
        assert!(path.exists());
        assert!(dir.join("ui.log.1").exists());
        // the file is rolled as soon as it passes the limit
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES + 4097);
    }

    #[test]
    fn the_generations_shift_and_stay_bounded() {
        let dir = temp_dir("generations");
        let path = dir.join("ui.log");
        let mut sink = Sink::open(&path).unwrap();
        let chunk = "a".repeat(4096);
        for _ in 0..800 {
            sink.write(&format!("{chunk}\n"));
        }
        assert!(dir.join("ui.log.1").exists());
        assert!(dir.join("ui.log.2").exists());
        assert!(!dir.join("ui.log.3").exists());
        let total: u64 = ["ui.log", "ui.log.1", "ui.log.2"]
            .iter()
            .map(|name| fs::metadata(dir.join(name)).map(|m| m.len()).unwrap_or(0))
            .sum();
        assert!(total <= 3 * (MAX_BYTES + 8192), "the log folder grew to {total} bytes");
    }

    #[test]
    fn a_roll_keeps_the_old_lines() {
        let dir = temp_dir("keep");
        let path = dir.join("ui.log");
        let mut sink = Sink::open(&path).unwrap();
        sink.write("first line\n");
        sink.roll().unwrap();
        let old = fs::read_to_string(dir.join("ui.log.1")).unwrap();
        assert!(old.contains("first line"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn an_oversized_file_is_rolled_at_startup() {
        let dir = temp_dir("startup");
        let path = dir.join("ui.log");
        fs::write(&path, vec![b'x'; MAX_BYTES as usize + 16]).unwrap();
        let mut sink = Sink::open(&path).unwrap();
        assert!(sink.size > MAX_BYTES);
        sink.roll().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert!(dir.join("ui.log.1").exists());
    }
}
