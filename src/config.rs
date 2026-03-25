use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub session_names: HashMap<String, String>,
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
    pub layout: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: "nightfox".into(),
            layout: "cards".into(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let mut config: Self = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        config.general.refresh_interval_ms = config.general.refresh_interval_ms.clamp(100, 30_000);
        config
    }

    pub fn session_name_for(&self, cwd: &str) -> Option<&String> {
        self.session_names.get(cwd)
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("nerve").join("config.toml"))
}
