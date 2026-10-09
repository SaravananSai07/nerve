use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Truncate `s` to at most `max_graphemes` grapheme clusters, appending
/// `…` when truncation occurred. Operates on graphemes rather than
/// codepoints so a decomposed accent (`e` + U+0301), a flag emoji
/// (regional-indicator pair), or a ZWJ sequence (👨‍💻) isn't sliced
/// mid-cluster — that would leave orphan combining marks in the
/// output.
///
/// `max_graphemes == 0` always returns an empty string;
/// `max_graphemes == 1` returns just the ellipsis when truncation
/// is needed.
pub(crate) fn truncate_graphemes(s: &str, max_graphemes: usize) -> String {
    if max_graphemes == 0 {
        return String::new();
    }
    let clusters: Vec<&str> = s.graphemes(true).collect();
    if clusters.len() <= max_graphemes {
        return s.to_string();
    }
    let keep = max_graphemes.saturating_sub(1);
    let mut out: String = clusters.iter().take(keep).copied().collect();
    out.push('…');
    out
}

/// Truncate `s` to at most `max_cols` terminal columns, appending `…`
/// when truncation occurred. Like `truncate_graphemes` it never splits a
/// cluster, but it budgets by display width: a CJK or emoji cluster takes
/// two columns, so a grapheme count would overrun the space.
pub(crate) fn truncate_width(s: &str, max_cols: usize) -> String {
    if s.width() <= max_cols {
        return s.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let budget = max_cols - 1; // room for the ellipsis
    let mut out = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > budget {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('…');
    out
}

/// Single-quote for a POSIX shell. Safe for any input: an embedded `'`
/// closes the quote, emits an escaped quote, and reopens.
pub(crate) fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Coarse "how long ago": one unit, largest first. For ages that can run
/// to months, where "2904h 05m" would be unreadable.
pub(crate) fn format_age(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// Greedy word wrap to `cols` display columns. Words wider than a line
/// (paths, ids) are split between grapheme clusters.
pub(crate) fn wrap_words(text: &str, cols: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut used = 0;
    for word in text.split_whitespace() {
        let w = word.width();
        if used > 0 && used + 1 + w > cols {
            lines.push(String::new());
            used = 0;
        }
        if used > 0 {
            lines.last_mut().unwrap().push(' ');
            used += 1;
        }
        for g in word.graphemes(true) {
            let gw = g.width();
            if used + gw > cols && used > 0 {
                lines.push(String::new());
                used = 0;
            }
            lines.last_mut().unwrap().push_str(g);
            used += gw;
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_words_and_splits_long_ones() {
        assert_eq!(wrap_words("stay silent to hear reply", 12), ["stay silent", "to hear", "reply"]);
        assert_eq!(wrap_words("/a/very/long/path", 8), ["/a/very/", "long/pat", "h"]);
        assert_eq!(wrap_words("", 8), [""]);
    }

    #[test]
    fn age_uses_one_coarse_unit() {
        assert_eq!(format_age(5.0), "just now");
        assert_eq!(format_age(150.0), "2m ago");
        assert_eq!(format_age(7300.0), "2h ago");
        assert_eq!(format_age(127.0 * 86_400.0), "127d ago");
        assert_eq!(format_age(-1.0), "just now");
    }

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("/usr/local/bin/nerve"), "'/usr/local/bin/nerve'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("/Users/jane doe/bin"), "'/Users/jane doe/bin'");
    }

    #[test]
    fn width_truncation_counts_wide_clusters_twice() {
        // 5 CJK chars = 10 columns; a grapheme budget of 6 would keep all.
        let s = "漢字漢字漢";
        let t = truncate_width(s, 6);
        assert_eq!(t, "漢字…");
        assert!(t.width() <= 6);
        assert_eq!(truncate_width("short", 10), "short");
        assert_eq!(truncate_width("abc", 0), "");
    }

    #[test]
    fn returns_input_when_within_cap() {
        assert_eq!(truncate_graphemes("hello", 10), "hello");
        assert_eq!(truncate_graphemes("hello", 5), "hello");
    }

    #[test]
    fn appends_ellipsis_when_truncating() {
        assert_eq!(truncate_graphemes("hello world", 5), "hell…");
    }

    #[test]
    fn keeps_grapheme_clusters_intact() {
        // 'é' decomposed = 'e' (U+0065) + combining acute (U+0301).
        let s = "cafe\u{0301}teria";
        // Treating each cluster as 1: c|a|f|é|t|e|r|i|a = 9. Truncate to 5:
        // c|a|f|é|… (the ellipsis sits where the next cluster would have).
        assert_eq!(truncate_graphemes(s, 5), "cafe\u{0301}…");
    }

    #[test]
    fn handles_emoji_zwj_sequences() {
        // 👨‍💻 is a single grapheme but multiple codepoints.
        let s = "👨\u{200d}💻 hi";
        // Clusters: |👨💻|space|h|i = 5 clusters total.
        let out = truncate_graphemes(s, 3);
        // Should keep emoji + space, then ellipsis. No half-emoji.
        assert!(out.starts_with("👨\u{200d}💻"));
        assert!(out.ends_with('…'));
    }

    #[test]
    fn handles_empty_input() {
        assert_eq!(truncate_graphemes("", 5), "");
    }

    #[test]
    fn max_one_returns_ellipsis_only() {
        assert_eq!(truncate_graphemes("hello", 1), "…");
    }

    #[test]
    fn max_zero_returns_empty_regardless_of_input() {
        // The doc-comment promises an empty string; previously the
        // body fell through to `push('…')` and returned a single
        // ellipsis instead.
        assert_eq!(truncate_graphemes("hello", 0), "");
        assert_eq!(truncate_graphemes("", 0), "");
        assert_eq!(truncate_graphemes("👨\u{200d}💻", 0), "");
    }
}
