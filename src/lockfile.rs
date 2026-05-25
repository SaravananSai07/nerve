use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use nix::fcntl::{Flock, FlockArg};

use crate::log_warn;
use crate::paths::Paths;

const LOCK_PID_MAX_BYTES: u64 = 64;

/// Owns the kernel `flock(2)` lease on `~/.config/nerve/nerve.lock`. Dropping
/// the value releases the lease (kernel reaps the fd on close → flock auto-
/// releases). The pid of the holder is written into the file for diagnostics.
#[derive(Debug)]
pub(crate) struct LockFile {
    _lock: Flock<File>,
}

#[derive(Debug)]
pub(crate) enum LockError {
    AlreadyHeld { pid: Option<u32> },
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHeld { pid: Some(p) } => write!(f, "nerve is already running as PID {p}"),
            Self::AlreadyHeld { pid: None } => write!(f, "nerve is already running"),
            Self::Io(e) => write!(f, "lockfile io error: {e}"),
        }
    }
}

impl LockFile {
    pub(crate) fn acquire(paths: &Paths) -> Result<Self, LockError> {
        paths.ensure_config_dir().map_err(LockError::Io)?;
        let path = paths.lock_file();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(LockError::Io)?;

        let mut lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(l) => l,
            Err((mut file, _errno)) => {
                let mut buf = String::new();
                let _ = (&mut file).take(LOCK_PID_MAX_BYTES).read_to_string(&mut buf);
                let pid = buf.trim().parse::<u32>().ok();
                return Err(LockError::AlreadyHeld { pid });
            }
        };

        // Stamp the current pid into the file so a second invocation can
        // report it in the "already running" message.
        if let Err(e) = stamp_pid(&mut lock) {
            log_warn!("lockfile: unable to stamp pid: {e}");
        }

        Ok(Self { _lock: lock })
    }
}

fn stamp_pid(lock: &mut Flock<File>) -> std::io::Result<()> {
    lock.set_len(0)?;
    lock.seek(SeekFrom::Start(0))?;
    writeln!(**lock, "{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::for_test(tmp.path().join("config"), tmp.path().join("home"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        (tmp, paths)
    }

    #[test]
    fn first_acquire_succeeds_and_stamps_pid() {
        let (_tmp, paths) = fixture_paths();
        let lock = LockFile::acquire(&paths).expect("first acquire");
        let contents = std::fs::read_to_string(paths.lock_file()).unwrap();
        assert_eq!(contents.trim(), std::process::id().to_string());
        drop(lock);
    }

    #[test]
    fn second_acquire_fails_with_pid_of_holder() {
        let (_tmp, paths) = fixture_paths();
        let _held = LockFile::acquire(&paths).expect("first acquire");
        match LockFile::acquire(&paths) {
            Err(LockError::AlreadyHeld { pid: Some(p) }) => {
                assert_eq!(p, std::process::id());
            }
            other => panic!("expected AlreadyHeld with pid, got {other:?}"),
        }
    }

    #[test]
    fn lock_releases_on_drop() {
        let (_tmp, paths) = fixture_paths();
        let lock = LockFile::acquire(&paths).expect("first acquire");
        drop(lock);
        let _again = LockFile::acquire(&paths).expect("re-acquire after drop");
    }
}
