#![deny(unreachable_pub)]

mod app;
mod config;
mod detect;
mod lockfile;
mod log;
mod notify;
mod paths;
mod platform;
mod state;
mod sync;
mod terminal;
mod tui;
mod updater;
mod util;

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
    let mut rlim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
    if rc != 0 {
        log_warn!("rlimit: getrlimit failed");
        return;
    }
    let target = rlim.rlim_max.min(4096);
    if rlim.rlim_cur >= target {
        return;
    }
    let new = libc::rlimit { rlim_cur: target, rlim_max: rlim.rlim_max };
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) };
    if rc != 0 {
        log_warn!("rlimit: setrlimit to {target} failed");
    }
}

const HELP: &str = "\
nerve — a dashboard for your coding-agent sessions

  nerve                    open the dashboard (needs a terminal)
  nerve --list             print active sessions, one per line
  nerve --dump             print active sessions as JSON
  nerve update             reinstall the latest release
  nerve sync <command>     lower-level sync commands (`nerve sync` lists them)
  nerve --version          print the version
  nerve --help             this text
";

/// A notification click: `--land <device>/<session>` leaves the session
/// for the running TUI, `--focus <id>` raises the tab it runs in.
fn handle_click(paths: &Paths, argv: &[String]) {
    let (mut land, mut focus) = (None, None);
    let mut args = argv.iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--land" => land = args.next(),
            "--focus" => focus = args.next(),
            _ => {}
        }
    }
    let both_safe = |v: &&String| v.split_once('/').is_some_and(|(d, s)| util::is_safe(d) && util::is_safe(s));
    if let Some(land) = land.filter(both_safe) {
        let _ = std::fs::write(paths.land_file(), land);
    }
    match focus {
        Some(id) if util::is_safe(id) => handle_focus(id),
        Some(_) => eprintln!("nerve: --focus takes a terminal id (letters, digits and : _ - % . $)"),
        None => {}
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
    // Without `--force` cargo still upgrades to a newer version, but skips
    // rebuilding the one already installed.
    let current = env!("CARGO_PKG_VERSION");
    let latest = latest_version();
    if latest.as_deref() == Some(current) {
        println!("nerve {current} is the latest version.");
        return Ok(());
    }
    match &latest {
        Some(v) => println!("Updating nerve {current} to {v} via cargo install nerve-tui --locked"),
        None => println!("Updating nerve {current} via cargo install nerve-tui --locked"),
    }
    let status = std::process::Command::new("cargo").args(["install", "nerve-tui", "--locked"]).status()?;
    if status.success() {
        println!("\nnerve updated. Restart any running instance to use the new version.");
    } else {
        eprintln!("\nUpdate failed. Run `cargo install nerve-tui --locked` directly to see errors.");
    }
    Ok(())
}

/// The newest nerve on crates.io, from `cargo search`; None when it can't tell.
fn latest_version() -> Option<String> {
    let out = std::process::Command::new("cargo").args(["search", "nerve-tui", "--limit", "1"]).output().ok()?;
    parse_search(&String::from_utf8_lossy(&out.stdout))
}

/// `nerve-tui = "0.6.1"    # …` → `0.6.1`.
fn parse_search(out: &str) -> Option<String> {
    let rest = out.lines().find_map(|l| l.strip_prefix("nerve-tui = \""))?;
    rest.split('"').next().filter(|v| !v.is_empty()).map(String::from)
}

fn dump(paths: &Paths) {
    let sessions = detect::claude::discover_sessions(Some(paths.statusline_dir()));
    match serde_json::to_string_pretty(&sessions) {
        Ok(rendered) => println!("{rendered}"),
        Err(e) => {
            // Scripts pipe `--dump | jq`: keep stdout valid JSON, say why
            // on stderr, and exit non-zero so `&&` chains see the failure.
            eprintln!("nerve: --dump serialisation failed: {e}");
            println!("[]");
            std::process::exit(2);
        }
    }
}

fn list(paths: &Paths) {
    let sessions = detect::claude::discover_sessions(Some(paths.statusline_dir()));
    if sessions.is_empty() {
        println!("No active sessions found.");
    }
    // The CLI has no registry, so how long a session has been in its
    // state is unknown; it would read as zero.
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

    // Only argv[1] picks the command: everything after `statusline --`
    // belongs to the wrapped command and must not be read as nerve flags.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        None => {}
        Some("statusline") => {
            let wrapped = match argv.iter().position(|a| a == "--") {
                Some(i) => &argv[i + 1..],
                None => &[][..],
            };
            return detect::statusline::run(&paths.statusline_dir(), wrapped);
        }
        Some("sync") => return sync::cli::run(&paths, &argv[1..]),
        Some("--version" | "-V") => {
            println!("nerve {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h" | "help") => {
            print!("{HELP}\n{}", sync::cli::HELP);
            return Ok(());
        }
        Some("--land" | "--focus") => {
            handle_click(&paths, &argv);
            return Ok(());
        }
        Some("update") => return handle_update(),
        Some("--dump") => {
            dump(&paths);
            return Ok(());
        }
        Some("--list") => {
            list(&paths);
            return Ok(());
        }
        Some(verb) => {
            if let Some(r) = sync::cli::verb(&paths, verb, &argv[1..]) {
                return r;
            }
            eprintln!("nerve: unknown command {verb:?} (see `nerve --help`)");
            std::process::exit(2);
        }
    }

    let shutdown = terminal::ShutdownFlag::install()?;

    let _lock = match lockfile::LockFile::acquire(&paths) {
        Ok(l) => l,
        Err(lockfile::LockError::AlreadyHeld { pid }) => {
            match pid {
                Some(p) => {
                    eprintln!("nerve is already running as PID {p}. Switch to it (Cmd-Tab) or quit it with 'q'.")
                }
                None => eprintln!("nerve is already running. Quit the other instance with 'q'."),
            }
            return Ok(());
        }
        Err(lockfile::LockError::Io(e)) => return Err(e),
    };

    // Scripts and agents run nerve with no terminal attached; ratatui::init()
    // panics there, so refuse before touching the terminal.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        eprintln!("nerve: the dashboard needs a terminal; stdout is not a tty (see `nerve --help`)");
        std::process::exit(2);
    }
    let mut guard = terminal::TerminalGuard::install()?;
    App::new(paths, shutdown).run(guard.terminal())
}

#[cfg(test)]
mod tests {
    #[test]
    fn update_reads_the_version_cargo_search_prints() {
        let out =
            "nerve-tui = \"0.6.1\"    # Watch, answer and steer…\n\nnote: to learn more, run `cargo info <name>`\n";
        assert_eq!(super::parse_search(out).as_deref(), Some("0.6.1"));
        assert_eq!(super::parse_search("nerve-tui-extra = \"9.0.0\"\n"), None);
        assert_eq!(super::parse_search(""), None);
    }
}
