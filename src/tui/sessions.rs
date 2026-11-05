//! The Sessions page: the headline, then sessions grouped by what needs
//! you. With a session open it splits: a narrow list on the left (j/k
//! still move) and the session's panel on the right, or the panel alone
//! on narrow terminals.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::model::{headline, short_age, Band, Row, Status};
use super::paint::{paint, window, Ink, Ln, SPIN, STARTING};
use super::{detail, hints, View, SPLIT_AT};

/// What a start that failed takes.
pub(crate) const START_FAILED: [(&str, &str); 3] = [("r", "Retry"), ("e", "Edit prompt"), ("x", "Dismiss")];

/// Width of the narrow list beside an open session.
const COMPACT: u16 = 34;

pub(crate) fn render(buf: &mut Buffer, area: Rect, v: &View, ink: &Ink) {
    let open = v.open && v.cur.is_some_and(|k| v.rows.iter().any(|r| r.key == *k));
    let row = v.cur.and_then(|k| v.rows.iter().find(|r| r.key == *k));
    match (open, row) {
        (true, Some(row)) if area.width >= SPLIT_AT => {
            let list = Rect { x: area.x + 1, width: COMPACT, ..area };
            paint(buf, list, &window(compact(v, ink), usize::from(area.height)));
            let divider: Vec<Ln> = (0..area.height).map(|_| Ln::text(0, "│", ink.dim)).collect();
            paint(buf, Rect { x: area.x + COMPACT + 2, width: 1, ..area }, &divider);
            let panel = Rect { x: area.x + COMPACT + 3, width: area.width - COMPACT - 4, ..area };
            detail::render(buf, panel, row, &v.detail, v.spin, ink);
        }
        (true, Some(row)) => detail::render(buf, area, row, &v.detail, v.spin, ink),
        _ => {
            paint(buf, area, &window(full(v, ink, area.width), usize::from(area.height)));
        }
    }
}

/// Is column `x` over the open session's panel (not the list beside it)?
pub(crate) fn over_panel(width: u16, x: u16) -> bool {
    width < SPLIT_AT || x > COMPACT + 2
}

fn glyph(r: &Row, spin: usize, ink: &Ink) -> (&'static str, ratatui::style::Style) {
    match r.status {
        Status::Asking => ("●", ink.attention),
        Status::Permission | Status::Request => ("◆", ink.attention),
        Status::Failed | Status::NotStarted => ("▲", ink.attention),
        Status::Working => (SPIN[spin % SPIN.len()], ink.text),
        Status::Starting => (STARTING[spin / 2 % STARTING.len()], ink.dim),
        Status::Gone | Status::Offline => ("◌", ink.dim),
        Status::Idle | Status::Dormant => ("○", ink.dim),
    }
}

/// Needs-you rows in bold, finished ones dim.
fn title_style(r: &Row, ink: &Ink) -> ratatui::style::Style {
    match r.status {
        Status::Offline | Status::Gone | Status::Starting => ink.dim,
        s if s.band() == Band::Need => ink.bold,
        _ => ink.text,
    }
}

/// ⇄ synced, ⊡ private, ⚑ notify when it finishes.
pub(crate) fn marks(r: &Row) -> String {
    let m: Vec<&str> =
        [r.synced.then_some("⇄"), r.private.then_some("⊡"), r.notify.then_some("⚑")].into_iter().flatten().collect();
    m.join(" ")
}

fn time(r: &Row, short: bool) -> String {
    match r.status {
        Status::Offline if short => "Off".into(),
        Status::Offline | Status::Gone => r.status.word().into(),
        _ => short_age(r.since),
    }
}

fn full(v: &View, ink: &Ink, w: u16) -> Vec<Ln> {
    let mut lines = Vec::new();
    if v.find.is_none() {
        let (head, sub) = headline(v.rows);
        lines.push(Ln::text(3, head, ink.bold));
        lines.push(Ln::text(3, sub, ink.dim));
        lines.push(Ln::new());
    }
    if v.bands.is_empty() {
        if let Some(q) = v.find {
            lines.push(Ln::text(3, format!("Nothing matches “{q}”."), ink.dim));
        }
    }
    let proj_x = w.saturating_sub(if w < 100 { 34 } else { 42 });
    let dev_x = w.saturating_sub(if w < 100 { 16 } else { 18 });
    for (i, (band, rows)) in v.bands.iter().enumerate() {
        if i > 0 {
            lines.push(Ln::new());
        }
        lines.push(Ln::text(3, band.title(), ink.dim));
        for r in rows {
            let sel = v.cur == Some(&r.key);
            let mut l = Ln::new().with_key(&r.key);
            let (g, gs) = glyph(r, v.spin, ink);
            l.put(3, g, gs);
            let title_style = title_style(r, ink);
            l.fit(6, &r.title, proj_x.saturating_sub(8), title_style);
            let x = l.fit(proj_x, &r.project, 16, ink.dim);
            let m = marks(r);
            if !m.is_empty() {
                l.put(x + 1, m, ink.dim);
            }
            if let Some(d) = &r.key.device {
                l.fit(dev_x, d, 10, ink.dim);
            }
            l.rput(w - 1, time(r, false), ink.dim);
            let mut group = vec![l];
            if sel && *band == Band::Need {
                group.extend(answer_lines(r, v, ink, w).into_iter().map(|l| l.with_key(&r.key)));
            }
            style_group(&mut group, v, r, ink);
            lines.extend(group);
        }
    }
    lines
}

