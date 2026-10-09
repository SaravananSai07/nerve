use std::path::{Path, PathBuf};

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

    pub(crate) fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub(crate) fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub(crate) fn prefs_file(&self) -> PathBuf {
        self.config_dir.join("prefs.toml")
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

    #[cfg(test)]
    pub(crate) fn for_test(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }
}
