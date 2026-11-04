//! Claude's markdown, as far as a terminal shows it. Headings, lists,
//! quotes, code blocks and tables keep their shape; `**bold**` and
//! `` `code` `` keep their look; everything else wraps as text.

use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::paint::{Ink, Ln};

/// A run of text in one style.
type Span = (String, Style);

pub(crate) fn render(text: &str, x: u16, cols: u16, base: Style, ink: &Ink) -> Vec<Ln> {
    let cols = cols.max(8);
    let mut out: Vec<Ln> = Vec::new();
    let mut code = false;
    let mut table: Vec<Vec<String>> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('|') && !code {
            // `|---|---|` only marks the header.
            if !t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
                table.push(t.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect());
            }
            continue;
        }
        out.extend(grid(std::mem::take(&mut table), x, cols, base, ink));
        if t.starts_with("```") {
            code = !code;
            continue;
        }
        if code {
            let mut l = Ln::text(x, "│", ink.dim);
            l.fit(x + 2, line, cols - 2, ink.accent);
            out.push(l);
        } else if t.is_empty() {
            if out.last().is_some_and(|l| !l.is_blank()) {
                out.push(Ln::new());
            }
        } else if let Some(h) = heading(t) {
            out.extend(flow(&spans(h, base.add_modifier(Modifier::BOLD), ink), x, cols, None));
        } else if let Some(q) = t.strip_prefix('>') {
            out.extend(flow(&spans(q.trim(), ink.dim, ink), x + 2, cols - 2, Some(("│", ink.dim, x))));
        } else if let Some((mark, rest)) = item(line) {
            let indent = (line.len() - line.trim_start().len()).min(8) as u16;
            let at = x + indent;
            let gap = super::paint::width(&mark) + 1;
            out.extend(flow(
                &spans(rest, base, ink),
                at + gap,
                cols.saturating_sub(indent + gap),
                Some((&mark, ink.dim, at)),
            ));
        } else {
            out.extend(flow(&spans(t, base, ink), x, cols, None));
        }
    }
    out.extend(grid(table, x, cols, base, ink));
    while out.last().is_some_and(Ln::is_blank) {
        out.pop();
    }
    out
}

fn heading(t: &str) -> Option<&str> {
    let rest = t.trim_start_matches('#');
    (rest.len() < t.len() && rest.starts_with(' ')).then(|| rest.trim())
}

/// A list item's mark (• for bullets, the number for numbered ones) and
/// its text.
fn item(line: &str) -> Option<(String, &str)> {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ ")) {
        return Some(("•".into(), rest));
    }
    let digits = t.find(|c: char| !c.is_ascii_digit())?;
    let rest = t[digits..].strip_prefix(". ")?;
    (digits > 0).then(|| (format!("{}.", &t[..digits]), rest))
}

/// Inline styles: `code` first (nothing inside it is markup), then
/// **bold**. A mark without its closing twin stays as typed.
fn spans(t: &str, base: Style, ink: &Ink) -> Vec<Span> {
    let mut out = Vec::new();
    let mut rest = t;
    while !rest.is_empty() {
        let tick = rest.find('`').filter(|&i| rest[i + 1..].contains('`'));
        let star = rest.find("**").filter(|&i| rest[i + 2..].contains("**"));
        let (at, mark, style) = match (tick, star) {
            (Some(c), Some(b)) if b < c => (b, "**", base.add_modifier(Modifier::BOLD)),
            (Some(c), _) => (c, "`", ink.accent),
            (None, Some(b)) => (b, "**", base.add_modifier(Modifier::BOLD)),
            (None, None) => break,
        };
        out.push((rest[..at].to_string(), base));
        let inner = &rest[at + mark.len()..];
        let end = inner.find(mark).unwrap_or(inner.len());
        out.push((inner[..end].to_string(), style));
        rest = &inner[(end + mark.len()).min(inner.len())..];
    }
    out.push((rest.to_string(), base));
    out.retain(|(s, _)| !s.is_empty());
    out
}

