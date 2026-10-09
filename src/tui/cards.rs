use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::state::registry::SessionRegistry;
use crate::state::session::Session;
use crate::tui::status::StatusMessage;
use crate::tui::theme::Theme;

/// Bundle of everything the render path reads from `App`. Threading
/// each field through `render`/`render_status_bar` separately tripped
/// `clippy::too_many_arguments` and made adding a new field a search-
/// and-replace across two signatures. One struct, one threading.
pub(crate) struct RenderContext<'a> {
    pub(crate) registry: &'a SessionRegistry,
    pub(crate) sessions: &'a [&'a Session],
    pub(crate) selected: usize,
    pub(crate) theme: &'a Theme,
    pub(crate) status_message: Option<&'a StatusMessage>,
    pub(crate) notifications_muted: bool,
    pub(crate) update_banner: Option<&'a str>,
    pub(crate) search_query: Option<&'a str>,
    pub(crate) claude_installed: bool,
}

pub(crate) fn render(frame: &mut Frame, area: Rect, ctx: RenderContext<'_>) {
    if ctx.sessions.is_empty() {
        if let Some(q) = ctx.search_query.filter(|q| !q.is_empty()) {
            render_search_empty(frame, area, ctx.theme, q);
        } else if !ctx.claude_installed {
            render_setup_hint(frame, area, ctx.theme);
        } else {
            render_empty(frame, area, ctx.theme);
        }
        return;
    }

    let outer = Block::default()
        .title(Span::styled(
            " nerve ",
            Style::default().fg(ctx.theme.text).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ctx.theme.border));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let mut constraints: Vec<Constraint> = Vec::with_capacity(3);
    if ctx.update_banner.is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(3));
    constraints.push(Constraint::Length(status_rows(&ctx, inner.width)));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let (card_area, status_area) = if let Some(version) = ctx.update_banner {
        render_update_banner(frame, chunks[0], ctx.theme, version);
        (chunks[1], chunks[2])
    } else {
        (chunks[0], chunks[1])
    };

    // One `Instant::now()` per frame so every card's duration
    // display agrees on a single timestamp.
    let now = Instant::now();
    render_cards(frame, card_area, ctx.sessions, ctx.selected, ctx.theme, now);
    render_status_bar(frame, status_area, &ctx);
}

/// Most rows a status message may take; beyond that it's clipped.
const STATUS_MAX_ROWS: u16 = 3;

/// Rows the status bar needs. A message (e.g. a CLI error) wraps rather
/// than being cut off at the terminal edge, borrowing up to
/// `STATUS_MAX_ROWS` from the card area; the summary line is always one.
fn status_rows(ctx: &RenderContext<'_>, width: u16) -> u16 {
    ctx.status_message.map_or(1, |msg| message_rows(&msg.text, width))
}

