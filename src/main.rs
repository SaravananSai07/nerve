mod app;
mod config;
mod detect;
mod notify;
mod platform;
mod state;
mod tui;
mod updater;

use std::str::FromStr;

use app::App;
use platform::BridgeId;

fn discover_with_usage() -> Vec<state::session::Session> {
    let mut sessions = detect::claude::discover_sessions();
    for s in &mut sessions {
        if let Some(ref jp) = s.jsonl_path {
            let (usage, offset) = detect::claude::parse_token_usage(jp, 0);
            s.usage = usage;
            s.usage.last_file_offset = offset;
        }
    }
    sessions
}

fn parse_focus_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--focus" {
            return args.next();
        }
        if let Some(val) = a.strip_prefix("--focus=") {
            return Some(val.to_string());
        }
    }
    None
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
    if let Some(arg) = parse_focus_arg() {
        handle_focus(&arg);
        return Ok(());
    }

    if std::env::args().any(|a| a == "update") {
        return handle_update();
    }

    if std::env::args().any(|a| a == "--dump") {
        let sessions = discover_with_usage();
        println!("{}", serde_json::to_string_pretty(&sessions).unwrap_or_else(|e| format!("error: {e}")));
        return Ok(());
    }

    if std::env::args().any(|a| a == "--list") {
        let sessions = discover_with_usage();
        if sessions.is_empty() {
            println!("No active sessions found.");
        }
        for s in &sessions {
            let token_info = if s.usage.total_tokens() > 0 {
                format!(" | {}", s.usage.compact_display())
            } else {
                String::new()
            };
            println!(
                "{} | {} | {} | {} | {} | {}{}",
                s.name,
                s.state.label(),
                s.tty.as_deref().unwrap_or("?"),
                s.branch.as_deref().unwrap_or("—"),
                s.format_duration(),
                s.activity.sparkline(),
                token_info,
            );
        }
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = App::new().run(&mut terminal);
    ratatui::restore();
    result
}
