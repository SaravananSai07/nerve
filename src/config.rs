use std::collections::HashMap;

use serde::Deserialize;

use crate::log_warn;
use crate::paths::Paths;
use crate::util::sanitize::strip_ansi;

#[derive(Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub updates: UpdatesConfig,
    #[serde(default)]
    pub session_names: HashMap<String, String>,
    #[serde(skip)]
    pub load_error: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct UpdatesConfig {
    pub check_on_launch: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            check_on_launch: true,
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct NotificationConfig {
    pub on_complete: bool,
    pub on_waiting: bool,
    pub on_error: bool,
    pub sound: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            on_complete: true,
            on_waiting: true,
            on_error: true,
            sound: true,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub refresh_interval_ms: u64,
    pub process_scan_interval_ms: u64,
    pub terminal: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            refresh_interval_ms: 1000,
            process_scan_interval_ms: 5000,
            terminal: "auto".into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub theme: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: "nightfox".into(),
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Self {
        let path = paths.config_file();
        let contents = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                let msg = format!("config: cannot read {}: {e}", path.display());
                log_warn!("{msg}");
                return Self {
                    load_error: Some(msg),
                    ..Default::default()
                };
            }
        };
        match toml::from_str::<Self>(&contents) {
            Ok(mut config) => {
                config.general.refresh_interval_ms =
                    config.general.refresh_interval_ms.clamp(100, 30_000);
                // Symmetric foot-gun guard: a zero here would refork `ps`
                // every tick; a huge value would hide terminated PIDs.
                config.general.process_scan_interval_ms =
                    config.general.process_scan_interval_ms.clamp(500, 60_000);
                // Override names render raw on the cards view. The config
                // file is normally trusted, but it may travel via dotfile
                // sync — sanitise once at load so the hot path doesn't have
                // to think about it.
                for value in config.session_names.values_mut() {
                    if let std::borrow::Cow::Owned(clean) = strip_ansi(value) {
                        *value = clean;
                    }
                }
                config
            }
            Err(e) => {
                let msg = format!(
                    "config: parse error at {} ({e}) — using defaults",
                    path.display()
                );
                log_warn!("{msg}");
                Self {
                    load_error: Some(msg),
                    ..Default::default()
                }
            }
        }
    }

    pub fn session_name_for(&self, cwd: &str) -> Option<&String> {
        self.session_names.get(cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_paths() -> (tempfile::TempDir, Paths) {
        // Lean on the Paths fields directly by constructing a temp config dir.
        // This keeps tests hermetic and independent of the user's real config.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::for_test(tmp.path().join("config"), tmp.path().join("home"));
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
        std::fs::write(
            paths.config_file(),
            "[general]\nrefresh_interval_ms = 50\n",
        )
        .unwrap();
        let config = Config::load(&paths);
        assert!(config.load_error.is_none());
        assert_eq!(config.general.refresh_interval_ms, 100);
    }

    #[test]
    fn process_scan_interval_is_clamped() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(
            paths.config_file(),
            "[general]\nprocess_scan_interval_ms = 0\n",
        )
        .unwrap();
        let config = Config::load(&paths);
        assert_eq!(config.general.process_scan_interval_ms, 500);

        std::fs::write(
            paths.config_file(),
            "[general]\nprocess_scan_interval_ms = 999999999\n",
        )
        .unwrap();
        let config = Config::load(&paths);
        assert_eq!(config.general.process_scan_interval_ms, 60_000);
    }

    #[test]
    fn session_name_overrides_are_sanitised() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(
            paths.config_file(),
            "[session_names]\n\"/home/u/p\" = \"clean\\u001b[2Jname\"\n",
        )
        .unwrap();
        let config = Config::load(&paths);
        assert_eq!(
            config.session_name_for("/home/u/p").map(String::as_str),
            Some("cleanname"),
        );
    }
}
