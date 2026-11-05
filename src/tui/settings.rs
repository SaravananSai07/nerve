//! The Settings page: devices (and connecting another), what stays
//! private here, who gets which notifications, and the colour theme.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use super::paint::para;
use super::paint::{paint, window, Ink, Ln};
use super::theme::Theme;

pub(crate) struct DeviceLine {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) online: bool,
    /// `This device`, `Online`, `Last seen 2h ago`.
    pub(crate) status: String,
}

pub(crate) struct PrivateLine {
    pub(crate) label: String,
    pub(crate) folder: bool,
}

pub(crate) struct NotifyLine {
    pub(crate) device: String,
    pub(crate) note: &'static str,
    pub(crate) attention: bool,
    pub(crate) finished: bool,
}

/// What the cursor can rest on, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Device(usize),
    Connect,
    Private(usize),
    Notify(usize),
    Theme,
}

#[derive(Default)]
pub(crate) struct SettingsView {
    pub(crate) me: String,
    /// Sync is set up: there are other devices and private sessions.
    pub(crate) sync: bool,
    pub(crate) devices: Vec<DeviceLine>,
    pub(crate) private: Vec<PrivateLine>,
    pub(crate) notify: Vec<NotifyLine>,
    pub(crate) theme: String,
    pub(crate) connection: Option<String>,
    pub(crate) cursor: usize,
    /// 0: Needs you, 1: Finished turns.
    pub(crate) col: u8,
}

impl SettingsView {
    pub(crate) fn items(&self) -> Vec<Item> {
        let mut v: Vec<Item> = (0..self.devices.len()).map(Item::Device).collect();
        v.push(Item::Connect);
        v.extend((0..self.private.len()).map(Item::Private));
        v.extend((0..self.notify.len()).map(Item::Notify));
        v.push(Item::Theme);
        v
    }

    pub(crate) fn item(&self) -> Option<Item> {
        self.items().get(self.cursor).copied()
    }
}

pub(crate) fn render(buf: &mut Buffer, area: Rect, s: &SettingsView, ink: &Ink, theme: &Theme) {
    let w = area.width;
    let cur = s.item();
    let select = |l: &mut Ln, item: Item| {
        if cur == Some(item) {
            l.select(ink, theme.selection);
        }
    };
    // Columns sit past the longest device name.
    let name_w = s
        .devices
        .iter()
        .map(|d| super::paint::width(&d.name))
        .chain(s.notify.iter().map(|n| super::paint::width(&n.device)))
        .max()
        .unwrap_or(0);
    let kind_x = 6 + name_w + 3;
    let note_x = 3 + name_w + 3;
    let need_x = note_x + 15;
    let done_x = need_x + 14;
    let mut lines = vec![Ln::text(3, "Settings", ink.bold), Ln::new()];

    lines.push(Ln::text(3, "Devices", ink.dim));
    for (i, d) in s.devices.iter().enumerate() {
        let mut l = Ln::new();
        l.put(3, if d.online { "●" } else { "○" }, if d.online { ink.text } else { ink.dim });
        l.put(6, d.name.as_str(), if d.name == s.me { ink.bold } else { ink.text });
        l.put(kind_x, d.kind.as_str(), ink.dim);
        l.rput(w - 2, d.status.as_str(), ink.dim);
        select(&mut l, Item::Device(i));
        lines.push(l);
    }
    let mut c = Ln::new();
    c.put(3, "+", ink.accent);
    c.put(6, "Connect a device", if cur == Some(Item::Connect) { ink.bold } else { ink.text });
    select(&mut c, Item::Connect);
    lines.push(c);
    if !s.sync {
        lines.extend(para(
            "Only this device so far. Connect another to see and answer its sessions here.",
            3,
            w - 6,
            ink.dim,
        ));
    }

    if s.sync {
        lines.push(Ln::new());
        lines.push(Ln::text(3, format!("Private on {}", s.me), ink.dim));
        if s.private.is_empty() {
            lines.extend(para(
                "Nothing. Press p on a session to keep it, or its folder, on this device.",
                3,
                w - 6,
                ink.dim,
            ));
        }
        for (i, p) in s.private.iter().enumerate() {
            let mut l = Ln::new();
            l.put(3, "⊡", ink.dim);
            l.fit(6, &p.label, 40, ink.text);
            l.put(48, if p.folder { "Folder" } else { "Session" }, ink.dim);
            if cur == Some(Item::Private(i)) {
                let x = l.rput(w - 2, "Share again", ink.dim);
                l.put(x - 2, "u", ink.text);
            }
            select(&mut l, Item::Private(i));
            lines.push(l);
        }
        if !s.private.is_empty() {
            lines.extend(para(
                "Your other devices never see these sessions, and their notifications stay here.",
                3,
                w - 6,
                ink.dim,
            ));
        }
    }

    lines.push(Ln::new());
    let mut head = Ln::text(3, "Notify on", ink.dim);
    head.put(need_x, "Needs you", ink.dim);
    head.put(done_x, "Finished turns", ink.dim);
    lines.push(head);
    for (i, n) in s.notify.iter().enumerate() {
        let mut l = Ln::new();
        let sel = cur == Some(Item::Notify(i));
        l.put(3, n.device.as_str(), ink.text);
        l.put(note_x, n.note, ink.dim);
        let mut check = |x: u16, on: bool, col: u8| {
            let focus = sel && s.col == col;
            let frame = if focus { ink.accent } else { ink.dim };
            l.put(x, "[", frame);
            l.put(
                x + 1,
                if on { "×" } else { " " },
                if focus { ink.accent.add_modifier(Modifier::BOLD) } else { ink.text },
            );
            l.put(x + 2, "]", frame);
        };
        check(need_x + 3, n.attention, 0);
        check(done_x + 3, n.finished, 1);
        select(&mut l, Item::Notify(i));
        lines.push(l);
    }
    lines.extend(para(
        "Questions, permission requests and failures notify the devices ticked under Needs you. Every session's finished turn notifies the devices ticked under Finished turns, except the one you have open in nerve. Press n on a session to choose for it alone.",
        3,
        w - 6,
        ink.dim,
    ));

    lines.push(Ln::new());
    let mut t = Ln::text(3, "Theme", ink.dim);
    t.put(16, s.theme.as_str(), ink.text);
    if cur == Some(Item::Theme) {
        let x = t.rput(w - 2, "Change", ink.dim);
        t.put(x - 4, "← →", ink.text);
    }
    select(&mut t, Item::Theme);
    lines.push(t);

    if let Some(c) = &s.connection {
        lines.push(Ln::new());
        lines.push(Ln::text(3, "Connection", ink.dim));
        lines.extend(para(c, 3, w - 6, ink.dim));
    }
    paint(buf, area, &window(lines, usize::from(area.height)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_can_be_chosen() {
        let theme = &Theme::catalog(None)[0];
        let line =
            |name: &str| DeviceLine { name: name.into(), kind: "Linux".into(), online: true, status: "Online".into() };
        let s = SettingsView {
            me: "laptop".into(),
            sync: true,
            devices: vec![line("laptop"), line("pi")],
            cursor: 1,
            ..Default::default()
        };
        assert_eq!(s.items()[..3], [Item::Device(0), Item::Device(1), Item::Connect]);
        let area = Rect::new(0, 0, 110, 20);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &s, &Ink::new(theme), theme);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(text.contains("+  Connect a device"), "{text}");
    }
}
