use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    config_dir: PathBuf,
    home_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Option<Self> {
        // `NERVE_CONFIG_DIR` overrides the OS default. Used by integration
        // tests to keep the user's real config dir untouched; also useful for
        // users who want nerve's state outside their `~/Library/Application
        // Support` (macOS) or `~/.config` (Linux).
        let config_dir = match std::env::var("NERVE_CONFIG_DIR") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => dirs::config_dir()?.join("nerve"),
        };
        Some(Self {
            config_dir,
            home_dir: dirs::home_dir()?,
        })
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn prefs_file(&self) -> PathBuf {
        self.config_dir.join("prefs.toml")
    }

    pub fn update_cache_file(&self) -> PathBuf {
        self.config_dir.join("update_cache.json")
    }

    pub fn log_file(&self) -> PathBuf {
        self.config_dir.join("nerve.log")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.config_dir.join("nerve.lock")
    }

    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    pub fn claude_root(&self) -> PathBuf {
        self.home_dir.join(".claude")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.claude_root().join("sessions")
    }

    pub fn ensure_config_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)
    }

    #[cfg(test)]
    pub fn for_test(config_dir: PathBuf, home_dir: PathBuf) -> Self {
        Self {
            config_dir,
            home_dir,
        }
    }
}
