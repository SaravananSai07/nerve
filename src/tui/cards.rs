use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::state::registry::SessionRegistry;
use crate::state::session::Session;
use crate::tui::theme::Theme;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    registry: &SessionRegistry,
    selected: usize,
    theme: &Theme,
    status_message: Option<&str>,
    notifications_muted: bool,
) {
    let sessions = registry.sorted_sessions();

    if sessions.is_empty() {
        render_empty(frame, area, theme);
        return;
    }

    let outer = Block::default()
        .title(Span::styled(" nerve ", Style::default().fg(theme.text).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let (card_area, status_area) = {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner);
        (chunks[0], chunks[1])
    };

    render_cards(frame, card_area, &sessions, selected, theme);
    render_status_bar(frame, status_area, registry, theme, status_message, notifications_muted);
}

fn render_cards(frame: &mut Frame, area: Rect, sessions: &[&Session], selected: usize, theme: &Theme) {
    let width = area.width as usize;
    let cols = if width >= 80 { 2 } else { 1 };
    let total_rows = (sessions.len() + cols - 1) / cols;

    const CARD_HEIGHT: u16 = 5;
    let visible_rows = (area.height / CARD_HEIGHT) as usize;

    let selected_row = selected / cols;
    let scroll_offset = if total_rows <= visible_rows {
        0
    } else {
        selected_row.min(total_rows - visible_rows)
    };

    let render_rows = visible_rows.min(total_rows - scroll_offset);

    let row_constraints: Vec<Constraint> = (0..render_rows)
        .map(|_| Constraint::Length(CARD_HEIGHT))
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();

    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    let col_constraints: Vec<Constraint> = (0..cols)
        .map(|_| Constraint::Percentage((100 / cols) as u16))
        .collect();

    for visible_row in 0..render_rows {
        let actual_row = visible_row + scroll_offset;
        for col in 0..cols {
            let i = actual_row * cols + col;
            if i >= sessions.len() {
                break;
            }

            let col_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(&col_constraints)
                .split(row_chunks[visible_row]);

            if col < col_chunks.len() {
                render_card(frame, col_chunks[col], &sessions[i], i == selected, theme);
            }
        }
    }
}

fn render_card(frame: &mut Frame, area: Rect, session: &Session, is_selected: bool, theme: &Theme) {
    let state_color = theme.state_color(&session.state);
    let indicator = theme.state_indicator(&session.state);

    let (card_bg, border_color, border_style, text_fg, secondary_fg) = if is_selected {
        (
            theme.selected_bg,
            state_color,
            Style::default().fg(state_color).add_modifier(Modifier::BOLD),
            theme.selected_text,
            theme.text,
        )
    } else {
        (
            Color::Reset,
            theme.border,
            Style::default().fg(theme.border),
            theme.text,
            theme.idle,
        )
    };

    let title_left = Span::styled(
        format!(" {} {} {} ", session.name, indicator, session.state.label()),
        if is_selected {
            Style::default().fg(text_fg).bg(card_bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(state_color)
        },
    );

    let title_right = Span::styled(
        format!(" {} ", session.format_duration()),
        if is_selected {
            Style::default().fg(state_color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.idle)
        },
    );

    let block = Block::default()
        .title_top(title_left)
        .title_top(Line::from(title_right).right_aligned())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Fill inner area with bg — avoids bleeding behind rounded corners
    if is_selected {
        for y in inner.y..(inner.y + inner.height) {
            for x in inner.x..(inner.x + inner.width) {
                if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                    cell.set_bg(card_bg);
                }
            }
        }
        // Paint left accent stripe over the border
        if area.height >= 3 {
            for y in (area.y + 1)..(area.y + area.height.saturating_sub(1)) {
                if let Some(cell) = frame.buffer_mut().cell_mut((area.x, y)) {
                    cell.set_char('▌');
                    cell.set_fg(state_color);
                }
            }
        }
    }

    let tty_str = session.tty.as_deref().unwrap_or("?");
    let branch_str = session.branch.as_deref().unwrap_or("—");

    let mut lines = vec![
        Line::from(vec![
            Span::styled(tty_str, Style::default().fg(secondary_fg)),
            Span::styled("  ⎇ ", Style::default().fg(border_color)),
            Span::styled(branch_str, Style::default().fg(text_fg)),
        ]),
    ];

    if let Some(ref tool) = session.current_tool {
        lines.push(Line::from(vec![
            Span::styled("  ◉ ", Style::default().fg(theme.processing)),
            Span::styled(tool.to_string(), Style::default().fg(text_fg)),
        ]));
    }

    let mut sparkline_spans = vec![
        Span::styled(session.activity.sparkline(), Style::default().fg(state_color)),
        Span::styled(
            format!("  {:.0}% cpu", session.cpu_percent),
            Style::default().fg(secondary_fg),
        ),
    ];
    if session.usage.total_tokens() > 0 {
        sparkline_spans.push(Span::styled(
            format!("  {}", session.usage.compact_display()),
            Style::default().fg(secondary_fg),
        ));
    }
    lines.push(Line::from(sparkline_spans));

    let para = Paragraph::new(lines).style(Style::default().bg(card_bg));
    frame.render_widget(para, inner);
}

fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    registry: &SessionRegistry,
    theme: &Theme,
    status_message: Option<&str>,
    notifications_muted: bool,
) {
    let line = if let Some(msg) = status_message {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(theme.error),
        ))
    } else {
        let counts = registry.count_by_state();
        let total = registry.len();
        let sort_label = registry.sort_mode().label();
        let total_cost: f64 = registry
            .sorted_sessions()
            .iter()
            .map(|s| s.usage.cost_usd)
            .sum();

        let mut spans = vec![
            Span::styled(format!(" {total} sessions"), Style::default().fg(theme.text)),
            Span::raw("   "),
            Span::styled(format!("{} active", counts.active), Style::default().fg(theme.processing)),
            Span::raw("   "),
            Span::styled(format!("{} waiting", counts.waiting), Style::default().fg(theme.waiting)),
            Span::raw("   "),
            Span::styled(format!("{} idle", counts.idle), Style::default().fg(theme.idle)),
        ];
        if total_cost >= 0.01 {
            spans.push(Span::raw("   "));
            spans.push(Span::styled(
                format!("${:.2} total", total_cost),
                Style::default().fg(theme.text),
            ));
        }
        spans.extend([
            Span::raw("   "),
            Span::styled(format!("[s]ort: {sort_label}"), Style::default().fg(theme.idle)),
            Span::raw("  "),
            Span::styled(format!("[t]heme: {}", theme.name), Style::default().fg(theme.idle)),
            Span::raw("  "),
            Span::styled("[?] help", Style::default().fg(theme.idle)),
        ]);
        if notifications_muted {
            spans.push(Span::raw("  "));
            spans.push(Span::styled("[muted]", Style::default().fg(theme.error)));
        }
        Line::from(spans)
    };

    let para = Paragraph::new(line);
    frame.render_widget(para, area);
}

fn render_empty(frame: &mut Frame, area: Rect, theme: &Theme) {
    let block = Block::default()
        .title(Span::styled(" nerve ", Style::default().fg(theme.text).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = Paragraph::new(vec![
        Line::raw(""),
        Line::styled(
            "No AI sessions detected.",
            Style::default().fg(theme.waiting),
        ),
        Line::raw(""),
        Line::styled(
            "Start a Claude Code session in another tab.",
            Style::default().fg(theme.idle),
        ),
    ])
    .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(text, inner);
}
