use std::borrow::Cow;

/// Strip terminal-control bytes from `input`, preserving normal text. Returns
/// a borrowed `Cow` when nothing was removed.
///
/// Bytes removed:
/// - C0 control characters (`0x00..=0x1F`) except `\t`, `\n`, `\r`
/// - DEL (`0x7F`)
/// - ESC-prefixed sequences (CSI `ESC [`, OSC `ESC ]`, two-char ESC X)
/// - C1 control characters (`0x80..=0x9F`) when they appear as standalone bytes
///
/// Defends against an untrusted JSONL transcript painting the host terminal
/// with arbitrary escapes (cursor moves, OSC 52 clipboard writes, etc.).
pub(crate) fn strip_ansi(input: &str) -> Cow<'_, str> {
    if input.chars().all(is_safe_char) {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // CSI / OSC / two-char ESC sequences. The OSC terminator can be
            // either BEL (0x07) or ST (ESC \).
            '\x1b' => {
                match chars.next() {
                    Some('[') => skip_csi(&mut chars),
                    Some(']') => skip_osc(&mut chars),
                    Some('P') | Some('X') | Some('^') | Some('_') => {
                        // DCS, SOS, PM, APC — terminate at ST (ESC \) or BEL.
                        skip_osc(&mut chars);
                    }
                    Some(_) => {
                        // Two-byte ESC sequence: discard the escape byte.
                    }
                    None => {}
                }
            }
            // Allow ordinary whitespace.
            '\t' | '\n' | '\r' => out.push(ch),
            // Drop C0 control bytes, DEL, and C1 controls; keep everything else.
            c if (c as u32) < 0x20 => {}
            '\x7f' => {}
            c if (0x80..=0x9f).contains(&(c as u32)) => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

fn skip_csi<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    // CSI: parameter bytes 0x30–0x3F, intermediate bytes 0x20–0x2F,
    // final byte 0x40–0x7E. Stop at the first final byte (inclusive).
    for ch in chars.by_ref() {
        let c = ch as u32;
        if (0x40..=0x7e).contains(&c) {
            break;
        }
    }
}

fn skip_osc<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    // OSC and friends terminate at BEL (0x07) or ST (ESC \).
    while let Some(ch) = chars.next() {
        if ch == '\x07' {
            break;
        }
        if ch == '\x1b' {
            if matches!(chars.peek(), Some('\\')) {
                chars.next();
            }
            break;
        }
    }
}

fn is_safe_char(c: char) -> bool {
    // Allow ordinary whitespace, printable ASCII, and any char from NBSP
    // upward. Forbid C0 (other than whitespace), DEL, and the C1 range.
    matches!(
        c as u32,
        0x09 | 0x0a | 0x0d | 0x20..=0x7e | 0xa0..=0x10_ffff,
    )
}

/// An owned string that has passed through ANSI / control-byte
/// filtering. Constructible only via `Sanitised::new`, so the type
/// system carries the "this string is safe to render in the TUI"
/// invariant from ingestion boundaries through to display.
///
/// Idempotent on its own output: re-wrapping an already-`Sanitised`
/// value (via `.as_str()` round-trip) yields the same bytes. Note
/// that `strip_ansi` alone is *not* a fixed point — it lets Cf
/// format chars (U+200E etc.) through that the second-pass
/// `is_control` filter drops. The second pass is the source of
/// truth for "safe to render"; treat `strip_ansi` as a CSI/OSC/DCS
/// pre-filter, not a complete sanitiser.
// DO NOT add `impl From<String> for Sanitised` or `impl From<&str>`:
// either would let callers bypass `Sanitised::new` and silently
// undo the type-level guarantee crate-wide. The only inhabited-
// string constructor is `new`. `Default` is derived because the
// empty string is vacuously safe (no escapes possible).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Sanitised(String);

impl Sanitised {
    pub(crate) fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let cleaned = strip_ansi(&raw).into_owned();
        let filtered: String = cleaned
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
            .collect();
        Self(filtered)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

impl std::fmt::Display for Sanitised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Sanitised {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Sanitised {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Sanitised {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_borrowed() {
        let s = "hello world";
        let out = strip_ansi(s);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "hello world");
    }

    #[test]
    fn csi_sequence_removed() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn osc_52_clipboard_write_removed() {
        // OSC 52 with BEL terminator
        assert_eq!(strip_ansi("ok\x1b]52;c;ZXZpbA==\x07after"), "okafter");
        // OSC 52 with ST terminator (ESC \)
        assert_eq!(strip_ansi("ok\x1b]52;c;ZXZpbA==\x1b\\after"), "okafter");
    }

    #[test]
    fn dcs_sos_pm_apc_removed() {
        assert_eq!(strip_ansi("a\x1bPignored\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1bXignored\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1b^ignored\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1b_ignored\x1b\\b"), "ab");
    }

    #[test]
    fn whitespace_preserved() {
        assert_eq!(strip_ansi("line1\nline2\ttabbed\r\nend"), "line1\nline2\ttabbed\r\nend");
    }

    #[test]
    fn c0_controls_removed_except_whitespace() {
        let input = "a\x01b\x02c\x07d\x08e";
        assert_eq!(strip_ansi(input), "abcde");
    }

    #[test]
    fn del_removed() {
        assert_eq!(strip_ansi("a\x7fb"), "ab");
    }

    #[test]
    fn c1_standalone_removed() {
        // 0x9b is single-byte CSI; some terminals interpret it.
        assert_eq!(strip_ansi("a\u{9b}31mb"), "a31mb");
    }

    #[test]
    fn nested_escapes_handled() {
        let s = "\x1b[31m\x1b]2;title\x07hello\x1b[0m";
        assert_eq!(strip_ansi(s), "hello");
    }

    #[test]
    fn trailing_unterminated_escape_drops_remainder() {
        // Unterminated CSI: drop what we can find.
        let s = "ok\x1b[";
        assert_eq!(strip_ansi(s), "ok");
    }

    #[test]
    fn unicode_passes_through() {
        let s = "café — émoji 🎉";
        assert_eq!(strip_ansi(s), s);
    }

    // --- Sanitised newtype ---

    #[test]
    fn new_strips_ansi_csi_sequences() {
        assert_eq!(Sanitised::new("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn new_strips_osc_sequences() {
        assert_eq!(Sanitised::new("ok\x1b]52;c;ZXZpbA==\x07tail"), "oktail");
    }

    #[test]
    fn new_preserves_tab_newline_cr() {
        let input = "a\tb\nc\rd";
        assert_eq!(Sanitised::new(input), input);
    }

    #[test]
    fn new_drops_other_control_bytes() {
        // NUL (\x00), BEL (\x07), and DEL (\x7f) are all stripped.
        assert_eq!(Sanitised::new("a\x00b\x07c\x7fd"), "abcd");
    }

    #[test]
    fn new_is_idempotent() {
        let once = Sanitised::new("\x1b[1mfoo");
        let twice = Sanitised::new(once.as_str());
        assert_eq!(once, twice);
    }

    #[test]
    fn display_writes_inner_string() {
        assert_eq!(format!("{}", Sanitised::new("hello")), "hello");
    }

    #[test]
    fn partial_eq_against_str_literal() {
        assert!(Sanitised::new("x") == "x");
    }

    #[test]
    fn default_is_empty() {
        assert!(Sanitised::default().is_empty());
    }
}
