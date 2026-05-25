use std::path::Path;

use ratatui::style::Color;
use serde::Deserialize;

use crate::log_warn;
use crate::state::session::SessionState;

const THEME_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Theme {
    pub(crate) name: String,
    pub(crate) border: Color,
    pub(crate) text: Color,
    pub(crate) processing: Color,
    pub(crate) waiting: Color,
    /// Idle-session state color. Deliberately dim so an idle card
    /// fades into the background. Don't use this for chrome text —
    /// `muted` exists for that.
    pub(crate) idle: Color,
    /// Chrome text (status-bar hints, secondary card text, help
    /// section dividers). Distinct from `idle` so a session-state
    /// color choice doesn't drag UI affordances below WCAG-AA
    /// contrast on dark backgrounds.
    pub(crate) muted: Color,
    pub(crate) error: Color,
    pub(crate) stale: Color,
    pub(crate) selected_bg: Color,
    pub(crate) selected_text: Color,
}

/// Built-in themes in cycle order. User themes from
/// `~/.config/nerve/themes/*.toml` are appended at runtime.
const BUILTIN_NAMES: &[&str] = &[
    "nightfox",
    "tokyonight",
    "catppuccin",
    "gruvbox",
    "dracula",
    "rosepine",
];

impl Theme {
    /// Build the cycle list: every built-in theme, followed by any
    /// user themes loaded from `themes_dir`. The cycle order is
    /// deterministic (alphabetical by filename within the user
    /// section) so launching with a particular `theme` config value
    /// always lands on the same index.
    pub(crate) fn catalog(themes_dir: Option<&Path>) -> Vec<Theme> {
        let mut catalog: Vec<Theme> = BUILTIN_NAMES.iter().map(|n| Self::by_name(n)).collect();
        if let Some(dir) = themes_dir {
            let mut user = Self::load_user_themes(dir);
            user.sort_by(|a, b| a.name.cmp(&b.name));
            catalog.extend(user);
        }
        catalog
    }