/// Word-wrap styled spans into `cols` from column `x`. A word longer than
/// a line is split. `lead` is a mark drawn on the first line only.
fn flow(spans: &[Span], x: u16, cols: u16, lead: Option<(&str, Style, u16)>) -> Vec<Ln> {
    // Words, each a list of styled pieces with no space between them.
    let mut words: Vec<Vec<Span>> = Vec::new();
    let mut open = false;
    for (text, style) in spans {
        for (i, part) in text.split(char::is_whitespace).enumerate() {
            if i > 0 {
                open = false;
            }
            if part.is_empty() {
                continue;
            }
            if !open {
                words.push(Vec::new());
                open = true;
            }
            words.last_mut().unwrap().push((part.to_string(), *style));
        }
    }
    let cols = usize::from(cols.max(4));
    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut used = 0;
    for word in words {
        let w: usize = word.iter().map(|(s, _)| s.width()).sum();
        if used > 0 && used + 1 + w > cols {
            lines.push(Vec::new());
            used = 0;
        }
        if used > 0 {
            push(lines.last_mut().unwrap(), " ", word[0].1);
            used += 1;
        }
        for (s, style) in word {
            for g in s.graphemes(true) {
                let gw = g.width();
                if used + gw > cols && used > 0 {
                    lines.push(Vec::new());
                    used = 0;
                }
                push(lines.last_mut().unwrap(), g, style);
                used += gw;
            }
        }
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(i, parts)| {
            let mut l = Ln::new();
            if let (0, Some((mark, style, at))) = (i, lead) {
                l.put(at, mark, style);
            }
            let mut at = x;
            for (s, style) in parts {
                at = l.put(at, s, style);
            }
            l
        })
        .collect()
}

fn push(line: &mut Vec<Span>, s: &str, style: Style) {
    match line.last_mut() {
        Some((last, st)) if *st == style => last.push_str(s),
        _ => line.push((s.to_string(), style)),
    }
}

/// A table with its columns lined up, the header in bold. Too wide for
/// the panel, each column gives up its share and its cells are cut.
fn grid(rows: Vec<Vec<String>>, x: u16, cols: u16, base: Style, ink: &Ink) -> Vec<Ln> {
    if rows.is_empty() {
        return Vec::new();
    }
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths: Vec<u16> = (0..n)
        .map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| super::paint::width(&plain(s))).max().unwrap_or(0))
        .collect();
    let gaps = 3 * n.saturating_sub(1) as u16;
    while widths.iter().sum::<u16>() + gaps > cols {
        let Some(widest) = widths.iter_mut().max() else { break };
        if *widest <= 4 {
            break;
        }
        *widest -= 1;
    }
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let mut l = Ln::new();
            let mut at = x;
            for (c, w) in widths.iter().enumerate() {
                if c > 0 {
                    l.put(at, "│", ink.dim);
                    at += 2;
                }
                let style = if i == 0 { base.add_modifier(Modifier::BOLD) } else { base };
                l.fit(at, &plain(r.get(c).map_or("", String::as_str)), *w, style);
                at += w + 1;
            }
            l
        })
        .collect()
}

/// A cell's text without inline marks.
fn plain(s: &str) -> String {
    s.replace("**", "").replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    fn text(lines: &[Ln]) -> Vec<String> {
        lines.iter().map(Ln::plain).collect()
    }

    fn ink() -> Ink {
        Ink::new(&Theme::catalog(None)[0])
    }

    #[test]
    fn keeps_shape_and_drops_marks() {
        let ink = ink();
        let md = "## Result\n\nAll **five** pass in `src/a.rs`.\n- one\n- two that wraps onto a second line\n2. numbered\n\n```\nlet x = 1;\n```\n> quoted";
        let out = text(&render(md, 0, 24, ink.text, &ink));
        assert_eq!(
            out,
            [
                "Result",
                "",
                "All five pass in",
                "src/a.rs.",
                "• one",
                "• two that wraps onto a",
                "  second line",
                "2. numbered",
                "",
                "│ let x = 1;",
                "│ quoted",
            ]
        );
    }

    #[test]
    fn inline_styles_follow_their_marks() {
        let ink = ink();
        let s = spans("a **b** `c` **open", ink.text, &ink);
        assert_eq!(s[1], ("b".into(), ink.text.add_modifier(Modifier::BOLD)));
        assert_eq!(s[3], ("c".into(), ink.accent));
        assert_eq!(s.last().unwrap().0, " **open", "an unclosed mark stays as typed");
    }

    #[test]
    fn tables_line_up_and_fit() {
        let ink = ink();
        let md = "| Unit | Pass |\n|---|---|\n| S5 | 45/68 |\n| K11 | 18/18 |";
        assert_eq!(text(&render(md, 0, 40, ink.text, &ink)), ["Unit │ Pass", "S5   │ 45/68", "K11  │ 18/18"]);
        let narrow = text(&render("| a very long cell | another long cell |", 0, 20, ink.text, &ink));
        assert!(narrow[0].chars().count() <= 20, "{narrow:?}");
    }
}
