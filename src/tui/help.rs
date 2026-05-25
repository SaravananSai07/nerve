use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::Theme;

/// Two-column layout: header rows have an empty `key` to act as
/// section dividers. The list is in display order — what the user
/// will actually try first goes near the top.
const BINDINGS: &[(&str, &str)] = &[
    ("", "─ navigate ─"),
    ("j/k  ↑/↓", "Row"),
    ("h/l  ←/→", "Column"),
    ("1-9", "Jump to nth session"),
    ("", "─ actions ─"),
    ("Enter", "Switch to session's terminal tab"),
    ("p", "Preview session log"),
    ("Shift+P", "Preview live terminal capture"),
    ("n", "Rename session"),
    ("x", "Kill session"),
    ("", "─ view ─"),
    ("s", "Cycle sort: stable → state → name → age"),
    ("t", "Cycle theme"),
    ("/", "Search (fuzzy, case-insensitive)"),
    ("Esc", "Clear search filter"),
    ("m", "Toggle notification mute"),
    ("", "─ preview overlay ─"),
    ("j/k  ↑/↓", "Scroll line"),
    ("PgUp/PgDn", "Scroll page"),
    ("g / G", "Top / bottom"),
    ("p / Esc / q", "Close preview"),
    ("", "─ app ─"),
    ("?", "Toggle this help"),
    ("q", "Close current overlay (or quit from main view)"),
    ("Ctrl+C", "Quit nerve"),
];

pub fn render(frame: &mut Frame, theme: &Theme) {
    let frame_area = frame.area();
    // Cap height to the available frame so the overlay clips
    // gracefully on small terminals rather than drawing partially
    // off-screen.
    let want_height = (BINDINGS.len() as u16) + 2;
    let height = want_height.min(frame_area.height.saturating_sub(2));
    let width = 56u16.min(frame_area.width.saturating_sub(2));
    let area = super::centered(frame_area, width, height);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            " keybindings ",
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.processing));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = BINDINGS
        .iter()
        .map(|(key, desc)| {
            if key.is_empty() {
                // Section header — dim, no leading column, full-width.
                Line::from(Span::styled(
                    format!(" {desc}"),
                    Style::default().fg(theme.idle).add_modifier(Modifier::DIM),
                ))
            } else {
                Line::from(vec![
                    Span::styled(
                        format!("  {:<14}", key),
                        Style::default()
                            .fg(theme.processing)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(*desc, Style::default().fg(theme.text)),
                ])
            }
        })
        .collect();

    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
}

