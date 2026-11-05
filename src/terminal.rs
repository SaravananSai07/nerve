//! The terminal nerve draws in: TUI mode on and off (`TerminalGuard`),
//! noticing the terminal go away (`tty_alive`, the watchdog), and the
//! signals that ask nerve to quit (`ShutdownFlag`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange};
use crossterm::execute;
use crossterm::style::Print;
use ratatui::DefaultTerminal;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

/// Clicks and the wheel (1000) in SGR form (1006), but not pointer
/// motion (crossterm's `EnableMouseCapture` asks for that too, and every
/// move would wake the loop). Text selection still works with Shift held
/// (Option in some terminals).
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

/// Set true as soon as the terminal is in an altered state (alt-screen / raw
/// mode / focus reporting). `restore` checks-and-clears this flag so that
/// double-restoration (Drop + panic hook firing for the same crash) is a
/// no-op.
static GUARD_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) struct TerminalGuard {
    terminal: DefaultTerminal,
}

impl TerminalGuard {
    pub(crate) fn install() -> std::io::Result<Self> {
        // Install the panic hook before touching the terminal so that a panic
        // inside ratatui::init() (rare but possible) still calls restore().
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));

        let terminal = ratatui::init();
        // The terminal is dirty the moment ratatui::init() returns; mark it
        // before any further fallible operation so the error path can unwind.
        GUARD_ACTIVE.store(true, Ordering::SeqCst);

        if let Err(e) = execute!(std::io::stdout(), EnableFocusChange, EnableBracketedPaste, Print(MOUSE_ON)) {
            restore();
            return Err(e);
        }

        Ok(Self { terminal })
    }

    pub(crate) fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if !GUARD_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = execute!(std::io::stdout(), Print(MOUSE_OFF), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
}

/// True when the given fd is attached to a controlling terminal. When Ghostty
/// (or any host terminal) tears the pty down without delivering SIGHUP, this
/// is the canary that lets us exit cleanly instead of orphaning.
fn is_controlling_tty<F: std::os::fd::AsFd>(fd: F) -> bool {
    nix::unistd::tcgetpgrp(fd.as_fd()).is_ok()
}

/// True once the far side of the pty has closed. When a tmux server or
/// terminal dies abruptly, macOS keeps the slave as our controlling tty
/// (`tcgetpgrp` still succeeds) but `poll` reports the hang-up.
fn is_hung_up<F: std::os::fd::AsFd>(fd: F) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
    poll(&mut fds, PollTimeout::ZERO).is_ok() && fds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLHUP))
}

/// The terminal nerve runs in is still there.
pub(crate) fn tty_alive() -> bool {
    let stdin = std::io::stdin();
    is_controlling_tty(&stdin) && !is_hung_up(&stdin)
}

/// How long the main loop gets to notice a lost tty on its own before the
/// watchdog force-exits.
const WATCHDOG_GRACE: Duration = Duration::from_secs(2);

/// Exit the process when the tty goes away even if the main loop is stuck.
///
/// crossterm 0.28/0.29 `try_read` spins forever when `read(2)` on a hung-up
/// tty returns `Ok(0)`, so `event::poll` never returns and the loop's own
/// tty check is never reached — the orphaned process (reparented to
/// launchd) pins a core indefinitely. This thread doesn't depend on
/// crossterm returning. It acts only on tty loss: with the tty gone there's
/// no terminal state worth restoring, whereas exiting on a mere shutdown
/// signal could leave a live terminal in raw mode. The flock is released by
/// the kernel on exit.
pub(crate) fn spawn_tty_watchdog() {
    let spawned = std::thread::Builder::new().name("nerve-tty-watchdog".into()).spawn(|| {
        let mut lost_since: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if tty_alive() {
                lost_since = None;
                continue;
            }
            let since = *lost_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= WATCHDOG_GRACE {
                crate::log_info!("app: main loop unresponsive after losing tty; forcing exit");
                std::process::exit(0);
            }
        }
    });
    if let Err(e) = spawned {
        crate::log_warn!("app: tty watchdog failed to start: {e}");
    }
}

/// Shared flag that flips to `true` the moment any termination signal arrives.
/// The main loop polls it at the top of each iteration and exits cleanly.
#[derive(Clone)]
pub(crate) struct ShutdownFlag {
    inner: Arc<AtomicBool>,
}

impl ShutdownFlag {
    pub(crate) fn install() -> std::io::Result<Self> {
        let inner = Arc::new(AtomicBool::new(false));
        for sig in [SIGHUP, SIGTERM, SIGINT, SIGQUIT] {
            signal_hook::flag::register(sig, Arc::clone(&inner))?;
        }
        Ok(Self { inner })
    }

    pub(crate) fn requested(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self { inner: Arc::new(AtomicBool::new(false)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GUARD_ACTIVE is process-global, so the flag-machinery checks have to
    // share one serial test. Tests are otherwise parallel.
    #[test]
    fn restore_flag_machinery() {
        // Starting state: no guard installed.
        GUARD_ACTIVE.store(false, Ordering::SeqCst);
        restore();
        assert!(!GUARD_ACTIVE.load(Ordering::SeqCst));

        // Arm and restore: the flag is cleared.
        GUARD_ACTIVE.store(true, Ordering::SeqCst);
        restore();
        assert!(!GUARD_ACTIVE.load(Ordering::SeqCst));

        // Double restore is idempotent — the second call exits early.
        restore();
        assert!(!GUARD_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn devnull_is_not_a_controlling_tty() {
        let dev_null = std::fs::File::open("/dev/null").expect("/dev/null open");
        assert!(!is_controlling_tty(&dev_null));
    }

    // Regression: SIGKILLing a tmux server orphaned nerve at 100% CPU.
    // The slave stayed nerve's controlling tty, so the watchdog's
    // `tcgetpgrp` check never fired; only `poll` sees the hang-up.
    #[test]
    fn closed_pty_master_reads_as_hung_up() {
        use nix::fcntl::OFlag;
        use nix::pty::{grantpt, posix_openpt, ptsname, unlockpt};
        let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let slave_path = unsafe { ptsname(&master) }.unwrap();
        let slave = std::fs::OpenOptions::new().read(true).write(true).open(slave_path).unwrap();
        assert!(!is_hung_up(&slave));
        drop(master);
        // Under load the kernel can post the hang-up a moment after close.
        let hung = (0..100).any(|_| {
            is_hung_up(&slave) || {
                std::thread::sleep(Duration::from_millis(10));
                false
            }
        });
        assert!(hung);
    }
}
