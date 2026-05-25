/// Bridge ids are at most 128 chars in practice (UUID + tag +
/// delimiter); capping a little above that prevents an attacker
/// launching `nerve --focus <huge>` from forcing a multi-megabyte
/// allocation before the parser even looks at the string.
pub const FOCUS_ARG_MAX_LEN: usize = 160;

/// True if `c` belongs in a bridge identifier: ASCII alphanumeric
/// plus the small set of delimiters the bridge protocols actually
/// use (`: _ - % . $`). Used both by `--focus` validation and by
/// the notification path that hands ids to `terminal-notifier`.
pub fn is_safe_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '%' | '.' | '$')
}

#[derive(Debug, PartialEq, Eq)]
pub enum FocusValidation {
    Ok(String),
    Empty,
    TooLong(usize),
    ForbiddenChar(char),
}

pub fn validate(arg: String) -> FocusValidation {
    if arg.is_empty() {
        return FocusValidation::Empty;
    }
    if arg.len() > FOCUS_ARG_MAX_LEN {
        return FocusValidation::TooLong(arg.len());
    }
    for ch in arg.chars() {
        if !is_safe_id_char(ch) {
            return FocusValidation::ForbiddenChar(ch);
        }
    }
    FocusValidation::Ok(arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_uuid_passes() {
        let s = "ghostty:abc-123_def".to_string();
        assert!(matches!(validate(s.clone()), FocusValidation::Ok(v) if v == s));
    }

    #[test]
    fn tmux_pane_id_passes() {
        let s = "tmux:%42".to_string();
        assert!(matches!(validate(s.clone()), FocusValidation::Ok(v) if v == s));
    }

    #[test]
    fn empty_rejected() {
        assert_eq!(validate(String::new()), FocusValidation::Empty);
    }

    #[test]
    fn overlong_rejected() {
        let s = "a".repeat(FOCUS_ARG_MAX_LEN + 1);
        let len = s.len();
        assert_eq!(validate(s), FocusValidation::TooLong(len));
    }

    #[test]
    fn shell_metacharacters_rejected() {
        for bad in ["a;b", "a`b", "a$(b)", "a|b", "a&b", "a b", "a'b", "a\"b", "a\nb"] {
            match validate(bad.to_string()) {
                FocusValidation::ForbiddenChar(_) => {}
                other => panic!("expected ForbiddenChar for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn unicode_rejected() {
        match validate("café".to_string()) {
            FocusValidation::ForbiddenChar(c) => assert_eq!(c, 'é'),
            other => panic!("expected ForbiddenChar('é'), got {other:?}"),
        }
    }
}