    fn load_user_themes(dir: &Path) -> Vec<Theme> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                log_warn!("themes: cannot read {}: {e}", dir.display());
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml") {
                match Self::from_toml(&path) {
                    Ok(theme) => out.push(theme),
                    Err(e) => log_warn!("themes: skipping {}: {e}", path.display()),
                }
            }
        }
        out
    }

    fn from_toml(path: &Path) -> Result<Theme, String> {
        let contents = crate::util::fs::read_capped(path, THEME_MAX_BYTES).map_err(|e| e.to_string())?;
        let raw: RawTheme = toml::from_str(&contents).map_err(|e| e.to_string())?;
        let fallback_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("custom")
            .to_string();
        let name = raw.name.unwrap_or(fallback_name);
        let idle = parse_color(&raw.idle, "idle")?;
        // muted falls back to idle so existing user themes don't
        // suddenly fail to load — the chrome stays at its prior
        // (slightly-dim) color until the user adopts the new field.
        // Log once per load so a theme author editing the TOML can
        // see why the chrome looks under-contrast without grepping
        // the CHANGELOG.
        let muted = match raw.muted.as_deref() {
            Some(s) => parse_color(s, "muted")?,
            None => {
                log_warn!(
                    "themes: {} has no `muted` colour — falling back to `idle`. \
                     Add a `muted = \"#RRGGBB\"` line for distinct chrome text.",
                    name
                );
                idle
            }
        };
        Ok(Theme {
            name,
            border: parse_color(&raw.border, "border")?,
            text: parse_color(&raw.text, "text")?,
            processing: parse_color(&raw.processing, "processing")?,
            waiting: parse_color(&raw.waiting, "waiting")?,
            idle,
            muted,
            error: parse_color(&raw.error, "error")?,
            stale: parse_color(&raw.stale, "stale")?,
            selected_bg: parse_color(&raw.selected_bg, "selected_bg")?,
            selected_text: parse_color(&raw.selected_text, "selected_text")?,
        })
    }

    pub(crate) fn by_name(name: &str) -> Self {
        match name {
            "tokyonight" | "tokyo-night" => Self::tokyonight(),
            "catppuccin" => Self::catppuccin(),
            "gruvbox" => Self::gruvbox(),
            "dracula" => Self::dracula(),
            "rosepine" | "rose-pine" => Self::rosepine(),
            _ => Self::nightfox(),
        }
    }

    pub(crate) fn nightfox() -> Self {
        Self {
            name: "nightfox".into(),
            border: Color::Rgb(0x71, 0x83, 0x9b),
            text: Color::Rgb(0xcd, 0xce, 0xcf),
            processing: Color::Rgb(0x81, 0xb2, 0x9a),
            waiting: Color::Rgb(0xdb, 0xc0, 0x74),
            idle: Color::Rgb(0x63, 0x71, 0x7f),
            muted: Color::Rgb(0x90, 0x98, 0xa4),
            error: Color::Rgb(0xc9, 0x4f, 0x6d),
            stale: Color::Rgb(0x50, 0x56, 0x5b),
            selected_bg: Color::Rgb(0x2a, 0x31, 0x3a),
            selected_text: Color::Rgb(0xea, 0xeb, 0xec),
        }
    }

    pub(crate) fn tokyonight() -> Self {
        Self {
            name: "tokyonight".into(),
            border: Color::Rgb(0x56, 0x5f, 0x89),
            text: Color::Rgb(0xc0, 0xca, 0xf5),
            processing: Color::Rgb(0x9e, 0xce, 0x6a),
            waiting: Color::Rgb(0xe0, 0xaf, 0x68),
            idle: Color::Rgb(0x54, 0x5c, 0x7e),
            muted: Color::Rgb(0x82, 0x8b, 0xb8),
            error: Color::Rgb(0xf7, 0x76, 0x8e),
            stale: Color::Rgb(0x41, 0x48, 0x68),
            selected_bg: Color::Rgb(0x29, 0x2e, 0x42),
            selected_text: Color::Rgb(0xd5, 0xdf, 0xff),
        }
    }

    pub(crate) fn catppuccin() -> Self {
        // Mocha variant
        Self {
            name: "catppuccin".into(),
            border: Color::Rgb(0x6c, 0x70, 0x86),
            text: Color::Rgb(0xcd, 0xd6, 0xf4),
            processing: Color::Rgb(0xa6, 0xe3, 0xa1),
            waiting: Color::Rgb(0xf9, 0xe2, 0xaf),
            idle: Color::Rgb(0x58, 0x5b, 0x70),
            muted: Color::Rgb(0x8a, 0x8f, 0xa8),
            error: Color::Rgb(0xf3, 0x8b, 0xa8),
            stale: Color::Rgb(0x45, 0x47, 0x5a),
            selected_bg: Color::Rgb(0x31, 0x32, 0x44),
            selected_text: Color::Rgb(0xe2, 0xe8, 0xfa),
        }
    }

    pub(crate) fn gruvbox() -> Self {
        Self {
            name: "gruvbox".into(),
            border: Color::Rgb(0x66, 0x5c, 0x54),
            text: Color::Rgb(0xeb, 0xdb, 0xb2),
            processing: Color::Rgb(0xb8, 0xbb, 0x26),
            waiting: Color::Rgb(0xfa, 0xbd, 0x2f),
            idle: Color::Rgb(0x7c, 0x6f, 0x64),
            muted: Color::Rgb(0xa8, 0x99, 0x84),
            error: Color::Rgb(0xfb, 0x49, 0x34),
            stale: Color::Rgb(0x50, 0x49, 0x45),
            selected_bg: Color::Rgb(0x3c, 0x38, 0x36),
            selected_text: Color::Rgb(0xf9, 0xf5, 0xd7),
        }
    }

    pub(crate) fn dracula() -> Self {
        Self {
            name: "dracula".into(),
            border: Color::Rgb(0x62, 0x72, 0xa4),
            text: Color::Rgb(0xf8, 0xf8, 0xf2),
            processing: Color::Rgb(0x50, 0xfa, 0x7b),
            waiting: Color::Rgb(0xf1, 0xfa, 0x8c),
            idle: Color::Rgb(0x62, 0x72, 0xa4),
            muted: Color::Rgb(0x9b, 0xa5, 0xc8),
            error: Color::Rgb(0xff, 0x55, 0x55),
            stale: Color::Rgb(0x44, 0x47, 0x5a),
            selected_bg: Color::Rgb(0x34, 0x35, 0x46),
            selected_text: Color::Rgb(0xf8, 0xf8, 0xf2),
        }
    }

    pub(crate) fn rosepine() -> Self {
        Self {
            name: "rosepine".into(),
            border: Color::Rgb(0x6e, 0x6a, 0x86),
            text: Color::Rgb(0xe0, 0xde, 0xf4),
            processing: Color::Rgb(0x9c, 0xce, 0xd6),
            waiting: Color::Rgb(0xf6, 0xc1, 0x77),
            idle: Color::Rgb(0x52, 0x4f, 0x67),
            muted: Color::Rgb(0x90, 0x8c, 0xae),
            error: Color::Rgb(0xeb, 0x6f, 0x92),
            stale: Color::Rgb(0x3e, 0x3c, 0x54),
            selected_bg: Color::Rgb(0x26, 0x23, 0x3a),
            selected_text: Color::Rgb(0xea, 0xe8, 0xf8),
        }
    }

    pub(crate) fn state_color(&self, state: &SessionState) -> Color {
        match state {
            SessionState::Processing => self.processing,
            SessionState::ToolRunning(_) => self.processing,
            SessionState::WaitingForInput => self.waiting,
            SessionState::Idle => self.idle,
            SessionState::Error => self.error,
            SessionState::Dormant | SessionState::Vanished => self.stale,
        }
    }

    pub(crate) fn state_indicator(&self, state: &SessionState) -> &'static str {
        match state {
            SessionState::Processing => "●",
            SessionState::ToolRunning(_) => "◉",
            SessionState::WaitingForInput => "○",
            SessionState::Idle => "◌",
            SessionState::Error => "✕",
            SessionState::Dormant => "⠿",
            SessionState::Vanished => "⊘",
        }
    }
}

