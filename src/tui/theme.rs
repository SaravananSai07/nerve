use ratatui::style::Color;

use crate::state::session::SessionState;

pub struct Theme {
    pub name: &'static str,
    pub border: Color,
    pub text: Color,
    pub processing: Color,
    pub waiting: Color,
    pub idle: Color,
    pub error: Color,
    pub stale: Color,
    pub selected_bg: Color,
    pub selected_text: Color,
}

pub const THEME_NAMES: &[&str] = &[
    "nightfox",
    "tokyonight",
    "catppuccin",
    "gruvbox",
    "dracula",
    "rosepine",
];

impl Theme {
    pub fn by_name(name: &str) -> Self {
        match name {
            "tokyonight" | "tokyo-night" => Self::tokyonight(),
            "catppuccin" => Self::catppuccin(),
            "gruvbox" => Self::gruvbox(),
            "dracula" => Self::dracula(),
            "rosepine" | "rose-pine" => Self::rosepine(),
            _ => Self::nightfox(),
        }
    }

    pub fn nightfox() -> Self {
        Self {
            name: "nightfox",
            border: Color::Rgb(0x71, 0x83, 0x9b),
            text: Color::Rgb(0xcd, 0xce, 0xcf),
            processing: Color::Rgb(0x81, 0xb2, 0x9a),
            waiting: Color::Rgb(0xdb, 0xc0, 0x74),
            idle: Color::Rgb(0x63, 0x71, 0x7f),
            error: Color::Rgb(0xc9, 0x4f, 0x6d),
            stale: Color::Rgb(0x50, 0x56, 0x5b),
            selected_bg: Color::Rgb(0x2a, 0x31, 0x3a),
            selected_text: Color::Rgb(0xea, 0xeb, 0xec),
        }
    }

    pub fn tokyonight() -> Self {
        Self {
            name: "tokyonight",
            border: Color::Rgb(0x56, 0x5f, 0x89),
            text: Color::Rgb(0xc0, 0xca, 0xf5),
            processing: Color::Rgb(0x9e, 0xce, 0x6a),
            waiting: Color::Rgb(0xe0, 0xaf, 0x68),
            idle: Color::Rgb(0x54, 0x5c, 0x7e),
            error: Color::Rgb(0xf7, 0x76, 0x8e),
            stale: Color::Rgb(0x41, 0x48, 0x68),
            selected_bg: Color::Rgb(0x29, 0x2e, 0x42),
            selected_text: Color::Rgb(0xd5, 0xdf, 0xff),
        }
    }

    pub fn catppuccin() -> Self {
        // Mocha variant
        Self {
            name: "catppuccin",
            border: Color::Rgb(0x6c, 0x70, 0x86),
            text: Color::Rgb(0xcd, 0xd6, 0xf4),
            processing: Color::Rgb(0xa6, 0xe3, 0xa1),
            waiting: Color::Rgb(0xf9, 0xe2, 0xaf),
            idle: Color::Rgb(0x58, 0x5b, 0x70),
            error: Color::Rgb(0xf3, 0x8b, 0xa8),
            stale: Color::Rgb(0x45, 0x47, 0x5a),
            selected_bg: Color::Rgb(0x31, 0x32, 0x44),
            selected_text: Color::Rgb(0xe2, 0xe8, 0xfa),
        }
    }

    pub fn gruvbox() -> Self {
        Self {
            name: "gruvbox",
            border: Color::Rgb(0x66, 0x5c, 0x54),
            text: Color::Rgb(0xeb, 0xdb, 0xb2),
            processing: Color::Rgb(0xb8, 0xbb, 0x26),
            waiting: Color::Rgb(0xfa, 0xbd, 0x2f),
            idle: Color::Rgb(0x7c, 0x6f, 0x64),
            error: Color::Rgb(0xfb, 0x49, 0x34),
            stale: Color::Rgb(0x50, 0x49, 0x45),
            selected_bg: Color::Rgb(0x3c, 0x38, 0x36),
            selected_text: Color::Rgb(0xf9, 0xf5, 0xd7),
        }
    }

    pub fn dracula() -> Self {
        Self {
            name: "dracula",
            border: Color::Rgb(0x62, 0x72, 0xa4),
            text: Color::Rgb(0xf8, 0xf8, 0xf2),
            processing: Color::Rgb(0x50, 0xfa, 0x7b),
            waiting: Color::Rgb(0xf1, 0xfa, 0x8c),
            idle: Color::Rgb(0x62, 0x72, 0xa4),
            error: Color::Rgb(0xff, 0x55, 0x55),
            stale: Color::Rgb(0x44, 0x47, 0x5a),
            selected_bg: Color::Rgb(0x34, 0x35, 0x46),
            selected_text: Color::Rgb(0xf8, 0xf8, 0xf2),
        }
    }

    pub fn rosepine() -> Self {
        Self {
            name: "rosepine",
            border: Color::Rgb(0x6e, 0x6a, 0x86),
            text: Color::Rgb(0xe0, 0xde, 0xf4),
            processing: Color::Rgb(0x9c, 0xce, 0xd6),
            waiting: Color::Rgb(0xf6, 0xc1, 0x77),
            idle: Color::Rgb(0x52, 0x4f, 0x67),
            error: Color::Rgb(0xeb, 0x6f, 0x92),
            stale: Color::Rgb(0x3e, 0x3c, 0x54),
            selected_bg: Color::Rgb(0x26, 0x23, 0x3a),
            selected_text: Color::Rgb(0xea, 0xe8, 0xf8),
        }
    }

    pub fn state_color(&self, state: &SessionState) -> Color {
        match state {
            SessionState::Processing => self.processing,
            SessionState::ToolRunning(_) => self.processing,
            SessionState::WaitingForInput => self.waiting,
            SessionState::WaitingForPermission => self.waiting,
            SessionState::Idle => self.idle,
            SessionState::Error => self.error,
            SessionState::Stale => self.stale,
        }
    }

    pub fn state_indicator(&self, state: &SessionState) -> &'static str {
        match state {
            SessionState::Processing => "●",
            SessionState::ToolRunning(_) => "◉",
            SessionState::WaitingForInput => "○",
            SessionState::WaitingForPermission => "◌",
            SessionState::Idle => "◌",
            SessionState::Error => "✕",
            SessionState::Stale => "⠿",
        }
    }
}
