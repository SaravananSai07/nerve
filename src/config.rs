//! `config.toml`, which people edit, and `prefs.toml`, which nerve keeps
//! for choices made in the TUI.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::{log_err, log_warn};

const CONFIG_MAX_BYTES: u64 = 256 * 1024;
const PREFS_MAX_BYTES: u64 = 64 * 1024;

#[derive(Deserialize, Default)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) general: GeneralConfig,
    #[serde(default)]
    pub(crate) appearance: AppearanceConfig,
    #[serde(default)]
    pub(crate) notifications: NotificationConfig,
    #[serde(default)]
    pub(crate) updates: UpdatesConfig,
    #[serde(default)]
    pub(crate) session_names: HashMap<String, String>,
    #[serde(skip)]
    pub(crate) load_error: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub(crate) struct UpdatesConfig {
    pub(crate) check_on_launch: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self { check_on_launch: true }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub(crate) struct NotificationConfig {
    pub(crate) sound: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { sound: true }
    }
}

#[derive(Deserialize)]
#[serde(default)]
pub(crate) struct GeneralConfig {
    pub(crate) refresh_interval_ms: u64,
    pub(crate) process_scan_interval_ms: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self { refresh_interval_ms: 1000, process_scan_interval_ms: 5000 }
    }
}

#[derive(Deserialize)]
#[serde(default)]
pub(crate) struct AppearanceConfig {
    pub(crate) theme: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self { theme: "terminal".into() }
    }
}

impl Config {
    pub(crate) fn load(paths: &Paths) -> Self {
        match crate::util::fs::load_toml::<Self>(&paths.config_file(), CONFIG_MAX_BYTES) {
            Ok(None) => Self::default(),
            Ok(Some(mut config)) => {
                config.general.refresh_interval_ms = config.general.refresh_interval_ms.clamp(100, 30_000);
                // Symmetric foot-gun guard: a zero here would refork `ps`
                // every tick; a huge value would hide terminated PIDs.
                config.general.process_scan_interval_ms = config.general.process_scan_interval_ms.clamp(500, 60_000);
                config
            }
            Err(e) => {
                let msg = format!("config: {e} — using defaults");
                log_warn!("{msg}");
                Self { load_error: Some(msg), ..Default::default() }
            }
        }
    }
}

/// Choices made in the TUI, kept in `prefs.toml`.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Prefs {
    /// Capturing a screen briefly switches tabs; the person said not to ask.
    pub(crate) preview_flicker_accepted: bool,
    /// The theme picked on the Settings page; wins over config.toml.
    pub(crate) theme: Option<String>,
    /// Why the file was ignored, shown once at startup.
    #[serde(skip)]
    pub(crate) load_error: Option<String>,
}

impl Prefs {
    pub(crate) fn load(paths: &Paths) -> Self {
        match crate::util::fs::load_toml::<Self>(&paths.prefs_file(), PREFS_MAX_BYTES) {
            Ok(None) => Self::default(),
            // The theme name is matched, never shown, but the file may
            // travel with dotfiles: strip control bytes all the same.
            Ok(Some(prefs)) => Self { theme: prefs.theme.map(crate::util::sanitize::clean), ..prefs },
            Err(e) => {
                let msg = format!("prefs: {e} — using defaults");
                log_warn!("{msg}");
                Self { load_error: Some(msg), ..Default::default() }
            }
        }
    }

    pub(crate) fn save(&self, paths: &Paths) {
        if let Err(e) = paths.ensure_config_dir() {
            log_warn!("prefs: cannot create config dir {}: {e}", paths.config_dir().display());
            return;
        }
        let path = paths.prefs_file();
        let content = match toml::to_string_pretty(self) {
            Ok(s) => s,
            Err(e) => {
                log_err!("prefs: serialize failed: {e}");
                return;
            }
        };
        if let Err(e) = std::fs::write(&path, content) {
            log_warn!("prefs: write failed at {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_paths() -> (tempfile::TempDir, Paths) {
        // Lean on the Paths fields directly by constructing a temp config dir.
        // This keeps tests hermetic and independent of the user's real config.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::at(tmp.path().join("config"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        (tmp, paths)
    }

    #[test]
    fn missing_config_returns_defaults_without_error() {
        let (_tmp, paths) = fixture_paths();
        let config = Config::load(&paths);
        assert!(config.load_error.is_none());
        assert_eq!(config.general.refresh_interval_ms, 1000);
    }

    #[test]
    fn corrupt_config_surfaces_load_error_and_defaults() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(paths.config_file(), "[invalid @@@").unwrap();
        let config = Config::load(&paths);
        let msg = config.load_error.expect("load error should be surfaced");
        assert!(msg.contains("parse error"), "unexpected message: {msg}");
        // Defaults still hold.
        assert_eq!(config.general.refresh_interval_ms, 1000);
    }

    #[test]
    fn refresh_interval_is_clamped() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(paths.config_file(), "[general]\nrefresh_interval_ms = 50\n").unwrap();
        let config = Config::load(&paths);
        assert!(config.load_error.is_none());
        assert_eq!(config.general.refresh_interval_ms, 100);
    }

    #[test]
    fn process_scan_interval_is_clamped() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(paths.config_file(), "[general]\nprocess_scan_interval_ms = 0\n").unwrap();
        let config = Config::load(&paths);
        assert_eq!(config.general.process_scan_interval_ms, 500);

        std::fs::write(paths.config_file(), "[general]\nprocess_scan_interval_ms = 999999999\n").unwrap();
        let config = Config::load(&paths);
        assert_eq!(config.general.process_scan_interval_ms, 60_000);
    }

    #[test]
    fn missing_or_corrupt_prefs_are_defaults() {
        let (_tmp, paths) = fixture_paths();
        let prefs = Prefs::load(&paths);
        assert!(!prefs.preview_flicker_accepted && prefs.theme.is_none() && prefs.load_error.is_none());
        std::fs::write(paths.prefs_file(), "[invalid @@").unwrap();
        let prefs = Prefs::load(&paths);
        assert!(!prefs.preview_flicker_accepted && prefs.load_error.is_some());
    }

    // Files from before 0.6 carry `version = 1` and a retired key.
    #[test]
    fn old_prefs_still_load() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(
            paths.prefs_file(),
            "version = 1\npreview_flicker_accepted = true\nnotifications_muted = false\n",
        )
        .unwrap();
        assert!(Prefs::load(&paths).preview_flicker_accepted);
    }

    #[test]
    fn prefs_round_trip() {
        let (_tmp, paths) = fixture_paths();
        let prefs = Prefs { preview_flicker_accepted: true, theme: Some("gruvbox".into()), load_error: None };
        prefs.save(&paths);
        let loaded = Prefs::load(&paths);
        assert!(loaded.preview_flicker_accepted);
        assert_eq!(loaded.theme.as_deref(), Some("gruvbox"));
    }
}
