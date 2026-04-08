use ratatui::layout::{Constraint, Flex, Layout, Rect};

pub mod cards;
pub mod confirm_kill;
pub mod confirm_preview;
pub mod help;
pub mod preview;
pub mod rename;
pub mod theme;

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .split(area);
    Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}
