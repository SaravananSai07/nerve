use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::state::log_entry::LogEntry;
use crate::state::session::Session;
use super::theme::Theme;

/// What the preview overlay renders. Lives inside the
/// `Overlay::Preview` variant in `app.rs` so closing the overlay
/// drops the captured buffer with it. The discriminant also
/// doubles as the "LIVE vs LOG" header label.
pub(crate) enum PreviewSource {
    /// Captured terminal scrollback from Ghostty's screen capture.
    TerminalCapture(Vec<String>),
    /// Parsed JSONL entries from the session transcript.
    LogEntries(Vec<LogEntry>),
}

impl PreviewSource {
    fn label(&self) -> &'static str {
        match self {
            Self::TerminalCapture(_) => "LIVE",
            Self::LogEntries(_) => "LOG",
        }
    }

    /// Total scrollable lines. Used to clamp the scroll cursor at the
    /// handler so holding `j` past the bottom doesn't require an equal
    /// number of `k` presses to recover.
    pub(crate) fn line_count(&self) -> usize {
        match self {
            Self::TerminalCapture(lines) => lines.len(),
            Self::LogEntries(entries) => entries.len(),
        }
    }
}

pub(crate) fn render(
    frame: &mut Frame,
    theme: &Theme,
    session: &Session,
    source: &PreviewSource,
    scroll: &mut usize,
) {
    let area = frame.area();
    let width = (area.width as f32 * 0.70).min(120.0) as u16;
    let height = (area.height as f32 * 0.60).min(40.0) as u16;
    let area = super::centered(area, width.max(40), height.max(10));

    frame.render_widget(Clear, area);

    let indicator = theme.state_indicator(session.state());
    let title = format!(
        " {} {} {} | {} | {} ",
        session.name(),
        indicator,
        session.state().label(),
        session.format_duration(),
        source.label(),
    );

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.state_color(session.state())));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match source {
        PreviewSource::TerminalCapture(lines) => {
            render_terminal_buffer(frame, inner, theme, lines, scroll);
        }
        PreviewSource::LogEntries(entries) => {
            render_log_entries(frame, inner, theme, entries, scroll);
        }
    }
}

fn render_terminal_buffer(
    frame: &mut Frame,
    inner: Rect,
    theme: &Theme,
    terminal_lines: &[String],
    scroll: &mut usize,
) {
    if terminal_lines.is_empty() {
        // Reset scroll so an empty buffer doesn't leave the cursor
        // pinned at usize::MAX (the initial value) when the user
        // later opens a populated buffer in the same overlay.
        *scroll = 0;
        let msg = Paragraph::new(Line::styled(
            "  No terminal content captured.",
            Style::default().fg(theme.muted),
        ));
        frame.render_widget(msg, inner);
        return;
    }

    let visible_height = inner.height as usize;
    let max_scroll = terminal_lines.len().saturating_sub(visible_height);
    if *scroll > max_scroll {
        *scroll = max_scroll;
    }

    let visible: Vec<Line> = terminal_lines
        .iter()
        .skip(*scroll)
        .take(visible_height.saturating_sub(1))
        .map(|l| Line::styled(l.as_str(), Style::default().fg(theme.text)))
        .collect();

    let mut all_lines = visible;
    all_lines.push(hint_bar(theme));

    let para = Paragraph::new(all_lines);
    frame.render_widget(para, inner);
}

fn render_log_entries(
    frame: &mut Frame,
    inner: Rect,
    theme: &Theme,
    entries: &[LogEntry],
    scroll: &mut usize,
) {
    if entries.is_empty() {
        *scroll = 0;
        let msg = Paragraph::new(Line::styled(
            "  No log entries found.",
            Style::default().fg(theme.muted),
        ));
        frame.render_widget(msg, inner);
        return;
    }

    let visible_height = inner.height as usize;
    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| format_entry(i, entry, theme))
        .collect();

    let max_scroll = lines.len().saturating_sub(visible_height);
    if *scroll > max_scroll {
        *scroll = max_scroll;
    }

    let visible: Vec<Line> = lines
        .into_iter()
        .skip(*scroll)
        .take(visible_height.saturating_sub(1))
        .collect();

    let mut all_lines = visible;
    all_lines.push(hint_bar(theme));

    let para = Paragraph::new(all_lines);
    frame.render_widget(para, inner);
}

fn hint_bar<'a>(theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(" [p/q/Esc]", Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
        Span::styled(" close  ", Style::default().fg(theme.muted)),
        Span::styled("[j/k]", Style::default().fg(theme.waiting).add_modifier(Modifier::BOLD)),
        Span::styled(" line  ", Style::default().fg(theme.muted)),
        Span::styled("[PgUp/PgDn]", Style::default().fg(theme.waiting).add_modifier(Modifier::BOLD)),
        Span::styled(" page  ", Style::default().fg(theme.muted)),
        Span::styled("[g/G]", Style::default().fg(theme.waiting).add_modifier(Modifier::BOLD)),
        Span::styled(" top/bottom", Style::default().fg(theme.muted)),
    ])
}

fn format_entry<'a>(idx: usize, entry: &LogEntry, theme: &Theme) -> Line<'a> {
    let num = format!("{:>3} ", idx + 1);
    match entry {
        LogEntry::UserText(text) => Line::from(vec![
            Span::styled(num, Style::default().fg(theme.muted)),
            Span::styled("▸ ", Style::default().fg(theme.waiting).add_modifier(Modifier::BOLD)),
            Span::styled(text.clone(), Style::default().fg(theme.waiting)),
        ]),
        LogEntry::AssistantText(text) => Line::from(vec![
            Span::styled(num, Style::default().fg(theme.muted)),
            Span::styled("  ", Style::default().fg(theme.text)),
            Span::styled(text.clone(), Style::default().fg(theme.text)),
        ]),
        LogEntry::ToolUse { name, detail } => {
            let mut spans = vec![
                Span::styled(num, Style::default().fg(theme.muted)),
                Span::styled("◉ ", Style::default().fg(theme.processing)),
                Span::styled(name.clone(), Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
            ];
            if !detail.is_empty() {
                spans.push(Span::styled(format!(" {detail}"), Style::default().fg(theme.muted)));
            }
            Line::from(spans)
        }
        LogEntry::ToolResult { status, snippet } => {
            let (icon, color) = if status == "error" {
                ("✗ ", theme.error)
            } else {
                ("✓ ", theme.processing)
            };
            let mut spans = vec![
                Span::styled(num, Style::default().fg(theme.muted)),
                Span::styled(icon, Style::default().fg(color)),
            ];
            if snippet.is_empty() {
                spans.push(Span::styled(status.clone(), Style::default().fg(color)));
            } else {
                spans.push(Span::styled(snippet.clone(), Style::default().fg(theme.muted)));
            }
            Line::from(spans)
        }
        LogEntry::Result { is_error } => {
            let (icon, label, color) = if *is_error {
                ("✕ ", "error result", theme.error)
            } else {
                ("● ", "result", theme.waiting)
            };
            Line::from(vec![
                Span::styled(num, Style::default().fg(theme.muted)),
                Span::styled(icon, Style::default().fg(color)),
                Span::styled(label, Style::default().fg(color)),
            ])
        }
    }
}