#[derive(Deserialize)]
struct RawTheme {
    name: Option<String>,
    border: String,
    text: String,
    processing: String,
    waiting: String,
    idle: String,
    /// Optional — older user themes without this key fall back to
    /// `idle`. Documented in CHANGELOG.
    #[serde(default)]
    muted: Option<String>,
    error: String,
    stale: String,
    selected_bg: String,
    selected_text: String,
}

fn parse_color(s: &str, field: &str) -> Result<Color, String> {
    let trimmed = s.trim().trim_start_matches('#');
    if trimmed.len() != 6 {
        return Err(format!("{field}: expected '#RRGGBB', got {s:?}"));
    }
    let r = u8::from_str_radix(&trimmed[0..2], 16)
        .map_err(|_| format!("{field}: bad red component"))?;
    let g = u8::from_str_radix(&trimmed[2..4], 16)
        .map_err(|_| format!("{field}: bad green component"))?;
    let b = u8::from_str_radix(&trimmed[4..6], 16)
        .map_err(|_| format!("{field}: bad blue component"))?;
    Ok(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_accepts_hash_prefix() {
        assert_eq!(
            parse_color("#11aabb", "test").unwrap(),
            Color::Rgb(0x11, 0xaa, 0xbb)
        );
    }

    #[test]
    fn parse_color_accepts_no_prefix() {
        assert_eq!(
            parse_color("11aabb", "test").unwrap(),
            Color::Rgb(0x11, 0xaa, 0xbb)
        );
    }

    #[test]
    fn parse_color_rejects_short() {
        assert!(parse_color("#abc", "test").is_err());
    }

    #[test]
    fn parse_color_rejects_non_hex() {
        assert!(parse_color("#zzzzzz", "test").is_err());
    }

    #[test]
    fn from_toml_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("inkpot.toml");
        std::fs::write(
            &path,
            r##"name = "inkpot"
border = "#445566"
text = "#aabbcc"
processing = "#11aa11"
waiting = "#aaaa11"
idle = "#666666"
error = "#aa1111"
stale = "#333333"
selected_bg = "#1a1a2a"
selected_text = "#ffffff"
"##,
        )
        .unwrap();
        let theme = Theme::from_toml(&path).expect("parse");
        assert_eq!(theme.name, "inkpot");
        assert_eq!(theme.border, Color::Rgb(0x44, 0x55, 0x66));
        assert_eq!(theme.text, Color::Rgb(0xaa, 0xbb, 0xcc));
    }

    #[test]
    fn from_toml_uses_file_stem_when_name_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("autoname.toml");
        std::fs::write(
            &path,
            r##"border = "#445566"
text = "#aabbcc"
processing = "#11aa11"
waiting = "#aaaa11"
idle = "#666666"
error = "#aa1111"
stale = "#333333"
selected_bg = "#1a1a2a"
selected_text = "#ffffff"
"##,
        )
        .unwrap();
        let theme = Theme::from_toml(&path).expect("parse");
        assert_eq!(theme.name, "autoname");
    }

    #[test]
    fn catalog_includes_builtins() {
        let cat = Theme::catalog(None);
        assert_eq!(cat.len(), BUILTIN_NAMES.len());
        assert_eq!(cat[0].name, "nightfox");
    }

    #[test]
    fn catalog_appends_user_themes_alphabetically() {
        let tmp = tempfile::tempdir().unwrap();
        for (file, name) in [("zeta.toml", "zeta"), ("alpha.toml", "alpha")] {
            std::fs::write(
                tmp.path().join(file),
                format!(
                    r##"name = "{name}"
border = "#445566"
text = "#aabbcc"
processing = "#11aa11"
waiting = "#aaaa11"
idle = "#666666"
error = "#aa1111"
stale = "#333333"
selected_bg = "#1a1a2a"
selected_text = "#ffffff"
"##
                ),
            )
            .unwrap();
        }
        let cat = Theme::catalog(Some(tmp.path()));
        assert_eq!(cat.len(), BUILTIN_NAMES.len() + 2);
        assert_eq!(cat[BUILTIN_NAMES.len()].name, "alpha");
        assert_eq!(cat[BUILTIN_NAMES.len() + 1].name, "zeta");
    }
}
