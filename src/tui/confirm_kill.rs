use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::Theme;

pub fn render(frame: &mut Frame, theme: &Theme, name: &str) {
    let area = super::centered(frame.area(), 48, 5);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            " kill session ",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.error));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Truncate long names so the prompt fits the modal's inner width
    // (48 cols minus border + leading "Kill '" + trailing "'?").
    let display_name = crate::util::text::truncate_chars(name, 35);
    let lines = vec![
        Line::from(Span::styled(
            format!(" Kill '{display_name}'?"),
            Style::default().fg(theme.text),
        )),
        Line::raw(""),
        // Default-on-cancel: Enter / Esc / n / q all dismiss the dialog
        // safely. `y` is the only key that actually fires SIGTERM, so
        // a "punch-through-with-Enter" reflex resolves to no-op.
        Line::from(vec![
            Span::styled(" [y]", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
            Span::styled(" kill  ", Style::default().fg(theme.muted)),
            Span::styled(
                "[Enter/Esc/n]",
                Style::default().fg(theme.processing).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel (default)", Style::default().fg(theme.muted)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}


