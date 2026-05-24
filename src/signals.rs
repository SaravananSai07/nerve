use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::log_warn;

/// Shared flag that flips to `true` the moment any termination signal arrives.
/// The main loop polls it at the top of each iteration and exits cleanly.
#[derive(Clone)]
pub struct ShutdownFlag {
    inner: Arc<AtomicBool>,
}

impl ShutdownFlag {
    pub fn install() -> std::io::Result<Self> {
        let inner = Arc::new(AtomicBool::new(false));
        for sig in [SIGHUP, SIGTERM, SIGINT, SIGQUIT] {
            signal_hook::flag::register(sig, Arc::clone(&inner))?;
        }
        Ok(Self { inner })
    }

    pub fn requested(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub fn raise(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }
}

/// Spawn a background thread that reaps zombie children whenever SIGCHLD
/// arrives. Without this, every `osascript`/`terminal-notifier` we
/// `.spawn()` leaves a zombie until the process exits — over a long
/// session, that's a slow process-table DoS.
pub fn spawn_child_reaper() -> std::io::Result<()> {
    let mut signals = Signals::new([SIGCHLD])?;
    std::thread::Builder::new()
        .name("nerve-reaper".into())
        .spawn(move || {
            for _sig in signals.forever() {
                drain_zombies();
            }
        })?;
    // Reap any children that died before the handler was installed (rare,
    // but free insurance for race-on-startup).
    drain_zombies();
    Ok(())
}

fn drain_zombies() {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(_) => continue,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(e) => {
                log_warn!("reaper: waitpid failed: {e}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raised_flag_reports_requested() {
        let flag = ShutdownFlag::for_test();
        assert!(!flag.requested());
        flag.raise();
        assert!(flag.requested());
    }

    #[test]
    fn drain_zombies_reaps_dead_child() {
        // Spawn `true`, which exits immediately, and deliberately drop the
        // Child handle without waiting. The process is now a zombie until
        // someone waitpid()s for it.
        let child = std::process::Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        drop(child);

        // Wait briefly for the child to actually exit.
        std::thread::sleep(std::time::Duration::from_millis(150));

        drain_zombies();

        // After draining, waitpid for the specific pid should return
        // ECHILD (the kernel no longer has any record of it).
        let res = nix::sys::wait::waitpid(
            nix::unistd::Pid::from_raw(pid as i32),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        );
        assert!(
            matches!(res, Err(nix::errno::Errno::ECHILD)),
            "expected ECHILD after drain_zombies, got {res:?}",
        );
    }

    // NB: there is no test for `install()` here — signal_hook registrations
    // are process-global and cannot be torn down once installed, so calling
    // install() in tests would leak signal handlers into every subsequent
    // test in the same binary. install() is exercised by the real binary.
}
