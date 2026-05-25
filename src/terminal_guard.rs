use crossterm::event::{DisableFocusChange, EnableFocusChange};
use crossterm::execute;
use ratatui::DefaultTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

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

        if let Err(e) = execute!(std::io::stdout(), EnableFocusChange) {
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
    let _ = execute!(std::io::stdout(), DisableFocusChange);
    ratatui::restore();
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
}
