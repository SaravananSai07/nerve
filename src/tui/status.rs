use ratatui::style::Color;

use super::theme::Theme;

/// Status-bar message vocabulary. Lives in the TUI layer because
/// the data is pure view-data (text + a colour intent) — the prior
/// home in `app.rs` forced `tui/cards.rs` to reach back up across
/// the layer boundary just to render the message.
#[derive(Clone, Copy)]
enum StatusKind {
    Info,
    Success,
    Error,
}

pub(crate) struct StatusMessage {
    kind: StatusKind,
    pub(crate) text: String,
}

impl StatusMessage {
    pub(crate) fn info(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Info, text: text.into() }
    }

    pub(crate) fn success(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Success, text: text.into() }
    }

    pub(crate) fn error(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Error, text: text.into() }
    }

    /// Render colour for this message under the given theme. Keeps
    /// the StatusKind → Color mapping in one place so a second
    /// renderer (e.g. a future preview-overlay banner) doesn't need
    /// to import the enum to match on it.
    pub(crate) fn color(&self, theme: &Theme) -> Color {
        match self.kind {
            StatusKind::Info => theme.text,
            StatusKind::Success => theme.processing,
            StatusKind::Error => theme.error,
        }
    }
}