/// Word wrap moves a word that straddles the edge to the next line, so
/// count with a word wrap, not `width / cols`. One column goes to the
/// leading space the renderer adds; erring a row high only borrows a row.
fn message_rows(text: &str, width: u16) -> u16 {
    let cols = (width as usize).saturating_sub(1).max(1);
    let needed = crate::util::text::wrap_words(text, cols).len();
    (needed.min(STATUS_MAX_ROWS as usize) as u16).max(1)
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
    // 1-col below 80 (single-card mobile-ish), 2-col 80–159 (default
    // laptop terminal), 3-col 160+ (ultra-wide). Smoother than the
    // prior 1↔2 cliff and uses the screen on big displays.
    let cols = match width {
        0..80 => 1,
        80..160 => 2,
        _ => 3,
    };
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
            theme.muted,
        )
    };

    let duration = session.format_duration_at(now);
    let label = session.state().label();
    // Claude's session names are sentences; clip the name so the left
    // title never runs into (and visually truncates) the right-hand
    // duration. Fixed parts: 2 borders, spaces, indicator, label, duration.
    let fixed = 2 + 4 + 1 + label.width() + duration.width() + 2;
    let name_room = (area.width as usize).saturating_sub(fixed).max(4);
    let name = crate::util::text::truncate_width(session.name().as_str(), name_room);
    let title_left = Span::styled(
        format!(" {} {} {} ", name, indicator, label),
        if is_selected {
            Style::default().fg(text_fg).bg(card_bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(state_color)
        },
    );

    let title_right = Span::styled(
        format!(" {duration} "),
        if is_selected {
            Style::default().fg(state_color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted)
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

    let tty_str = session.kind.location(session.tty.as_ref());
    let branch_str = session.branch.as_ref().map(|s| s.as_str()).unwrap_or("—");

    let mut where_spans = vec![Span::styled(tty_str, Style::default().fg(secondary_fg))];
    if let Some(project) = session.project() {
        where_spans.push(Span::styled(" · ", Style::default().fg(border_color)));
        // Capped so a long repo name doesn't push the branch off the card.
        let project = crate::util::text::truncate_width(project.as_str(), 24);
        where_spans.push(Span::styled(project, Style::default().fg(text_fg)));
    }
    where_spans.push(Span::styled("  ⎇ ", Style::default().fg(border_color)));
    where_spans.push(Span::styled(branch_str, Style::default().fg(text_fg)));
    let mut lines = vec![Line::from(where_spans)];

    // While waiting, Claude's reason ("permission prompt", a background
    // job's open questions) is what the user needs; otherwise the last tool.
    if let Some(ref why) = session.waiting_for {
        lines.push(Line::from(vec![
            Span::styled("  ? ", Style::default().fg(theme.waiting)),
            Span::styled(why.as_str(), Style::default().fg(text_fg)),
        ]));
    } else if let Some(ref tool) = session.current_tool {
        lines.push(Line::from(vec![
            Span::styled("  ◉ ", Style::default().fg(theme.processing)),
            Span::styled(tool.as_str(), Style::default().fg(text_fg)),
        ]));
    }

    let mut sparkline_spans = vec![
        Span::styled(session.activity_sparkline(), Style::default().fg(state_color)),
        Span::styled(
            format!("  {:.0}% cpu", session.cpu_percent),
            Style::default().fg(secondary_fg),
        ),
    ];
    let official = session.official.as_ref();
    if let Some(pct) = official.and_then(|o| o.context_pct) {
        // Context fill is the number that predicts trouble (compaction,
        // degraded answers), so it turns to the warning colour near full.
        let color = if pct >= 80.0 { theme.waiting } else { secondary_fg };
        sparkline_spans.push(Span::styled(format!("  ctx {pct:.0}%"), Style::default().fg(color)));
    }
    match official.and_then(|o| o.cost_usd) {
        Some(cost) => sparkline_spans.push(Span::styled(
            format!("  ${cost:.2}"),
            Style::default().fg(secondary_fg),
        )),
        None if session.usage.total_tokens() > 0 => sparkline_spans.push(Span::styled(
            format!("  {}", session.usage.compact_display()),
            Style::default().fg(secondary_fg),
        )),
        None => {}
    }
    lines.push(Line::from(sparkline_spans));

    let para = Paragraph::new(lines).style(Style::default().bg(card_bg));
    frame.render_widget(para, inner);
}

fn render_status_bar(frame: &mut Frame, area: Rect, ctx: &RenderContext<'_>) {
    let theme = ctx.theme;
    let registry = ctx.registry;
    let search_query = ctx.search_query;
    let filtered_count = ctx.sessions.len();
    let notifications_muted = ctx.notifications_muted;

    // Action / search messages take the whole row. Colour by kind so
    // a successful kill reads green, a failed kill reads red, and a
    // navigational note reads in the neutral text colour. The prior
    // design rendered every message in `theme.error`, which made
    // "returned from 'foo'" look like a failure.
    if let Some(msg) = ctx.status_message {
        let line = Line::from(Span::styled(
            format!(" {}", msg.text),
            Style::default().fg(msg.color(theme)),
        ));
        frame.render_widget(
            Paragraph::new(line).wrap(ratatui::widgets::Wrap { trim: false }),
            area,
        );
        return;
    }

    let total = registry.len();
    let total_cost = registry.total_cost_usd();
    let counts = registry.count_by_state();
    let searching = search_query.is_some();
    let sep: &str = if searching { "  " } else { "   " };

    // Left half — session summary. Always present.
    let mut left: Vec<Span> = Vec::new();
    if let Some(q) = search_query {
        left.push(Span::styled(
            format!(" /{q}  {filtered_count}/{total} "),
            Style::default().fg(theme.processing).add_modifier(Modifier::BOLD),
        ));
    } else {
        left.push(Span::styled(format!(" {total} sessions"), Style::default().fg(theme.text)));
    }
    left.push(Span::raw(sep));
    left.push(Span::styled(format!("{} active", counts.active), Style::default().fg(theme.processing)));
    left.push(Span::raw(sep));
    left.push(Span::styled(format!("{} waiting", counts.waiting), Style::default().fg(theme.waiting)));
    left.push(Span::raw(sep));
    left.push(Span::styled(format!("{} idle", counts.idle), Style::default().fg(theme.idle)));
    if total_cost >= 0.01 {
        left.push(Span::raw(sep));
        let cost = if searching {
            format!("${total_cost:.2}")
        } else {
            format!("${total_cost:.2} total")
        };
        left.push(Span::styled(cost, Style::default().fg(theme.text)));
    }

    // Right half — chrome hints + [muted]. Right-aligned so the
    // muted indicator survives a narrow terminal. On a tmux-split
    // 30–80 col terminal we drop chrome hints entirely (they'd
    // silently truncate anyway) and only show the muted indicator
    // when it applies; on a mid-width terminal we drop the less
    // critical hints ([t]heme, [/] search) to keep the row honest.
    let width = area.width as usize;
    let mut right: Vec<Span> = Vec::new();
    if searching {
        // Esc-to-clear-search is routine navigation, not a failure.
        // Render in `muted` to match the search overlay's own hint
        // and avoid training the user that red means nothing.
        right.push(Span::styled("[Esc] clear", Style::default().fg(theme.muted)));
    } else if width >= 60 {
        let sort_label = registry.sort_mode().label();
        right.extend([
            Span::styled(format!("[s]ort: {sort_label}"), Style::default().fg(theme.muted)),
            Span::raw("  "),
            Span::styled("[?] help", Style::default().fg(theme.muted)),
        ]);
        if width >= 100 {
            right.extend([
                Span::raw("  "),
                Span::styled(format!("[t]heme: {}", theme.name), Style::default().fg(theme.muted)),
                Span::raw("  "),
                Span::styled("[/] search", Style::default().fg(theme.muted)),
            ]);
        }
    }
    if notifications_muted {
        if !right.is_empty() {
            right.push(Span::raw("  "));
        }
        right.push(Span::styled("[muted] ", Style::default().fg(theme.error)));
    }

    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Min(0)])
        .split(area);
    frame.render_widget(Paragraph::new(Line::from(left)), halves[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right).right_aligned()),
        halves[1],
    );
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
            Style::default().fg(theme.muted),
        ),
    ])
    .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(text, inner);
}

