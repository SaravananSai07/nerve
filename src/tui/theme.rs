use std::path::Path;

use ratatui::style::Color;
use serde::Deserialize;

use crate::log_warn;
use crate::util::sanitize::Sanitised;

const THEME_MAX_BYTES: u64 = 64 * 1024;

/// Four colour roles on top of the terminal's own background. Amber is
/// for what needs you and blue for where you are; everything secondary
/// is dim. Shape and words carry the rest.
#[derive(Debug, Clone)]
pub(crate) struct Theme {
    pub(crate) name: Sanitised,
    pub(crate) text: Color,
    pub(crate) dim: Color,
    pub(crate) attention: Color,
    pub(crate) accent: Color,
    /// Behind the row under the cursor. None leaves the terminal's own
    /// background, which is the only safe choice when it may be light
    /// or dark.
    pub(crate) selection: Option<Color>,
    /// Behind a row that just arrived or was landed on.
    pub(crate) flash: Option<Color>,
}

/// Built-in themes in cycle order. User themes from
/// `~/.config/nerve/themes/*.toml` are appended at runtime.
const BUILTIN_NAMES: &[&str] = &["terminal", "nightfox", "tokyonight", "catppuccin", "gruvbox", "dracula", "rosepine"];

impl Theme {
    /// Every built-in theme, followed by user themes from `themes_dir`
    /// sorted by name, so a configured name always lands on one index.
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
        let raw: RawTheme = crate::util::fs::load_toml(path, THEME_MAX_BYTES)?.ok_or("file vanished")?;
        let fallback_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("custom").to_string();
        // "custom" when sanitising empties the name or it is long enough
        // to crowd the settings line.
        let name = {
            let s = Sanitised::new(raw.name.unwrap_or(fallback_name));
            if s.is_empty() || s.len() > 64 {
                log_warn!(
                    "themes: name in {} rejected (empty after sanitise or >64 B), using 'custom'",
                    path.display()
                );
                Sanitised::new("custom")
            } else {
                s
            }
        };
        let optional = |v: &Option<String>, field| v.as_deref().map(|s| parse_color(s, field)).transpose();
        Ok(Theme {
            name,
            text: parse_color(&raw.text, "text")?,
            dim: parse_color(&raw.dim, "dim")?,
            attention: parse_color(&raw.attention, "attention")?,
            accent: parse_color(&raw.accent, "accent")?,
            selection: optional(&raw.selection, "selection")?,
            flash: optional(&raw.flash, "flash")?,
        })
    }

    fn by_name(name: &str) -> Self {
        let rgb = |hex: u32| Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
        let (text, dim, attention, accent, selection, flash) = match name {
            "nightfox" => (0xcdcecf, 0x8a929c, 0xdbc074, 0x719cd6, 0x2a313a, 0x3a3424),
            "tokyonight" | "tokyo-night" => (0xc0caf5, 0x7c84ad, 0xe0af68, 0x7aa2f7, 0x292e42, 0x3b3426),
            "catppuccin" => (0xcdd6f4, 0x8a8fa8, 0xf9e2af, 0x89b4fa, 0x313244, 0x3e3528),
            "gruvbox" => (0xebdbb2, 0xa89984, 0xfabd2f, 0x83a598, 0x3c3836, 0x4a3b25),
            "dracula" => (0xf8f8f2, 0x9ba5c8, 0xf1fa8c, 0xbd93f9, 0x343546, 0x44402a),
            "rosepine" | "rose-pine" => (0xe0def4, 0x908cae, 0xf6c177, 0xc4a7e7, 0x26233a, 0x3d3127),
            // The terminal's own palette, so it follows a light or dark
            // terminal theme without asking.
            _ => {
                return Self {
                    name: Sanitised::new("terminal"),
                    text: Color::Reset,
                    dim: Color::DarkGray,
                    attention: Color::Yellow,
                    accent: Color::Blue,
                    selection: None,
                    flash: None,
                }
            }
        };
        Self {
            name: Sanitised::new(name),
            text: rgb(text),
            dim: rgb(dim),
            attention: rgb(attention),
            accent: rgb(accent),
            selection: Some(rgb(selection)),
            flash: Some(rgb(flash)),
        }
    }
}

