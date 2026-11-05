//! A box to type a prompt in: it wraps to its width, takes several lines,
//! and once the text outgrows its height it scrolls to keep the cursor in
//! view. ↵ sends; ⌥↵, ⇧↵ or Ctrl+J start a new line.

use std::cell::Cell;
use std::ops::Range;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthChar;

use super::paint::{Ink, Ln};

pub(crate) struct Editor {
    text: String,
    /// Byte offset of the cursor, always on a char boundary.
    at: usize,
    /// Most bytes it takes.
    max: usize,
    /// The width it last drew at, which ↑ and ↓ move through.
    cols: Cell<u16>,
    /// The first row it last showed.
    top: Cell<usize>,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    /// ↵: the text is ready.
    Submit,
    Handled,
    /// Not an editing key.
    Ignored,
}

impl Editor {
    pub(crate) fn new(max: usize) -> Self {
        Self::with_text("", max)
    }

    pub(crate) fn with_text(text: &str, max: usize) -> Self {
        let mut e = Self { text: String::new(), at: 0, max, cols: Cell::new(60), top: Cell::new(0) };
        e.insert(text);
        e
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn take(&mut self) -> String {
        self.at = 0;
        self.top.set(0);
        std::mem::take(&mut self.text)
    }

    /// Type or paste `s` at the cursor, as much of it as fits. Line ends
    /// become `\n`, tabs spaces, and other control characters go.
    pub(crate) fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
        let mut room = self.max.saturating_sub(self.text.len());
        let mut add = String::new();
        for c in s.chars().filter(|c| *c == '\n' || !c.is_control()) {
            if c.len_utf8() > room {
                break;
            }
            room -= c.len_utf8();
            add.push(c);
        }
        self.text.insert_str(self.at, &add);
        self.at += add.len();
    }

    pub(crate) fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> Edit {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let word = ctrl || mods.contains(KeyModifiers::ALT);
        match code {
            KeyCode::Enter if mods.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => self.insert("\n"),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter => return Edit::Submit,
            KeyCode::Char('a') if ctrl => self.at = self.row_of(self.at).1.start,
            KeyCode::Char('e') if ctrl => self.at = self.row_end(self.row_of(self.at).0),
            KeyCode::Char('u') if ctrl => {
                let start = self.row_of(self.at).1.start;
                self.text.replace_range(start..self.at, "");
                self.at = start;
            }
            KeyCode::Char('w') if ctrl => self.delete_back(self.word_start()),
            KeyCode::Char(_) if ctrl => return Edit::Ignored,
            KeyCode::Char(c) => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace if word => self.delete_back(self.word_start()),
            KeyCode::Backspace => self.delete_back(self.prev(self.at)),
            KeyCode::Delete => {
                let next = self.next(self.at);
                self.text.replace_range(self.at..next, "");
            }
            KeyCode::Left if word => self.at = self.word_start(),
            KeyCode::Right if word => self.at = self.word_end(),
            KeyCode::Left => self.at = self.prev(self.at),
            KeyCode::Right => self.at = self.next(self.at),
            KeyCode::Home => self.at = self.row_of(self.at).1.start,
            KeyCode::End => self.at = self.row_end(self.row_of(self.at).0),
            KeyCode::Up => self.vertical(-1),
            KeyCode::Down => self.vertical(1),
            _ => return Edit::Ignored,
        }
        Edit::Handled
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
    }

    fn delete_back(&mut self, from: usize) {
        self.text.replace_range(from..self.at, "");
        self.at = from;
    }

    /// The start of the word before the cursor (spaces before it too).
    fn word_start(&self) -> usize {
        let before = self.text[..self.at].trim_end_matches([' ', '\n']);
        before.rfind([' ', '\n']).map_or(0, |i| i + 1)
    }

