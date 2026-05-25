mod app;
mod config;
mod detect;
mod lockfile;
mod log;
mod notify;
mod paths;
mod platform;
mod signals;
mod state;
mod terminal_guard;
mod tui;
mod updater;
mod util;
mod workers;

use std::str::FromStr;

use app::App;
use paths::Paths;
use platform::BridgeId;


/// Raise `RLIMIT_NOFILE` soft limit up to `min(hard, 4096)`.
/// Best-effort: failures are logged but don't block startup.
fn raise_fd_limit() {
    use nix::libc;
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
    if rc != 0 {
        log_warn!("rlimit: getrlimit failed");
        return;
    }
    let target = rlim.rlim_max.min(4096);
    if rlim.rlim_cur >= target {
        return;
    }
    let new = libc::rlimit {
        rlim_cur: target,
        rlim_max: rlim.rlim_max,
    };
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) };
    if rc != 0 {
        log_warn!("rlimit: setrlimit to {target} failed");
    }
}

fn parse_focus_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--focus" {
            return args.next().and_then(accept_focus_arg);
        }
        if let Some(val) = a.strip_prefix("--focus=") {
            return accept_focus_arg(val.to_string());
        }
    }
    None
}

fn accept_focus_arg(s: String) -> Option<String> {
    use util::focus_arg::{validate, FocusValidation};
    match validate(s) {
        FocusValidation::Ok(v) => Some(v),
        FocusValidation::Empty => {
            eprintln!("nerve: --focus argument is empty");
            None
        }
        FocusValidation::TooLong(n) => {
            eprintln!("nerve: --focus argument is too long ({n} bytes)");
            None
        }
        FocusValidation::ForbiddenChar(_) => {
            eprintln!("nerve: --focus argument contains forbidden characters");
            None
        }
    }
}

fn handle_focus(s: &str) {
    let focused = BridgeId::from_str(s).and_then(|id| id.focus());
    if focused.is_ok() {
        return;
    }

    // Stale id (closed tab) or parse error: fall back to plain Ghostty activation
    // so a notification click is never a silent no-op. Linux/non-Ghostty: nothing
    // useful to do — exit quietly.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .args(["-e", "tell application \"Ghostty\" to activate"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn handle_update() -> std::io::Result<()> {
    println!("Updating nerve via cargo install nerve-tui --force");
    let status = std::process::Command::new("cargo")
        .args(["install", "nerve-tui", "--force"])
        .status()?;
    if status.success() {
        println!("\nnerve updated. Restart any running instance to use the new version.");
    } else {
        eprintln!("\nUpdate failed. Run `cargo install nerve-tui --force` directly to see errors.");
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let paths = match Paths::discover() {
        Some(p) => p,
        None => {
            eprintln!("nerve: unable to resolve $HOME or config dir; refusing to start");
            return Ok(());
        }
    };
    let _ = paths.ensure_config_dir();
    log::init(Some(paths.log_file()));

    // Many active sessions × open JSONLs × occasional notification
    // spawn can run into the default 256-fd cap on macOS. Raise
    // the soft limit early; failures are logged but non-fatal.
    raise_fd_limit();

    if let Some(arg) = parse_focus_arg() {
        handle_focus(&arg);
        return Ok(());
    }

    if std::env::args().any(|a| a == "update") {
        return handle_update();
    }

    if std::env::args().any(|a| a == "--dump") {
        let sessions = detect::claude::discover_sessions();
        println!(
            "{}",
            serde_json::to_string_pretty(&sessions).unwrap_or_else(|e| format!("error: {e}"))
        );
        return Ok(());
    }

    if std::env::args().any(|a| a == "--list") {
        let sessions = detect::claude::discover_sessions();
        if sessions.is_empty() {
            println!("No active sessions found.");
        }
        // CLI doesn't carry registry-owned state, so we don't print
        // duration or sparkline — both would always read as zero.
        for s in &sessions {
            let token_info = if s.usage.total_tokens() > 0 {
                format!(" | {}", s.usage.compact_display())
            } else {
                String::new()
            };
            println!(
                "{} | {} | {} | {}{}",
                s.name,
                s.detected_state.label(),
                s.tty.as_deref().unwrap_or("?"),
                s.branch.as_deref().unwrap_or("—"),
                token_info,
            );
        }
        return Ok(());
    }

    let shutdown = signals::ShutdownFlag::install()?;
    if let Err(e) = signals::spawn_child_reaper() {
        log_warn!("signals: child reaper failed to start: {e}");
    }

    let _lock = match lockfile::LockFile::acquire(&paths) {
        Ok(l) => l,
        Err(lockfile::LockError::AlreadyHeld { pid }) => {
            match pid {
                Some(p) => eprintln!(
                    "nerve is already running as PID {p}. Switch to it (Cmd-Tab) or quit it with 'q'."
                ),
                None => eprintln!("nerve is already running. Quit the other instance with 'q'."),
            }
            return Ok(());
        }
        Err(lockfile::LockError::Io(e)) => return Err(e),
    };

    let mut guard = terminal_guard::TerminalGuard::install()?;
    App::new(paths, shutdown).run(guard.terminal())
}
