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

#[cfg(test)]
mod tests {
    use super::*;

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