    fn word_end(&self) -> usize {
        let rest = &self.text[self.at..];
        let skip = rest.len() - rest.trim_start_matches([' ', '\n']).len();
        self.at + rest[skip..].find([' ', '\n']).map_or(rest.len(), |i| skip + i)
    }

    fn rows(&self) -> Vec<Range<usize>> {
        rows(&self.text, usize::from(self.cols.get().max(4)))
    }

    /// The row the cursor at `at` shows on: (index, its bytes).
    fn row_of(&self, at: usize) -> (usize, Range<usize>) {
        let rows = self.rows();
        let i = cursor_row(&rows, at);
        (i, rows[i].clone())
    }

    /// The last place the cursor can stand on row `i`: before the space
    /// a wrapped row breaks after, at the end of a line.
    fn row_end(&self, i: usize) -> usize {
        let rows = self.rows();
        let r = &rows[i];
        if rows.get(i + 1).is_some_and(|n| n.start == r.end) {
            self.prev(r.end)
        } else {
            r.end
        }
    }

    fn vertical(&mut self, by: isize) {
        let rows = self.rows();
        let i = cursor_row(&rows, self.at);
        let Some(t) = i.checked_add_signed(by).filter(|t| *t < rows.len()) else {
            self.at = if by < 0 { 0 } else { self.text.len() };
            return;
        };
        let col = width(&self.text[rows[i].start..self.at]);
        let (mut at, mut used, end) = (rows[t].start, 0, self.row_end(t));
        for (j, c) in self.text[rows[t].start..end].char_indices() {
            let w = c.width().unwrap_or(0);
            if used + w > col {
                break;
            }
            used += w;
            at = rows[t].start + j + c.len_utf8();
        }
        self.at = at;
    }

    /// The box at `x`, `cols` wide: at least `min` rows, at most `max`,
    /// scrolled so the cursor shows. `›` marks the first line, `↑` that
    /// more lies above.
    /// Rows scrolled off the top at the last `lines`.
    pub(crate) fn above(&self) -> usize {
        self.top.get()
    }

    pub(crate) fn lines(&self, x: u16, cols: u16, min: usize, max: usize, ink: &Ink) -> Vec<Ln> {
        let inner = cols.saturating_sub(3).max(4);
        self.cols.set(inner);
        let rows = self.rows();
        let cur = cursor_row(&rows, self.at);
        let height = max.max(1);
        let mut top = self.top.get().min(rows.len().saturating_sub(1));
        if cur < top {
            top = cur;
        } else if cur >= top + height {
            top = cur + 1 - height;
        }
        self.top.set(top);
        let mut out = Vec::new();
        for (i, r) in rows.iter().enumerate().skip(top).take(height) {
            let mut l = Ln::new();
            if i == 0 {
                l.put(x, "›", ink.accent);
            } else if i == top {
                l.put(x, "↑", ink.dim);
            }
            l.put(x + 2, &self.text[r.clone()], ink.text);
            if i == cur {
                let col = (width(&self.text[r.start..self.at]) as u16).min(inner.saturating_sub(1));
                let under = self.text[self.at..r.end].chars().next().unwrap_or(' ');
                l.put(x + 2 + col, under.to_string(), ink.text.add_modifier(Modifier::REVERSED));
            }
            out.push(l);
        }
        while out.len() < min {
            out.push(Ln::new());
        }
        out
    }
}

fn width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// `text` as rows of at most `cols` columns: lines split at `\n` (not part
/// of any row), long ones wrapped after a space, or mid-word when a word
/// is too long. Spaces a row breaks after stay on it.
fn rows(text: &str, cols: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut line_start = 0;
    for line in text.split('\n') {
        let mut start = line_start;
        let mut used = 0;
        let mut after_space: Option<usize> = None;
        for (i, c) in line.char_indices() {
            let b = line_start + i;
            let w = c.width().unwrap_or(0);
            if c != ' ' && used + w > cols && b > start {
                let cut = after_space.filter(|s| *s > start).unwrap_or(b);
                out.push(start..cut);
                start = cut;
                used = width(&text[cut..b]);
                after_space = None;
            }
            used += w;
            if c == ' ' {
                after_space = Some(b + 1);
            }
        }
        out.push(start..line_start + line.len());
        line_start += line.len() + 1;
    }
    out
}

