use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const RING_CAPACITY: usize = 256;
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

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub at: SystemTime,
    pub level: Level,
    pub message: String,
}

struct Sink {
    ring: Vec<LogEntry>,
    head: usize,
    file: Option<PathBuf>,
}

impl Sink {
    fn push(&mut self, entry: LogEntry) {
        if let Some(path) = self.file.as_deref() {
            let _ = append_to_file(path, &entry);
        }
        if self.ring.len() < RING_CAPACITY {
            self.ring.push(entry);
        } else {
            self.ring[self.head] = entry;
            self.head = (self.head + 1) % RING_CAPACITY;
        }
    }

    #[allow(dead_code)]
    fn snapshot(&self) -> Vec<LogEntry> {
        if self.ring.len() < RING_CAPACITY {
            self.ring.clone()
        } else {
            let mut out = Vec::with_capacity(RING_CAPACITY);
            out.extend_from_slice(&self.ring[self.head..]);
            out.extend_from_slice(&self.ring[..self.head]);
            out
        }
    }
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

pub fn init(file: Option<PathBuf>) {
    let _ = SINK.set(Mutex::new(Sink {
        ring: Vec::with_capacity(RING_CAPACITY),
        head: 0,
        file,
    }));
}

pub fn record(level: Level, message: String) {
    let Some(sink) = SINK.get() else { return };
    if let Ok(mut s) = sink.lock() {
        s.push(LogEntry {
            at: SystemTime::now(),
            level,
            message,
        });
    }
}

#[allow(dead_code)]
pub fn snapshot() -> Vec<LogEntry> {
    SINK.get()
        .and_then(|s| s.lock().ok().map(|s| s.snapshot()))
        .unwrap_or_default()
}

fn append_to_file(path: &Path, entry: &LogEntry) -> std::io::Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > FILE_MAX_BYTES {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let secs = entry
        .at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writeln!(f, "[{secs}] {} {}", entry.level.tag(), entry.message)
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
