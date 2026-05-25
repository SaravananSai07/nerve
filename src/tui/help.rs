use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::Theme;

/// One entry in the help table. Explicit variants beat the prior
/// sentinel-string convention (an empty key meaning "section
/// header") — adding a new key with no description would have
/// silently rendered as a divider.
enum HelpLine {
    Header(&'static str),
    Entry { key: &'static str, desc: &'static str },
    Note(&'static str),
}

const BINDINGS: &[HelpLine] = &[
    HelpLine::Header("navigate"),
    HelpLine::Entry { key: "j/k  ↑/↓", desc: "Row" },
    HelpLine::Entry { key: "h/l  ←/→", desc: "Column" },
    HelpLine::Entry { key: "1-9", desc: "Jump to nth session" },
    HelpLine::Header("actions"),
    HelpLine::Entry { key: "Enter", desc: "Switch to session's terminal tab" },
    HelpLine::Entry { key: "p", desc: "Preview session log" },
    HelpLine::Entry { key: "Shift+P", desc: "Preview live terminal capture" },
    HelpLine::Entry { key: "n", desc: "Rename session" },
    HelpLine::Entry { key: "x", desc: "Kill session" },
    HelpLine::Header("view"),
    HelpLine::Entry { key: "s", desc: "Cycle sort: stable → state → name → age" },
    HelpLine::Entry { key: "t", desc: "Cycle theme" },
    HelpLine::Entry { key: "/", desc: "Search (fuzzy, case-insensitive)" },
    HelpLine::Entry { key: "Esc", desc: "Clear search filter" },
    HelpLine::Entry { key: "m", desc: "Toggle notification mute" },
    HelpLine::Entry { key: "u", desc: "Dismiss update banner (until next version)" },
    HelpLine::Header("preview overlay"),
    HelpLine::Entry { key: "j/k  ↑/↓", desc: "Scroll line" },
    HelpLine::Entry { key: "PgUp/PgDn", desc: "Scroll page" },
    HelpLine::Entry { key: "g / G", desc: "Top / bottom" },
    HelpLine::Entry { key: "p / Esc / q", desc: "Close preview" },
    HelpLine::Header("app"),
    HelpLine::Entry { key: "?", desc: "Toggle this help" },
    HelpLine::Entry { key: "q", desc: "Close current overlay (or quit from main view)" },
    HelpLine::Entry { key: "Ctrl+C", desc: "Quit nerve" },
    HelpLine::Header("preferences"),
    HelpLine::Note("~/.config/nerve/prefs.toml  (mute, dismissed banner, etc.)"),
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

    // Section-header dash run scales with the inner width — on a
    // narrow help box the fixed-width ruler used to consume the
    // entire visual budget; on a wide box six dashes looked stingy.
    // Quarter of inner width capped at 12 hits both ends.
    let dash_run = ((inner.width / 4) as usize).clamp(3, 12);
    let header_rule: String = "─".repeat(dash_run);

    // If the bindings list doesn't fit, leave one row free for an
    // overflow indicator so the user knows there's more behind the
    // clip. Without this the bottom rows just vanish — including
    // `Ctrl+C` quit — and the help looks complete when it isn't.
    let visible_rows = inner.height as usize;
    let (slice_len, overflow) = if BINDINGS.len() > visible_rows {
        (visible_rows.saturating_sub(1), Some(BINDINGS.len() - (visible_rows - 1)))
    } else {
        (BINDINGS.len(), None)
    };

    let mut lines: Vec<Line> = BINDINGS
        .iter()
        .take(slice_len)
        .map(|line| match line {
            HelpLine::Header(label) => Line::from(vec![
                Span::styled(
                    format!("  {}  ", header_rule),
                    Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    *label,
                    Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
                ),
            ]),
            HelpLine::Entry { key, desc } => Line::from(vec![
                Span::styled(
                    format!("  {:<14}", key),
                    Style::default()
                        .fg(theme.processing)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(*desc, Style::default().fg(theme.text)),
            ]),
            HelpLine::Note(text) => Line::from(Span::styled(
                format!("  {text}"),
                Style::default().fg(theme.muted),
            )),
        })
        .collect();

    if let Some(remaining) = overflow {
        lines.push(Line::from(Span::styled(
            format!("  … +{remaining} more (resize for the full list)"),
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        )));
    }

    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
}
