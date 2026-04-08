use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::Theme;

const BINDINGS: &[(&str, &str)] = &[
    ("j/k  ↑/↓", "Navigate rows"),
    ("h/l  ←/→", "Navigate columns"),
    ("Enter / g", "Go to session tab"),
    ("p", "Preview session log"),
    ("x", "Kill session"),
    ("s", "Cycle sort: stable → state → name → age"),
    ("t", "Cycle theme"),
    ("n", "Rename session"),
    ("m", "Toggle notification mute"),
    ("1-9", "Jump to session"),
    ("?", "Toggle this help"),
    ("q", "Quit"),
];

pub fn render(frame: &mut Frame, theme: &Theme) {
    let area = super::centered(frame.area(), 52, (BINDINGS.len() as u16) + 4);

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
            Line::from(vec![
                Span::styled(
                    format!("  {:<14}", key),
                    Style::default()
                        .fg(theme.processing)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(*desc, Style::default().fg(theme.text)),
            ])
        })
        .collect();

    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
}