/// The row the cursor at `at` shows on. At a wrap, the start of the next row.
fn cursor_row(rows: &[Range<usize>], at: usize) -> usize {
    rows.iter()
        .enumerate()
        .position(|(i, r)| r.contains(&at) || (at == r.end && rows.get(i + 1).is_none_or(|n| n.start > r.end)))
        .unwrap_or(rows.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    fn shown(e: &Editor, cols: u16, max: usize) -> Vec<String> {
        let ink = Ink::new(&Theme::catalog(None)[0]);
        e.lines(0, cols, 0, max, &ink).iter().map(Ln::plain).map(|s| s.trim_end().to_string()).collect()
    }

    fn typed(s: &str) -> Editor {
        let mut e = Editor::new(10_000);
        for c in s.chars() {
            e.key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        e
    }

    #[test]
    fn long_text_wraps_at_words_instead_of_scrolling_sideways() {
        let e = typed("fix the flaky test in the sync agent please");
        assert_eq!(shown(&e, 23, 9), ["› fix the flaky test", "  in the sync agent", "  please"]);
        assert_eq!(rows("abcdefghij", 4), [0..4, 4..8, 8..10], "a word longer than a row breaks mid-word");
    }

    #[test]
    fn new_lines_come_from_alt_or_shift_enter_and_plain_enter_submits() {
        let mut e = typed("one");
        assert_eq!(e.key(KeyCode::Enter, KeyModifiers::ALT), Edit::Handled);
        e.key(KeyCode::Char('t'), KeyModifiers::NONE);
        e.key(KeyCode::Enter, KeyModifiers::SHIFT);
        e.key(KeyCode::Char('j'), KeyModifiers::CONTROL);
        e.key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(e.text(), "one\nt\n\nx");
        assert_eq!(e.key(KeyCode::Enter, KeyModifiers::NONE), Edit::Submit);
    }

    #[test]
    fn the_cursor_moves_and_edits_anywhere_in_the_text() {
        let mut e = typed("hello world");
        e.key(KeyCode::Left, KeyModifiers::ALT);
        e.key(KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(e.text(), "hello Xworld");
        e.key(KeyCode::Home, KeyModifiers::NONE);
        e.key(KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(e.text(), "ello Xworld");
        e.key(KeyCode::End, KeyModifiers::NONE);
        e.key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(e.text(), "ello Xworl");
    }

    #[test]
    fn up_and_down_keep_the_column_across_rows() {
        let mut e = typed("abcdef\nxy\nlmnopq");
        e.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(e.at, 9, "the end of the shorter line above");
        e.key(KeyCode::Up, KeyModifiers::NONE);
        e.key(KeyCode::Char('_'), KeyModifiers::NONE);
        assert_eq!(e.text(), "ab_cdef\nxy\nlmnopq");
        e.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(e.at, 0, "up from the first row goes to the start");
    }

    #[test]
    fn a_tall_prompt_scrolls_to_keep_the_cursor_in_view() {
        let mut e = typed("1\n2\n3\n4\n5\n6");
        assert_eq!(shown(&e, 20, 3), ["↑ 4", "  5", "  6"]);
        for _ in 0..5 {
            e.key(KeyCode::Up, KeyModifiers::NONE);
        }
        assert_eq!(shown(&e, 20, 3), ["› 1", "  2", "  3"]);
    }

    #[test]
    fn a_paste_keeps_its_lines_and_stops_at_the_limit() {
        let mut e = Editor::new(8);
        e.insert("ab\r\ncd\tefghij");
        assert_eq!(e.text(), "ab\ncd   ");
    }
}
