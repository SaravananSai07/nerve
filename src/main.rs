mod app;
mod config;
mod detect;
mod platform;
mod state;
mod tui;

use app::App;

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--list") {
        let sessions = detect::claude::discover_sessions();
        if sessions.is_empty() {
            println!("No active sessions found.");
        }
        for s in &sessions {
            println!(
                "{} | {} | {} | {} | {} | {}",
                s.name,
                s.state.label(),
                s.tty.as_deref().unwrap_or("?"),
                s.branch.as_deref().unwrap_or("—"),
                s.format_duration(),
                s.activity.sparkline(),
            );
        }
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = App::new().run(&mut terminal);
    ratatui::restore();
    result
}
