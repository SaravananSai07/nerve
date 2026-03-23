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
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub session_names: HashMap<String, String>,
}

#[derive(Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_refresh")]
    pub refresh_interval_ms: u64,
    #[serde(default = "default_process_scan")]
    pub process_scan_interval_ms: u64,
    #[serde(default = "default_terminal")]
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

fn default_refresh() -> u64 { 1000 }
fn default_process_scan() -> u64 { 5000 }
fn default_terminal() -> String { "auto".into() }

#[derive(Deserialize)]
pub struct AppearanceConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_layout")]
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

fn default_theme() -> String { "nightfox".into() }
fn default_layout() -> String { "cards".into() }

#[derive(Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "default_true")]
    pub on_waiting: bool,
    #[serde(default = "default_true")]
    pub on_error: bool,
    #[serde(default = "default_sound")]
    pub sound: String,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            on_waiting: true,
            on_error: true,
            sound: "bell".into(),
        }
    }
}

fn default_true() -> bool { true }
fn default_sound() -> String { "bell".into() }

impl Config {
    pub fn load() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn session_name_for(&self, cwd: &str) -> Option<&String> {
        self.session_names.get(cwd)
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("nerve").join("config.toml"))
}
