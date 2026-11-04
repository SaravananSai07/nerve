//! Rendering. `draw` takes a `View` (everything one frame shows, gathered
//! by the app) and paints it: the top bar, the page, the bottom line and
//! any open dialog. Nothing here does I/O or keeps state.

use ratatui::layout::Rect;
use ratatui::Frame;

pub(crate) mod detail;
pub(crate) mod dialog;
mod markdown;
pub(crate) mod model;
pub(crate) mod paint;
pub(crate) mod sessions;
pub(crate) mod settings;
pub(crate) mod sync;
pub(crate) mod theme;

use model::{Band, Row, RowKey, Said};
use paint::{paint, Ink, Ln};
use theme::Theme;

/// Below this width the session panel takes the whole screen.
pub(crate) const SPLIT_AT: u16 = 110;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Page {
    #[default]
    Sessions,
    Sync,
    Settings,
}

impl Page {
    pub(crate) const ALL: [Page; 3] = [Page::Sessions, Page::Sync, Page::Settings];

    fn title(self) -> &'static str {
        match self {
            Self::Sessions => "Sessions",
            Self::Sync => "Sync",
            Self::Settings => "Settings",
        }
    }
}

/// The bottom line: what the keys do now, or a message, or a question.
pub(crate) enum Footer {
    Hints(Vec<(&'static str, String)>),
    Toast(String),
    Confirm { text: String, hints: Vec<(&'static str, String)> },
    Find(String),
}

/// What the session panel shows besides the row itself.
pub(crate) struct DetailView<'a> {
    /// None while a local transcript is still being read.
    pub(crate) transcript: Option<&'a [Said]>,
    /// Lines scrolled up from the newest; the panel clamps it to what
    /// there is.
    pub(crate) scroll: &'a std::cell::Cell<usize>,
    /// A captured screen, shown instead of the transcript.
    pub(crate) screen: Option<&'a [String]>,
    /// The prompt being typed, when the composer has focus.
    pub(crate) compose: Option<&'a str>,
    /// `Sending to studio…` / `Sent to studio` after an answer.
    pub(crate) sending: Option<&'a str>,
    /// Can a prompt be sent from here at all (sync running).
    pub(crate) can_compose: bool,
}

/// What the page other than Sessions shows, built only while it is open.
pub(crate) enum Body<'a> {
    Sessions,
    Sync(sync::SyncPage<'a>),
    Settings(settings::SettingsView),
}

pub(crate) struct View<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) page: Page,
    pub(crate) me: &'a str,
    /// Devices online besides this one; None before sync is set up.
    pub(crate) others_online: Option<usize>,
    pub(crate) rows: &'a [Row],
    pub(crate) bands: &'a [(Band, Vec<&'a Row>)],
    pub(crate) cur: Option<&'a RowKey>,
    pub(crate) open: bool,
    pub(crate) find: Option<&'a str>,
    /// A row to light up, and whether it is in its second, fainter beat.
    pub(crate) flash: Option<(&'a RowKey, bool)>,
    /// The spinner frame: it turns about ten times a second.
    pub(crate) spin: usize,
    pub(crate) detail: DetailView<'a>,
    pub(crate) body: Body<'a>,
    pub(crate) footer: Footer,
    pub(crate) dialog: Option<&'a dialog::Dialog>,
}

pub(crate) fn draw(frame: &mut Frame, v: &View) {
    let area = frame.area();
    if area.height < 4 || area.width < 20 {
        return;
    }
    let ink = Ink::new(v.theme);
    let buf = frame.buffer_mut();
    paint(buf, Rect { height: 1, ..area }, &[top_bar(v, &ink, area.width)]);
    let body = Rect { x: area.x, y: area.y + 2, width: area.width, height: area.height - 4 };
    match &v.body {
        Body::Sessions => sessions::render(buf, body, v, &ink),
        Body::Sync(p) => sync::render(buf, body, p, &ink, v.theme),
        Body::Settings(s) => settings::render(buf, body, s, &ink, v.theme),
    }
    let foot = Rect { y: area.y + area.height - 1, height: 1, ..area };
    paint(buf, foot, &[footer(&v.footer, &ink, area.width)]);
    if let Some(d) = v.dialog {
        dialog::render(buf, area, d, &ink);
    }
}

fn top_bar(v: &View, ink: &Ink, w: u16) -> Ln {
    let mut l = Ln::new();
    l.put(1, "nerve", ink.bold);
    let mut x = 10;
    for p in Page::ALL {
        let style = if p == v.page { ink.bold.add_modifier(ratatui::style::Modifier::UNDERLINED) } else { ink.dim };
        x = l.put(x, p.title(), style) + 3;
    }
    let mut right = w - 1;
    if let Some(n) = v.others_online {
        let others = match n {
            0 => "No other devices online".to_string(),
            1 => "1 other online".to_string(),
            n => format!("{n} others online"),
        };
        right = l.rput(right, others, ink.dim) - 2;
    }
    right = l.rput(right, v.me, ink.text);
    let need = v.rows.iter().filter(|r| r.status.band() == Band::Need).count();
    if v.page != Page::Sessions && need > 0 {
        l.rput(right.saturating_sub(4), format!("● {need} need you"), ink.attention);
    }
    l
}

/// `key Label` pairs, keys in the text colour and labels dim.
pub(crate) fn hints(l: &mut Ln, mut x: u16, hints: &[(&str, String)], ink: &Ink) -> u16 {
    for (i, (k, label)) in hints.iter().enumerate() {
        if i > 0 {
            x += 3;
        }
        x = l.put(x, *k, ink.text);
        x = l.put(x + 1, label.as_str(), ink.dim);
    }
    x
}

fn footer(f: &Footer, ink: &Ink, w: u16) -> Ln {
    let mut l = Ln::new();
    match f {
        Footer::Hints(h) => {
            hints(&mut l, 1, h, ink);
        }
        Footer::Toast(t) => {
            l.fit(1, t, w - 2, ink.dim);
        }
        Footer::Confirm { text, hints: h } => {
            // The keys must stay visible: a long question gives way.
            let keys: u16 = h.iter().map(|(k, v)| paint::width(k) + 1 + paint::width(v) + 3).sum();
            let x = l.fit(1, text, w.saturating_sub(keys + 4), ink.bold);
            hints(&mut l, x + 3, h, ink);
        }
        Footer::Find(q) => {
            let x = l.put(1, "/ ", ink.accent);
            let x = l.put(x, q.as_str(), ink.text);
            l.put(x, " ", ink.text.add_modifier(ratatui::style::Modifier::REVERSED));
            let start = l.rput(w - 1, "↵ Keep   Esc Clear", ink.dim);
            l.put(start, "↵", ink.text);
            l.put(start + 9, "Esc", ink.text);
        }
    }
    l
}
