use std::time::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::state::registry::SessionRegistry;
use crate::state::session::Session;
use crate::tui::theme::Theme;

#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    registry: &SessionRegistry,
    sessions: &[&Session],
    selected: usize,
    theme: &Theme,
    status_message: Option<&str>,
    notifications_muted: bool,
    update_banner: Option<&str>,
    search_query: Option<&str>,
    claude_installed: bool,
) {
    if sessions.is_empty() {
        if let Some(q) = search_query.filter(|q| !q.is_empty()) {
            render_search_empty(frame, area, theme, q);
        } else if !claude_installed {
            render_setup_hint(frame, area, theme);
        } else {
            render_empty(frame, area, theme);
        }
        return;
    }

    let outer = Block::default()
        .title(Span::styled(" nerve ", Style::default().fg(theme.text).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let mut constraints: Vec<Constraint> = Vec::with_capacity(3);
    if update_banner.is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(3));
    constraints.push(Constraint::Length(1));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let (card_area, status_area) = if let Some(version) = update_banner {
        render_update_banner(frame, chunks[0], theme, version);
        (chunks[1], chunks[2])
    } else {
        (chunks[0], chunks[1])
    };

    // One `Instant::now()` per frame so every card's duration
    // display agrees on a single timestamp.
    let now = Instant::now();
    render_cards(frame, card_area, sessions, selected, theme, now);
    render_status_bar(
        frame,
        status_area,
        registry,
        sessions.len(),
        theme,
        status_message,
        notifications_muted,
        search_query,
    );
}

fn render_update_banner(frame: &mut Frame, area: Rect, theme: &Theme, version: &str) {
    let line = Line::from(vec![
        Span::styled(
            "  ↑ Update available: ",
            Style::default().fg(theme.text).add_modifier(Modifier::DIM),
        ),
        Span::styled(
            version,
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  — run ",
            Style::default().fg(theme.text).add_modifier(Modifier::DIM),
        ),
        Span::styled(
            "nerve update",
            Style::default().fg(theme.text),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_cards(
    frame: &mut Frame,
    area: Rect,
    sessions: &[&Session],
    selected: usize,
    theme: &Theme,
    now: Instant,
) {
    let width = area.width as usize;
    let cols = if width >= 80 { 2 } else { 1 };
    let total_rows = sessions.len().div_ceil(cols);

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
                render_card(frame, col_chunks[col], sessions[i], i == selected, theme, now);
            }
        }
    }
}

fn render_card(
    frame: &mut Frame,
    area: Rect,
    session: &Session,
    is_selected: bool,
    theme: &Theme,
    now: Instant,
) {
    let state_color = theme.state_color(session.state());
    let indicator = theme.state_indicator(session.state());

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
        format!(" {} {} {} ", session.name(), indicator, session.state().label()),
        if is_selected {
            Style::default().fg(text_fg).bg(card_bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(state_color)
        },
    );

    let title_right = Span::styled(
        format!(" {} ", session.format_duration_at(now)),
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

    if is_selected {
        // Fill inner area with bg — avoids bleeding behind rounded corners
        for y in inner.y..(inner.y + inner.height) {
            for x in inner.x..(inner.x + inner.width) {
                if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                    cell.set_bg(card_bg);
                }
            }
        }
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
        Span::styled(session.activity_sparkline(), Style::default().fg(state_color)),
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

#[allow(clippy::too_many_arguments)]
fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    registry: &SessionRegistry,
    filtered_count: usize,
    theme: &Theme,
    status_message: Option<&str>,
    notifications_muted: bool,
    search_query: Option<&str>,
) {
    let line = if let Some(msg) = status_message {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(theme.error),
        ))
    } else {
        let total = registry.len();
        let total_cost = registry.total_cost_usd();
        let counts = registry.count_by_state();
        let searching = search_query.is_some();
        let sep: &str = if searching { "  " } else { "   " };

        let mut spans: Vec<Span> = Vec::new();
        if let Some(q) = search_query {
            spans.push(Span::styled(
                format!(" /{q}  {filtered_count}/{total} "),
                Style::default().fg(theme.processing).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(format!(" {total} sessions"), Style::default().fg(theme.text)));
        }
        spans.push(Span::raw(sep));
        spans.push(Span::styled(format!("{} active", counts.active), Style::default().fg(theme.processing)));
        spans.push(Span::raw(sep));
        spans.push(Span::styled(format!("{} waiting", counts.waiting), Style::default().fg(theme.waiting)));
        spans.push(Span::raw(sep));
        spans.push(Span::styled(format!("{} idle", counts.idle), Style::default().fg(theme.idle)));
        if total_cost >= 0.01 {
            spans.push(Span::raw(sep));
            let cost = if searching {
                format!("${total_cost:.2}")
            } else {
                format!("${total_cost:.2} total")
            };
            spans.push(Span::styled(cost, Style::default().fg(theme.text)));
        }
        if searching {
            spans.push(Span::raw("  "));
            spans.push(Span::styled("[Esc] clear", Style::default().fg(theme.error)));
        } else {
            let sort_label = registry.sort_mode().label();
            spans.extend([
                Span::raw("   "),
                Span::styled(format!("[s]ort: {sort_label}"), Style::default().fg(theme.idle)),
                Span::raw("  "),
                Span::styled(format!("[t]heme: {}", theme.name), Style::default().fg(theme.idle)),
                Span::raw("  "),
                Span::styled("[?] help", Style::default().fg(theme.idle)),
                Span::raw("  "),
                Span::styled("[/] search", Style::default().fg(theme.idle)),
            ]);
        }
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

/// Shown when `~/.claude/` is missing entirely (first-time
/// setup). Distinct from `render_empty` so the user gets a clear
/// "install Claude Code" message rather than the confusing "no
/// sessions detected" when the cause is that Claude Code itself
/// hasn't been set up.
fn render_setup_hint(frame: &mut Frame, area: Rect, theme: &Theme) {
    let block = Block::default()
        .title(Span::styled(
            " nerve ",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.waiting));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = Paragraph::new(vec![
        Line::raw(""),
        Line::styled(
            "Claude Code isn't installed (no ~/.claude directory).",
            Style::default().fg(theme.waiting),
        ),
        Line::raw(""),
        Line::styled(
            "Install it from https://docs.claude.com/claude-code",
            Style::default().fg(theme.text),
        ),
        Line::raw(""),
        Line::styled(
            "Then start a session in another tab — nerve will pick it up.",
            Style::default().fg(theme.idle),
        ),
    ])
    .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(text, inner);
}

fn render_search_empty(frame: &mut Frame, area: Rect, theme: &Theme, query: &str) {
    let block = Block::default()
        .title(Span::styled(" nerve ", Style::default().fg(theme.text).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = Paragraph::new(vec![
        Line::raw(""),
        Line::styled(
            format!("No sessions match \"/{query}\"."),
            Style::default().fg(theme.waiting),
        ),
        Line::raw(""),
        Line::styled(
            "Press Esc to clear the search.",
            Style::default().fg(theme.idle),
        ),
    ])
    .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(text, inner);
}
