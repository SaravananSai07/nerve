use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const FILE_MAX_BYTES: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERR ",
        }
    }
}

/// Append-only file sink shared by every `log_*!` macro. Configured
/// once at startup via `init`; calls before that point are silently
/// dropped, which means the macros are safe to call from any module
/// without ordering ceremony.
static SINK: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

pub fn init(file: Option<PathBuf>) {
    let _ = SINK.set(Mutex::new(file));
}

pub fn record(level: Level, message: String) {
    let Some(sink) = SINK.get() else { return };
    let Ok(guard) = sink.lock() else { return };
    let Some(path) = guard.as_deref() else { return };
    let _ = append_to_file(path, level, &message);
}

fn append_to_file(path: &Path, level: Level, message: &str) -> std::io::Result<()> {
    // Best-effort rotation. The rename + reopen race is benign — at
    // worst one log line lands in the rolled-over file.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > FILE_MAX_BYTES {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writeln!(f, "[{secs}] {} {message}", level.tag())
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Info, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Warn, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_err {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Error, format!($($arg)*))
    };
}
