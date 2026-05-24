use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

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

    // NB: there is no test for `install()` here — signal_hook registrations
    // are process-global and cannot be torn down once installed, so calling
    // install() in tests would leak signal handlers into every subsequent
    // test in the same binary. install() is exercised by the real binary.
}
