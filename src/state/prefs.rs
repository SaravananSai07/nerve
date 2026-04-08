use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
pub struct Prefs {
    #[serde(default)]
    pub preview_flicker_accepted: bool,
    #[serde(default)]
    pub notifications_muted: bool,
}

impl Prefs {
    pub fn load() -> Self {
        prefs_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(path) = prefs_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(content) = toml::to_string_pretty(self) {
                let _ = std::fs::write(path, content);
            }
        }
    }
}

fn prefs_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("nerve").join("prefs.toml"))
}
