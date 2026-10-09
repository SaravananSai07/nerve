#![deny(unreachable_pub)]

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

use crate::state::session::effective_cost;
use crate::util::sanitize::Sanitised;

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
    if BridgeId::from_str(s).and_then(|id| id.focus()).is_err() {
        activate_terminal_fallback();
    }
}

/// Stale id (closed tab) or parse error: fall back to plain Ghostty
/// activation so a notification click is never a silent no-op.
#[cfg(target_os = "macos")]
fn activate_terminal_fallback() {
    let _ = std::process::Command::new("osascript")
        .args(["-e", "tell application \"Ghostty\" to activate"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Linux / non-Ghostty: nothing useful to do — exit quietly.
#[cfg(not(target_os = "macos"))]
fn activate_terminal_fallback() {}

fn handle_update() -> std::io::Result<()> {
    // `--locked` builds against the published Cargo.lock. Without it cargo
    // resolves the newest dependencies, which can need a newer rustc than
    // the declared MSRV (instability/darling did).
    println!("Updating nerve via cargo install nerve-tui --locked --force");
    let status = std::process::Command::new("cargo")
        .args(["install", "nerve-tui", "--locked", "--force"])
        .status()?;
    if status.success() {
        println!("\nnerve updated. Restart any running instance to use the new version.");
    } else {
        eprintln!("\nUpdate failed. Run `cargo install nerve-tui --locked --force` directly to see errors.");
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

    // Checked first, and only as argv[1]: everything after `--` belongs
    // to the wrapped command and must not be read as nerve flags.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("statusline") {
        let wrapped = match argv.iter().position(|a| a == "--") {
            Some(i) => &argv[i + 1..],
            None => &[][..],
        };
        return detect::statusline::run(&paths.statusline_dir(), wrapped);
    }

    if let Some(arg) = parse_focus_arg() {
        handle_focus(&arg);
        return Ok(());
    }

    if std::env::args().any(|a| a == "update") {
        return handle_update();
    }

    if std::env::args().any(|a| a == "--dump") {
        let sessions = detect::claude::discover_sessions(Some(paths.statusline_dir()));
        match serde_json::to_string_pretty(&sessions) {
            Ok(rendered) => println!("{rendered}"),
            Err(e) => {
                // Scripts pipe `--dump | jq`; previously a serde failure
                // produced "error: <msg>" on stdout with exit 0, breaking
                // downstream parsing silently. Emit a valid empty array so
                // the JSON contract holds, route the diagnosis to stderr,
                // and exit non-zero so `&&` chains see the failure.
                eprintln!("nerve: --dump serialisation failed: {e}");
                println!("[]");
                std::process::exit(2);
            }
        }
        return Ok(());
    }

    if std::env::args().any(|a| a == "--list") {
        let sessions = detect::claude::discover_sessions(Some(paths.statusline_dir()));
        if sessions.is_empty() {
            println!("No active sessions found.");
        }
        // CLI doesn't carry registry-owned state, so we don't print
        // duration or sparkline — both would always read as zero.
        for s in &sessions {
            let official = s.official.as_ref();
            let token_info = match official.and_then(|o| o.cost_usd) {
                Some(_) => {
                    let cost = effective_cost(official, &s.usage);
                    match official.and_then(|o| o.context_pct) {
                        Some(pct) => format!(" | ctx {pct:.0}% | ${cost:.2}"),
                        None => format!(" | ${cost:.2}"),
                    }
                }
                None if s.usage.total_tokens() > 0 => format!(" | {}", s.usage.compact_display()),
                None => String::new(),
            };
            let state = match &s.waiting_for {
                Some(why) => format!("{} ({why})", s.detected_state.label()),
                None => s.detected_state.label(),
            };
            let location = s.kind.location(s.tty.as_ref());
            println!(
                "{} | {} | {} | {}{}",
                s.name,
                state,
                location,
                s.branch.as_ref().map(Sanitised::as_str).unwrap_or("—"),
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