/// A user theme. Older files name the roles after session states; those
/// keys still load.
#[derive(Deserialize)]
struct RawTheme {
    name: Option<String>,
    text: String,
    #[serde(alias = "muted")]
    dim: String,
    #[serde(alias = "waiting")]
    attention: String,
    #[serde(alias = "processing")]
    accent: String,
    #[serde(default, alias = "selected_bg")]
    selection: Option<String>,
    #[serde(default)]
    flash: Option<String>,
}

fn parse_color(s: &str, field: &str) -> Result<Color, String> {
    let trimmed = s.trim().trim_start_matches('#');
    if trimmed.len() != 6 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{field}: expected '#RRGGBB', got {s:?}"));
    }
    let n = u32::from_str_radix(trimmed, 16).map_err(|_| format!("{field}: expected '#RRGGBB', got {s:?}"))?;
    Ok(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_accepts_either_prefix_and_rejects_junk() {
        assert_eq!(parse_color("#11aabb", "t").unwrap(), Color::Rgb(0x11, 0xaa, 0xbb));
        assert_eq!(parse_color("11aabb", "t").unwrap(), Color::Rgb(0x11, 0xaa, 0xbb));
        assert!(parse_color("#abc", "t").is_err());
        assert!(parse_color("#zzzzzz", "t").is_err());
        // Six bytes, but not six hex digits: must not slice mid-character.
        assert!(parse_color("ab€d", "t").is_err());
    }

    #[test]
    fn user_theme_with_role_names() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("inkpot.toml");
        std::fs::write(
            &path,
            "name = \"inkpot\"\ntext = \"#aabbcc\"\ndim = \"#666666\"\nattention = \"#aaaa11\"\naccent = \"#1166aa\"\n",
        )
        .unwrap();
        let theme = Theme::from_toml(&path).expect("parse");
        assert_eq!(theme.name, "inkpot");
        assert_eq!(theme.accent, Color::Rgb(0x11, 0x66, 0xaa));
        assert_eq!(theme.selection, None);
    }

    // Themes written for the old layout keep loading: their state
    // colours map onto the roles, and the keys with no role are ignored.
    #[test]
    fn older_user_theme_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("autoname.toml");
        std::fs::write(
            &path,
            "border = \"#445566\"\ntext = \"#aabbcc\"\nprocessing = \"#11aa11\"\nwaiting = \"#aaaa11\"\nidle = \"#666666\"\nmuted = \"#777777\"\nerror = \"#aa1111\"\nstale = \"#333333\"\nselected_bg = \"#1a1a2a\"\nselected_text = \"#ffffff\"\n",
        )
        .unwrap();
        let theme = Theme::from_toml(&path).expect("parse");
        assert_eq!(theme.name, "autoname");
        assert_eq!(theme.attention, Color::Rgb(0xaa, 0xaa, 0x11));
        assert_eq!(theme.selection, Some(Color::Rgb(0x1a, 0x1a, 0x2a)));
    }

    #[test]
    fn catalog_starts_with_the_terminal_palette_then_user_themes_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["zeta", "alpha"] {
            std::fs::write(
                tmp.path().join(format!("{name}.toml")),
                "text = \"#aabbcc\"\ndim = \"#666666\"\nattention = \"#aaaa11\"\naccent = \"#1166aa\"\n",
            )
            .unwrap();
        }
        let cat = Theme::catalog(Some(tmp.path()));
        assert_eq!(cat[0].name, "terminal");
        assert_eq!(cat.len(), BUILTIN_NAMES.len() + 2);
        assert_eq!(cat[BUILTIN_NAMES.len()].name, "alpha");
        assert_eq!(cat[BUILTIN_NAMES.len() + 1].name, "zeta");
    }
}
