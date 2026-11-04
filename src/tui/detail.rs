//! One session's panel: what it is and where, what it is asking (with
//! numbered answers), its recent transcript, and a prompt line at the
//! bottom.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use std::cell::Cell;

use super::model::{short_age, Band, Row, Said, Status};
use super::paint::{paint, para, tail_fit, wrap, Ink, Ln, SPIN};
use super::sessions::marks;
use super::DetailView;

pub(crate) fn render(buf: &mut Buffer, area: Rect, r: &Row, d: &DetailView, spin: usize, ink: &Ink) {
    let x = 2;
    let tw = area.width.saturating_sub(4);
    let mut top: Vec<Ln> = para(&r.title, x, tw, ink.bold);

    let mut meta = Ln::new();
    let mut mx = meta.put(x, r.project.as_str(), ink.dim);
    let m = marks(r);
    if !m.is_empty() {
        mx = meta.put(mx + 1, m, ink.dim);
    }
    let place = match &r.key.device {
        Some(dev) => format!("On {dev}"),
        None => "This device".into(),
    };
    mx = meta.put(mx + 2, place, ink.dim);
    let status = match r.status {
        Status::Offline | Status::Gone => r.status.word().to_string(),
        s => format!("{}, {}", s.word(), short_age(r.since)),
    };
    let status_x =
        meta.rput(area.width - 2, status, if r.status.band() == Band::Need { ink.attention } else { ink.dim });
    let mut where_ = r.path.clone();
    if let Some(b) = &r.branch {
        where_.push_str(&format!("  {b}"));
    }
    if r.cost >= 0.01 {
        where_.push_str(&format!("  ${:.2}", r.cost));
    }
    meta.fit(mx + 2, &where_, status_x.saturating_sub(mx + 4), ink.dim);
    top.push(meta);
    if r.private {
        top.push(Ln::text(x, "Private. Your other devices can't see this session.", ink.dim));
    }
    top.push(Ln::new());

    if r.status.band() == Band::Need {
        top.extend(ask_card(r, d, ink, x, tw));
        top.push(Ln::new());
    }

    let body: Vec<Ln> = match d.screen {
        Some(lines) => {
            let mut b = vec![
                Ln::text(x, "Its screen, captured just now. Esc goes back to the transcript.", ink.dim),
                Ln::new(),
            ];
            b.extend(lines.iter().map(|l| {
                let mut ln = Ln::new();
                ln.fit(x, l, tw, ink.text);
                ln
            }));
            b
        }
        None => match if r.is_local() { d.transcript } else { Some(r.tail.as_slice()) } {
            Some(said) => transcript(said, ink, x, tw),
            None => {
                let mut l = Ln::text(x, SPIN[spin % SPIN.len()], ink.accent);
                l.put(x + 2, "Reading the transcript", ink.dim);
                vec![l]
            }
        },
    };

    let mut composer = vec![Ln::text(x, "─".repeat(usize::from(tw)), ink.dim)];
    let mut c = Ln::new();
    match d.compose {
        Some(text) => {
            let e = c.put(x, "› ", ink.accent);
            let shown = tail_fit(text, tw.saturating_sub(4));
            let e = c.put(e, shown, ink.text);
            c.put(e, " ", ink.text.add_modifier(Modifier::REVERSED));
        }
        None => {
            let hint = match (r.status, d.can_compose, r.can_send) {
                (Status::Offline, ..) => format!("{} is offline", r.key.device.as_deref().unwrap_or("Its device")),
                (_, false, _) => "Connect a device (c in Settings) to send prompts from here".into(),
                (_, true, false) => "This session has no terminal or mod to send a prompt through".into(),
                _ => "Send a prompt to this session".into(),
            };
            c.put(x, format!("› {hint}"), ink.dim);
        }
    }
    composer.push(c);

    let height = usize::from(area.height);
    let room = height.saturating_sub(top.len() + composer.len());
    let mut lines = top;
    lines.truncate(height.saturating_sub(composer.len()));
    lines.extend(scrolled(body, room, d.scroll, ink, x));
    while lines.len() + composer.len() < height {
        lines.push(Ln::new());
    }
    lines.extend(composer);
    paint(buf, area, &lines);
}

/// The `room` lines of `body` that end `scroll` lines above its newest,
/// with a note where more lies above or below. Clamps `scroll`, so
/// scrolling back down never has slack to take up first.
fn scrolled(mut body: Vec<Ln>, room: usize, scroll: &Cell<usize>, ink: &Ink, x: u16) -> Vec<Ln> {
    let n = body.len();
    if n <= room || room < 3 {
        scroll.set(0);
        body.drain(..n.saturating_sub(room));
        return body;
    }
    let s = scroll.get().min(n - room + 1);
    scroll.set(s);
    let end = n - s;
    let mut start = end - (room - usize::from(s > 0));
    let mut out = Vec::with_capacity(room);
    if start > 0 {
        start += 1;
        out.push(Ln::text(x, "↑ Earlier messages", ink.dim));
    }
    out.extend(body.drain(start..end));
    if s > 0 {
        out.push(Ln::text(x, "↓ Newer messages. End jumps back.", ink.dim));
    }
    out
}

