use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::Theme;

pub fn render(frame: &mut Frame, theme: &Theme) {
    let area = super::centered(frame.area(), 52, 7);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            " preview ",
            Style::default()
                .fg(theme.processing)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.processing));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            " Briefly switches tabs to capture the terminal",
            Style::default().fg(theme.text),
        )),
        Line::from(Span::styled(
            " buffer.",
            Style::default().fg(theme.text),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [y]", Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
            Span::styled(" continue  ", Style::default().fg(theme.idle)),
            Span::styled("[d]", Style::default().fg(theme.waiting).add_modifier(Modifier::BOLD)),
            Span::styled(" don't ask again  ", Style::default().fg(theme.idle)),
            Span::styled("[n/Esc]", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::default().fg(theme.idle)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

