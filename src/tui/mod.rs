use ratatui::layout::{Constraint, Flex, Layout, Rect};

pub(crate) mod cards;
pub(crate) mod confirm_kill;
pub(crate) mod confirm_preview;
pub(crate) mod help;
pub(crate) mod preview;
pub(crate) mod rename;
pub(crate) mod status;
pub(crate) mod theme;

pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    // Clamp to the available area so a request larger than the
    // current frame returns a Rect that still fits (the overlay box
    // will be smaller than asked-for, but it won't clip off-screen
    // with half-drawn borders).
    let width = width.min(area.width);
    let height = height.min(area.height);
    let vertical = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .split(area);
    Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}
