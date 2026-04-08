mod app;
mod config;
mod detect;
mod notify;
mod platform;
mod state;
mod tui;

use app::App;

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

fn main() -> std::io::Result<()> {
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
