//! Lines of positioned text, painted straight into the frame buffer. Each
//! screen builds its lines, `window` keeps the cursor's lines in view, and
//! `paint` draws them. Strings are clipped to the cell grid by display
//! width, so wide or combining characters never push a column over.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::model::RowKey;
use crate::util::text::truncate_width;

#[derive(Default)]
pub(crate) struct Ln {
    parts: Vec<(u16, String, Style)>,
    pub(crate) bg: Option<Color>,
    /// The session this line belongs to (for clicks and scrolling).
    pub(crate) key: Option<RowKey>,
    /// The cursor's first line: `window` keeps it on screen.
    pub(crate) cursor: bool,
}

/// The working spinner's frames.
pub(crate) const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// A session on its way to starting: a slower, quieter turn.
pub(crate) const STARTING: [&str; 4] = ["◜", "◝", "◞", "◟"];

pub(crate) fn width(s: &str) -> u16 {
    u16::try_from(s.width()).unwrap_or(u16::MAX)
}

impl Ln {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn text(x: u16, s: impl Into<String>, style: Style) -> Self {
        let mut l = Self::new();
        l.put(x, s, style);
        l
    }

    /// Put `s` at column `x`; returns the column after it.
    pub(crate) fn put(&mut self, x: u16, s: impl Into<String>, style: Style) -> u16 {
        let s = s.into();
        let end = x.saturating_add(width(&s));
        self.parts.push((x, s, style));
        end
    }

    /// Put `s` so it ends just before column `end`; returns its start.
    pub(crate) fn rput(&mut self, end: u16, s: impl Into<String>, style: Style) -> u16 {
        let s = s.into();
        let x = end.saturating_sub(width(&s));
        self.parts.push((x, s, style));
        x
    }

    /// Put `s` at `x`, cut to `max` columns.
    pub(crate) fn fit(&mut self, x: u16, s: &str, max: u16, style: Style) -> u16 {
        self.put(x, truncate_width(s, usize::from(max)), style)
    }

    pub(crate) fn is_blank(&self) -> bool {
        self.parts.is_empty()
    }

    /// The line as text, gaps as spaces.
    #[cfg(test)]
    pub(crate) fn plain(&self) -> String {
        let mut out = String::new();
        for (x, s, _) in &self.parts {
            while width(&out) < *x {
                out.push(' ');
            }
            out.push_str(s);
        }
        out
    }

    /// Mark this line as the cursor's: the bar, the selection background.
    pub(crate) fn select(&mut self, ink: &Ink, bg: Option<Color>) {
        self.put(1, "▌", ink.accent);
        self.bg = bg;
        self.cursor = true;
    }

    pub(crate) fn with_key(mut self, key: &RowKey) -> Self {
        self.key = Some(key.clone());
        self
    }
}

/// Keep the cursor's lines (the cursor line and those after it with the
/// same key), plus one line after them, inside `height`; otherwise start
/// at the top.
pub(crate) fn window(mut lines: Vec<Ln>, height: usize) -> Vec<Ln> {
    if lines.len() <= height {
        return lines;
    }
    let Some(first) = lines.iter().position(|l| l.cursor) else {
        lines.truncate(height);
        return lines;
    };
    let mut last = first;
    while lines.get(last + 1).is_some_and(|l| l.key.is_some() && l.key == lines[first].key) {
        last += 1;
    }
    let top = if last < height { 0 } else { (last + 2).saturating_sub(height).min(lines.len() - height) };
    lines.drain(..top);
    lines.truncate(height);
    lines
}

/// Draw `lines` from the top of `area`; a line's background fills its
/// whole row.
pub(crate) fn paint(buf: &mut Buffer, area: Rect, lines: &[Ln]) {
    for (i, ln) in lines.iter().enumerate().take(usize::from(area.height)) {
        let y = area.y + i as u16;
        if let Some(bg) = ln.bg {
            buf.set_style(Rect { x: area.x, y, width: area.width, height: 1 }, Style::default().bg(bg));
        }
        for (x, s, style) in &ln.parts {
            if *x >= area.width {
                continue;
            }
            let room = usize::from(area.width - x);
            let s = truncate_width(s, room);
            let style = match ln.bg {
                Some(bg) if style.bg.is_none() => style.bg(bg),
                _ => *style,
            };
            buf.set_stringn(area.x + x, y, &s, room, style);
        }
    }
}

/// Styles for the four roles, plus emphasis.
pub(crate) struct Ink {
    pub(crate) text: Style,
    pub(crate) bold: Style,
    pub(crate) dim: Style,
    pub(crate) attention: Style,
    pub(crate) accent: Style,
}

impl Ink {
    pub(crate) fn new(t: &super::theme::Theme) -> Self {
        let text = Style::default().fg(t.text);
        Self {
            text,
            bold: text.add_modifier(Modifier::BOLD),
            dim: Style::default().fg(t.dim),
            attention: Style::default().fg(t.attention),
            accent: Style::default().fg(t.accent),
        }
    }
}

/// Lines of a wrapped paragraph at `x`, for the panel and dialogs.
pub(crate) fn para(text: &str, x: u16, cols: u16, style: Style) -> Vec<Ln> {
    wrap(text, cols).into_iter().map(|t| Ln::text(x, t, style)).collect()
}

/// Word-wrap `s` to `cols`; never returns an empty list.
pub(crate) fn wrap(s: &str, cols: u16) -> Vec<String> {
    crate::util::text::wrap_words(s, usize::from(cols.max(8)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(id: &str, cursor: bool) -> Ln {
        let mut l = Ln::new().with_key(&RowKey { device: None, id: id.into() });
        l.cursor = cursor;
        l
    }

    #[test]
    fn window_keeps_the_cursor_and_its_option_line_in_view() {
        let mut lines: Vec<Ln> = (0..10).map(|i| keyed(&i.to_string(), false)).collect();
        lines[8].cursor = true;
        lines.insert(9, keyed("8", false));
        let shown = window(lines, 5);
        let ids: Vec<&str> = shown.iter().map(|l| l.key.as_ref().unwrap().id.as_str()).collect();
        assert_eq!(ids, ["6", "7", "8", "8", "9"], "the whole group, and one line after it");
    }

    #[test]
    fn window_follows_the_cursor_through_a_long_list() {
        for cur in 0..30 {
            let mut lines: Vec<Ln> = vec![Ln::text(0, "Idle", Style::default())];
            lines.extend((0..30).map(|i| keyed(&i.to_string(), i == cur)));
            let shown = window(lines, 12);
            assert_eq!(shown.len(), 12);
            assert!(shown.iter().any(|l| l.cursor), "row {cur} stays in view");
        }
    }

    #[test]
    fn paint_clips_to_the_area() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        let mut l = Ln::new();
        l.put(2, "abcdefgh", Style::default());
        paint(&mut buf, area, &[l]);
        assert_eq!(buf.content().iter().map(|c| c.symbol()).collect::<String>(), "  abc…");
    }
}
