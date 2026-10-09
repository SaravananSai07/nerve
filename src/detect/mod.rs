pub(crate) mod claude;
pub(crate) mod git;
pub(crate) mod jsonl;
pub(crate) mod jsonl_cache;
pub(crate) mod process;
pub(crate) mod statusline;

use std::path::PathBuf;

/// Claude Code config roots to scan, deduplicated, in priority order.
///
/// `$CLAUDE_CONFIG_DIR` comes first. Like Claude Code ≥ 2.1.284 we only
/// honour it when it's absolute. `~/.claude` is always included as well:
/// the Claude desktop app writes its sessions there even when the user's
/// shell points the CLI somewhere else.
pub(crate) fn claude_roots() -> Vec<PathBuf> {
    roots_from(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn roots_from(config_dir: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(2);
    if let Some(dir) = config_dir.filter(|d| d.is_absolute()) {
        roots.push(dir);
    }
    if let Some(default) = home.map(|h| h.join(".claude")) {
        if !roots.contains(&default) {
            roots.push(default);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_comes_first_and_default_is_kept() {
        let roots = roots_from(Some("/x/.claude-ext".into()), Some("/home/u".into()));
        assert_eq!(roots, vec![PathBuf::from("/x/.claude-ext"), PathBuf::from("/home/u/.claude")]);
    }

    #[test]
    fn relative_config_dir_is_ignored() {
        let roots = roots_from(Some("rel/dir".into()), Some("/home/u".into()));
        assert_eq!(roots, vec![PathBuf::from("/home/u/.claude")]);
    }

    #[test]
    fn config_dir_equal_to_default_is_not_duplicated() {
        let roots = roots_from(Some("/home/u/.claude".into()), Some("/home/u".into()));
        assert_eq!(roots, vec![PathBuf::from("/home/u/.claude")]);
    }
}
