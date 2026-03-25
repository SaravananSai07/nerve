use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::Theme;

pub fn render(frame: &mut Frame, theme: &Theme, input: &str) {
    let area = centered(frame.area(), 44, 5);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            " rename ",
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.processing));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            format!(" {input}▏"),
            Style::default().fg(theme.selected_text),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" Enter", Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
            Span::styled(" confirm  ", Style::default().fg(theme.idle)),
            Span::styled("Esc", Style::default().fg(theme.waiting).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::default().fg(theme.idle)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .split(area);
    Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}
