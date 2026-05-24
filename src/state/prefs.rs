use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::{log_err, log_info, log_warn};

const CURRENT_VERSION: u32 = 1;

fn default_version() -> u32 {
    CURRENT_VERSION
}

#[derive(Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub preview_flicker_accepted: bool,
    #[serde(default)]
    pub notifications_muted: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            preview_flicker_accepted: false,
            notifications_muted: false,
        }
    }
}

impl Prefs {
    pub fn load(paths: &Paths) -> Self {
        let path = paths.prefs_file();
        let contents = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log_warn!("prefs: unable to read {}: {e}", path.display());
                return Self::default();
            }
        };
        match toml::from_str::<Self>(&contents) {
            Ok(mut prefs) => {
                prefs.migrate();
                prefs
            }
            Err(e) => {
                log_warn!(
                    "prefs: parse failed at {} ({e}) — using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    fn migrate(&mut self) {
        if self.version != CURRENT_VERSION {
            log_info!(
                "prefs: upgrading from v{} to v{CURRENT_VERSION}",
                self.version
            );
            self.version = CURRENT_VERSION;
        }
    }

    pub fn save(&self, paths: &Paths) {
        if let Err(e) = paths.ensure_config_dir() {
            log_warn!(
                "prefs: cannot create config dir {}: {e}",
                paths.config_dir().display()
            );
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::for_test(tmp.path().join("config"), tmp.path().join("home"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        (tmp, paths)
    }

    #[test]
    fn missing_prefs_returns_defaults_at_current_version() {
        let (_tmp, paths) = fixture_paths();
        let prefs = Prefs::load(&paths);
        assert_eq!(prefs.version, CURRENT_VERSION);
        assert!(!prefs.preview_flicker_accepted);
        assert!(!prefs.notifications_muted);
    }

    #[test]
    fn versionless_prefs_migrate_to_current_version() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(
            paths.prefs_file(),
            "preview_flicker_accepted = true\nnotifications_muted = false\n",
        )
        .unwrap();
        let prefs = Prefs::load(&paths);
        assert_eq!(prefs.version, CURRENT_VERSION);
        assert!(prefs.preview_flicker_accepted);
    }

    #[test]
    fn corrupt_prefs_falls_back_to_defaults() {
        let (_tmp, paths) = fixture_paths();
        std::fs::write(paths.prefs_file(), "[invalid @@").unwrap();
        let prefs = Prefs::load(&paths);
        assert_eq!(prefs.version, CURRENT_VERSION);
        assert!(!prefs.preview_flicker_accepted);
    }

    #[test]
    fn save_round_trips() {
        let (_tmp, paths) = fixture_paths();
        let prefs = Prefs {
            version: CURRENT_VERSION,
            preview_flicker_accepted: true,
            notifications_muted: true,
        };
        prefs.save(&paths);
        let loaded = Prefs::load(&paths);
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert!(loaded.preview_flicker_accepted);
        assert!(loaded.notifications_muted);
    }
}
