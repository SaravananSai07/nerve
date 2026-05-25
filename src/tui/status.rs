/// Status-bar message vocabulary. Lives in the TUI layer because
/// the data is pure view-data (text + a colour intent) — the prior
/// home in `app.rs` forced `tui/cards.rs` to reach back up across
/// the layer boundary just to render the message.
#[derive(Clone, Copy)]
pub enum StatusKind {
    Info,
    Success,
    Error,
}

pub struct StatusMessage {
    pub kind: StatusKind,
    pub text: String,
}

impl StatusMessage {
    pub fn info(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Info, text: text.into() }
    }

    pub fn success(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Success, text: text.into() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Error, text: text.into() }
    }
}
