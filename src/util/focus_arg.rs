/// Maximum permitted length of a `--focus` argument. Bridge ids are at most
/// 128 chars (UUID + bridge tag + delimiter). Capping here prevents an
/// attacker-launched `nerve --focus <huge>` from allocating arbitrary RAM
/// before the parser ever gets to look at the string.
pub const FOCUS_ARG_MAX_LEN: usize = 160;

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
        let b = ch as u32;
        let allowed = ch.is_ascii_alphanumeric()
            || matches!(b, 0x3a | 0x5f | 0x2d | 0x25 | 0x2e | 0x24); // : _ - % . $
        if !allowed {
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
