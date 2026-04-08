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

    let lines = vec![
        Line::from(Span::styled(
            format!(" Kill '{name}'?"),
            Style::default().fg(theme.text),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [y]", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
            Span::styled(" confirm  ", Style::default().fg(theme.idle)),
            Span::styled("[n/Esc]", Style::default().fg(theme.waiting).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::default().fg(theme.idle)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