/// Shown when no Claude config dir exists at all (first-time
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
            "Claude Code isn't installed (no ~/.claude or $CLAUDE_CONFIG_DIR).",
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
            Style::default().fg(theme.muted),
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
            Style::default().fg(theme.muted),
        ),
    ])
    .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(text, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::registry::SessionRegistry;
    use crate::state::session::{DiscoverySnapshot, Session, SessionState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // Regression: long action errors were clipped at the terminal edge
    // ("couldn't confirm 78240abc was stopped — th"). They now wrap.
    #[test]
    fn long_status_message_wraps_instead_of_clipping() {
        let mut registry = SessionRegistry::new();
        registry.upsert(Session::from_snapshot(DiscoverySnapshot::for_test("s1", SessionState::Idle)));
        let theme = crate::tui::theme::Theme::catalog(None).remove(0);
        let text = "'Analyze repository overview and state': couldn't confirm 78240abc was \
                    stopped — the background service didn't answer in time; try again";
        let msg = StatusMessage::error(text);
        let sessions: Vec<&Session> = registry.sorted_sessions();
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| {
            render(
                f,
                f.area(),
                RenderContext {
                    registry: &registry,
                    sessions: &sessions,
                    selected: 0,
                    theme: &theme,
                    status_message: Some(&msg),
                    notifications_muted: false,
                    update_banner: None,
                    search_query: None,
                    claude_installed: true,
                },
            )
        })
        .unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join(" ");
        let squashed = screen.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(squashed.contains("try again"), "message was clipped:\n{screen}");
    }

    // A word straddling the edge wraps whole, so width / cols undercounts.
    #[test]
    fn message_rows_count_word_wrapped_lines() {
        let text = format!("{} bbbb {}", "a".repeat(37), "c".repeat(36));
        assert_eq!(message_rows(&text, 40), 3);
        assert_eq!(message_rows("short", 40), 1);
        assert_eq!(message_rows(&"x ".repeat(200), 40), STATUS_MAX_ROWS);
    }
}
