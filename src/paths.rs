use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    config_dir: PathBuf,
}

impl Paths {
    pub(crate) fn discover() -> Option<Self> {
        // `NERVE_CONFIG_DIR` overrides the OS default. Used by integration
        // tests to keep the user's real config dir untouched; also useful for
        // users who want nerve's state outside their `~/Library/Application
        // Support` (macOS) or `~/.config` (Linux).
        let config_dir = match std::env::var("NERVE_CONFIG_DIR") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => dirs::config_dir()?.join("nerve"),
        };
        // Nothing works without a home dir (Claude's config lives there),
        // so refuse to start rather than half-run.
        dirs::home_dir()?;
        Some(Self { config_dir })
    }

    /// Paths under a known config dir.
    pub(crate) fn at(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub(crate) fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub(crate) fn prefs_file(&self) -> PathBuf {
        self.config_dir.join("prefs.toml")
    }

    /// Who hears about what: `NotifyBook`.
    pub(crate) fn notify_file(&self) -> PathBuf {
        self.config_dir.join("notify.toml")
    }

    /// Where a running TUI leaves its own tab's id, for notification clicks.
    pub(crate) fn focus_file(&self) -> PathBuf {
        self.config_dir.join("tui-focus")
    }

    /// Where `nerve --land` leaves a session for the running TUI.
    pub(crate) fn land_file(&self) -> PathBuf {
        self.config_dir.join("land")
    }

    pub(crate) fn update_cache_file(&self) -> PathBuf {
        self.config_dir.join("update_cache.json")
    }

    pub(crate) fn log_file(&self) -> PathBuf {
        self.config_dir.join("nerve.log")
    }

    pub(crate) fn lock_file(&self) -> PathBuf {
        self.config_dir.join("nerve.lock")
    }

    pub(crate) fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    /// Per-session JSON written by `nerve statusline`, keyed by session id.
    pub(crate) fn statusline_dir(&self) -> PathBuf {
        self.config_dir.join("statusline")
    }

    pub(crate) fn ensure_config_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)
    }
}

/// The home dir as a string, read once; empty when unknown.
fn home() -> &'static str {
    static HOME: OnceLock<String> = OnceLock::new();
    HOME.get_or_init(|| dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default())
}

/// `~/x` for a path under the home dir: shorter, and what people type.
pub(crate) fn tilde(path: &str) -> String {
    tilde_in(home(), path)
}

/// A path that may start with `~` or `~/`, made whole.
pub(crate) fn untilde(path: &str) -> PathBuf {
    untilde_in(home(), path)
}

fn tilde_in(home: &str, path: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn untilde_in(home: &str, path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            PathBuf::from(format!("{home}{rest}"))
        }
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_round_trips_under_home_only() {
        let home = "/home/u";
        assert_eq!(tilde_in(home, "/home/u/code"), "~/code");
        assert_eq!(tilde_in(home, "/home/u"), "~");
        assert_eq!(tilde_in(home, "/home/user"), "/home/user");
        assert_eq!(tilde_in("", "/x"), "/x");
        assert_eq!(untilde_in(home, "~/code"), PathBuf::from("/home/u/code"));
        assert_eq!(untilde_in(home, "~"), PathBuf::from("/home/u"));
        assert_eq!(untilde_in(home, "~bob/x"), PathBuf::from("~bob/x"));
        assert_eq!(untilde_in("", "~/x"), PathBuf::from("~/x"));
    }
}