/// Under the cursor, a session that needs you shows how to answer it:
/// the options (1–9 pick), or what to do when there are none.
fn answer_lines(r: &Row, v: &View, ink: &Ink, w: u16) -> Vec<Ln> {
    if let Some(sending) = v.detail.sending {
        return vec![Ln::text(6, sending, ink.dim)];
    }
    match &r.ask {
        Some(ask) => {
            let mut out = Vec::new();
            if let Some(cmd) = &ask.command {
                let mut l = Ln::new();
                l.fit(6, &format!("$ {cmd}"), w.saturating_sub(8), ink.dim);
                out.push(l);
            }
            let mut l = Ln::new();
            let mut x = 6;
            for (i, o) in ask.options.iter().enumerate() {
                x = l.put(x, (i + 1).to_string(), ink.attention.add_modifier(ratatui::style::Modifier::BOLD));
                x = l.put(x + 1, o.as_str(), ink.text) + 3;
            }
            out.push(l);
            out
        }
        None => {
            let what = match (r.status, &r.why) {
                (Status::Failed, Some(why)) => why.clone(),
                (Status::Failed, None) => "It stopped with an error.".into(),
                (_, Some(why)) => why.clone(),
                (_, None) => "Waiting for you.".into(),
            };
            let mut text = Ln::new();
            text.fit(6, &what, w.saturating_sub(8), ink.dim);
            let mut keys = Ln::new();
            let h: Vec<(&str, String)> = match (r.status, r.is_local()) {
                (Status::Request, _) => vec![("y", "Allow".into()), ("↵", "Review".into()), ("n", "Refuse".into())],
                (Status::NotStarted, _) => START_FAILED.iter().map(|(k, v)| (*k, v.to_string())).collect(),
                (Status::Failed, _) => vec![("i", "Reply".into()), ("↵", "Open".into())],
                (_, true) => vec![("t", "Answer in its terminal".into())],
                (_, false) => vec![("↵", "Open".into())],
            };
            hints(&mut keys, 6, &h, ink);
            vec![text, keys]
        }
    }
}

/// The cursor bar and selection or flash background over a row's lines.
fn style_group(group: &mut [Ln], v: &View, r: &Row, ink: &Ink) {
    let sel = v.cur == Some(&r.key);
    let flash = v.flash.filter(|(k, _)| **k == r.key).map(|(_, faint)| faint);
    if sel {
        group[0].select(ink, None);
    }
    let bg = match flash {
        Some(_) if v.theme.flash.is_some() => v.theme.flash,
        _ if sel => v.theme.selection,
        _ => None,
    };
    for l in group.iter_mut() {
        l.bg = bg;
    }
    // Without a flash colour, the arriving row's title lights up instead.
    if flash == Some(false) && v.theme.flash.is_none() {
        group[0].put(1, "▌", ink.attention);
    }
}

fn compact(v: &View, ink: &Ink) -> Vec<Ln> {
    let w = COMPACT;
    let mut lines = Vec::new();
    for (i, (band, rows)) in v.bands.iter().enumerate() {
        if i > 0 {
            lines.push(Ln::new());
        }
        lines.push(Ln::text(2, band.title(), ink.dim));
        for r in rows {
            let mut l = Ln::new().with_key(&r.key);
            let (g, gs) = glyph(r, v.spin, ink);
            l.put(2, g, gs);
            let dev_w = r.key.device.as_ref().map_or(0, |d| super::paint::width(d) + 2);
            let style = title_style(r, ink);
            let x = l.fit(5, &r.project, w.saturating_sub(13 + dev_w), style);
            let m = marks(r);
            if !m.is_empty() {
                l.put(x + 1, m, ink.dim);
            }
            if let Some(d) = &r.key.device {
                l.rput(w - 6, d.as_str(), ink.dim);
            }
            l.rput(w, time(r, true), ink.dim);
            let mut group = [l];
            style_group(&mut group, v, r, ink);
            let [l] = group;
            lines.push(l);
        }
    }
    lines
}