fn ask_card(r: &Row, d: &DetailView, ink: &Ink, x: u16, tw: u16) -> Vec<Ln> {
    let bar = |mut l: Ln| {
        l.put(x, "┃", ink.attention);
        l
    };
    let inner = tw.saturating_sub(3);
    let mut out = Vec::new();
    let question = match (&r.ask, r.status) {
        (Some(a), _) => a.text.clone(),
        (None, Status::Failed) => {
            format!("It stopped with an error. {}", r.why.clone().unwrap_or_default()).trim().to_string()
        }
        (None, _) => r.why.clone().unwrap_or_else(|| "It is waiting for you.".into()),
    };
    out.extend(wrap(&question, inner).into_iter().map(|t| bar(Ln::text(x + 2, t, ink.text))));
    if let Some(cmd) = r.ask.as_ref().and_then(|a| a.command.as_ref()) {
        out.extend(wrap(&format!("$ {cmd}"), inner).into_iter().map(|t| bar(Ln::text(x + 2, t, ink.dim))));
    }
    out.push(bar(Ln::new()));
    match (&r.ask, d.sending) {
        (_, Some(s)) => out.push(bar(Ln::text(x + 2, s, ink.dim))),
        (Some(a), None) => {
            for (i, o) in a.options.iter().enumerate() {
                let mut l = Ln::new();
                l.put(x + 2, (i + 1).to_string(), ink.attention.add_modifier(Modifier::BOLD));
                l.fit(x + 5, o, inner.saturating_sub(3), ink.text);
                out.push(bar(l));
            }
        }
        (None, None) => {
            let how = match (r.status, r.is_local()) {
                (Status::Failed, _) => "Press i to tell it what to do next.".to_string(),
                (_, true) => "Answer it in its terminal: press t.".to_string(),
                (_, false) => {
                    format!("Answer it in its terminal on {}.", r.key.device.as_deref().unwrap_or("its device"))
                }
            };
            out.push(bar(Ln::text(x + 2, how, ink.dim)));
        }
    }
    out
}

/// Runs of tool calls longer than this fold their start into one line.
const TOOL_RUN: usize = 4;

/// The conversation: › what you typed, ● what Claude said, ▸ its tool
/// calls, with a blank line wherever the speaker changes.
fn transcript(said: &[Said], ink: &Ink, x: u16, tw: u16) -> Vec<Ln> {
    if said.is_empty() {
        return vec![Ln::text(x, "No transcript yet.", ink.dim)];
    }
    let cols = tw.saturating_sub(2);
    let mut out: Vec<Ln> = Vec::new();
    let mut i = 0;
    while i < said.len() {
        if i > 0 {
            out.push(Ln::new());
        }
        match &said[i] {
            Said::You(t) => {
                let mut body = super::markdown::render(t, x + 2, cols, ink.bold, ink);
                if let Some(first) = body.first_mut() {
                    first.put(x, "›", ink.accent);
                }
                out.extend(body);
                i += 1;
            }
            Said::Claude(t) => {
                let mut body = super::markdown::render(t, x + 2, cols, ink.text, ink);
                if let Some(first) = body.first_mut() {
                    first.put(x, "●", ink.accent);
                }
                out.extend(body);
                i += 1;
            }
            Said::Tool(_) => {
                let run: Vec<&str> = said[i..]
                    .iter()
                    .map_while(|s| if let Said::Tool(t) = s { Some(t.as_str()) } else { None })
                    .collect();
                i += run.len();
                let shown = if run.len() > TOOL_RUN { TOOL_RUN - 1 } else { run.len() };
                let (folded, kept) = run.split_at(run.len() - shown);
                if !folded.is_empty() {
                    let mut l = Ln::new();
                    l.fit(x + 2, &format!("⋯ {} earlier: {}", folded.len(), tally(folded)), cols, ink.dim);
                    out.push(l);
                }
                out.extend(kept.iter().map(|t| tool(t, ink, x + 2, cols)));
            }
        }
    }
    out
}

/// `▸ Bash  cargo test`: the tool's name in text, what it ran dim. A
/// failure reads in amber.
fn tool(t: &str, ink: &Ink, x: u16, cols: u16) -> Ln {
    let mut l = Ln::new();
    if t.starts_with("Failed") || t == "Turn failed" {
        l.fit(x, &format!("▲ {t}"), cols, ink.attention);
        return l;
    }
    let (name, rest) = t.split_once(' ').unwrap_or((t, ""));
    let e = l.put(x, "▸", ink.dim);
    let e = l.fit(e + 1, name, cols.saturating_sub(2), ink.text);
    l.fit(e + 2, rest, cols.saturating_sub(e + 2 - x), ink.dim);
    l
}

/// `Bash ×5, Read ×2`, most used first.
fn tally(tools: &[&str]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for t in tools {
        let name = t.split(' ').next().unwrap_or(t);
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => counts.push((name, 1)),
        }
    }
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    counts.iter().map(|(n, c)| if *c > 1 { format!("{n} ×{c}") } else { n.to_string() }).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    #[test]
    fn scrolling_shows_where_more_lies_and_stops_at_the_ends() {
        let ink = Ink::new(&Theme::catalog(None)[0]);
        let body = || (0..20).map(|i| Ln::text(0, i.to_string(), ink.text)).collect::<Vec<_>>();
        let shown = |s: usize| {
            let scroll = Cell::new(s);
            let lines: Vec<String> = scrolled(body(), 6, &scroll, &ink, 0).iter().map(Ln::plain).collect();
            (lines, scroll.get())
        };
        assert_eq!(
            shown(0),
            (vec!["↑ Earlier messages".into(), "15".into(), "16".into(), "17".into(), "18".into(), "19".into()], 0)
        );
        let (mid, _) = shown(4);
        assert_eq!(
            (mid.first().unwrap().as_str(), mid[4].as_str(), mid.last().unwrap().as_str()),
            ("↑ Earlier messages", "15", "↓ Newer messages. End jumps back.")
        );
        let (top, s) = shown(usize::MAX);
        assert_eq!(
            (top[0].as_str(), top.len(), s),
            ("0", 6, 15),
            "the top shows the first line, and the scroll is clamped"
        );
    }
}
