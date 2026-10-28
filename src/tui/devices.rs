//! The Devices view (`D`): your other devices, what each is doing, and
//! what is waiting for you, with the actions on them.
//!
//! Wide terminals get a device list on the left and the selected device
//! on the right. Below 90 columns (a phone over SSH) the list collapses
//! into a tab strip and the device fills the screen. On every device
//! "needs you" comes first: pending permission prompts and questions,
//! answerable in place.
//!
//! Pure rendering over a `SyncView` snapshot; strings from the network go
//! through `Sanitised` here, at the render boundary.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::theme::Theme;
use crate::sync::agent::{PeerView, SyncView};
use crate::sync::protocol::{now_secs, DeviceState, PendingRequest, RemoteSession};
use crate::util::sanitize::Sanitised;
use crate::util::text::{format_age, truncate_width};

/// Below this width the device list becomes a tab strip.
const NARROW: u16 = 90;
/// A device whose state is older than this is shown as stale.
const STALE_SECS: u64 = 180;

/// One selectable line of the selected device.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Row {
    Session { device: String, id: String, injectable: bool, via_mod: bool },
    Request { device: String, req: PendingRequest },
    Job { device: String, id: String },
}

pub(crate) enum Mode {
    List,
    /// Typing an instruction for a remote session.
    Compose { device: String, session: String, via_mod: bool, buf: String },
    /// Reading a fetched diff / log.
    Blob { scroll: usize },
}

pub(crate) struct DevicesState {
    /// Name of the device shown; follows the device, not its position.
    pub(crate) device: Option<String>,
    pub(crate) cursor: usize,
    /// Identity of the selected row. Peers republish their state every
    /// few seconds and rows can shift; acting on an index once sent an
    /// instruction to the wrong session, so the cursor follows this.
    pub(crate) key: Option<String>,
    pub(crate) mode: Mode,
}

impl Row {
    pub(crate) fn key(&self) -> String {
        match self {
            Row::Session { device, id, .. } => format!("s/{device}/{id}"),
            Row::Request { device, req } => format!("r/{device}/{}", req.id),
            Row::Job { device, id } => format!("j/{device}/{id}"),
        }
    }
}

impl DevicesState {
    pub(crate) fn new() -> Self {
        Self { device: None, cursor: 0, key: None, mode: Mode::List }
    }

    /// The device shown: the remembered one if it still exists, else the
    /// first one that needs you, else the first.
    pub(crate) fn current<'a>(&mut self, view: &'a SyncView) -> Option<&'a PeerView> {
        let found = self.device.as_ref().and_then(|d| view.peers.iter().find(|p| p.name == *d));
        let p = found
            .or_else(|| view.peers.iter().find(|p| p.state.as_ref().is_some_and(|s| !s.requests.is_empty())))
            .or_else(|| view.peers.first())?;
        self.device = Some(p.name.clone());
        Some(p)
    }

    /// Show the next (or previous) device.
    pub(crate) fn cycle(&mut self, view: &SyncView, forward: bool) {
        let n = view.peers.len();
        if n == 0 {
            return;
        }
        let i = self.device.as_ref().and_then(|d| view.peers.iter().position(|p| p.name == *d)).unwrap_or(0);
        let j = if forward { (i + 1) % n } else { (i + n - 1) % n };
        self.device = Some(view.peers[j].name.clone());
        self.cursor = 0;
        self.key = None;
    }

    /// Re-find the selected row after a refresh; falls back to the old
    /// index (clamped) when it vanished.
    pub(crate) fn resolve(&mut self, rows: &[Row]) -> Option<Row> {
        if let Some(i) = self.key.as_ref().and_then(|k| rows.iter().position(|r| r.key() == *k)) {
            self.cursor = i;
        }
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        let row = rows.get(self.cursor).cloned();
        self.key = row.as_ref().map(Row::key);
        row
    }
}

/// Rows of one device in display order: what needs you, then sessions
/// (busiest first), then jobs. The cursor indexes into this.
pub(crate) fn rows(peer: Option<&PeerView>) -> Vec<Row> {
    let Some(p) = peer else { return Vec::new() };
    let Some(st) = &p.state else { return Vec::new() };
    let mut out: Vec<Row> = st.requests.iter().map(|r| Row::Request { device: p.name.clone(), req: r.clone() }).collect();
    for s in ordered_sessions(st) {
        out.push(Row::Session { device: p.name.clone(), id: s.id.clone(), injectable: s.injectable, via_mod: s.via_mod });
    }
    for j in &st.jobs {
        out.push(Row::Job { device: p.name.clone(), id: j.id.clone() });
    }
    out
}

/// Waiting first, then working, then idle; stable within each group.
fn ordered_sessions(st: &DeviceState) -> Vec<&RemoteSession> {
    let rank = |s: &RemoteSession| match s.state.as_str() {
        "Waiting" => 0,
        "Error" => 1,
        "Processing" => 2,
        x if x.starts_with("Tool") => 2,
        _ => 3,
    };
    let mut v: Vec<&RemoteSession> = st.sessions.iter().collect();
    v.sort_by_key(|s| rank(s));
    v
}

/// Network text, made safe to draw: sanitised, and tabs (which the
/// terminal expands past our width math) turned into spaces.
fn san(s: &str) -> String {
    Sanitised::new(s.to_string()).as_str().replace('\t', "  ")
}

fn short_path(p: &str) -> String {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
    let p = san(p);
    match p.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => p.replacen("/home/", "~", 1).replacen("/Users/", "~", 1),
    }
}

fn age_short(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

struct Palette {
    text: Style,
    bold: Style,
    muted: Style,
    ok: Color,
    warn: Color,
    bad: Color,
    accent: Color,
}

impl Palette {
    fn new(t: &Theme) -> Self {
        Self {
            text: Style::default().fg(t.text),
            bold: Style::default().fg(t.text).add_modifier(Modifier::BOLD),
            muted: Style::default().fg(t.muted),
            ok: t.processing,
            warn: t.waiting,
            bad: t.error,
            accent: t.processing,
        }
    }
}

/// Freshness of a device: (dot colour, "synced 4s ago" text).
fn freshness(p: &PeerView, pal: &Palette, now: u64) -> (Color, String) {
    match p.state_generated_at {
        Some(g) => {
            let age = now.saturating_sub(g);
            (if age < STALE_SECS { pal.ok } else { pal.warn }, format!("synced {}", format_age(age as f64)))
        }
        None => (pal.bad, "never synced".into()),
    }
}

fn state_color(state: &str, t: &Theme) -> Color {
    match state {
        "Processing" => t.processing,
        "Waiting" => t.waiting,
        "Error" => t.error,
        s if s.starts_with("Tool") => t.processing,
        _ => t.idle,
    }
}

pub(crate) fn render(frame: &mut Frame, theme: &Theme, view: &SyncView, ui: &mut DevicesState) {
    // A full view, not a popup: covers the grid entirely.
    let area = frame.area();
    frame.render_widget(Clear, area);
    let pal = Palette::new(theme);
    let needs: usize = view.peers.iter().filter_map(|p| p.state.as_ref()).map(|s| s.requests.len()).sum();
    let mut title = vec![Span::styled(" Devices ", pal.bold)];
    if needs > 0 {
        title.push(Span::styled(format!(" {needs} need{} you ", if needs == 1 { "s" } else { "" }), Style::default().fg(theme.selected_text).bg(pal.warn).add_modifier(Modifier::BOLD)));
        title.push(Span::raw(" "));
    }
    let block = Block::default()
        .title(Line::from(title))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [head, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)]).areas(inner);
    render_header(frame, head, view, &pal);

    if let Mode::Blob { scroll } = &mut ui.mode {
        render_blob(frame, body, foot, theme, view, scroll);
        return;
    }

    let peer = ui.current(view).cloned();
    let rows = rows(peer.as_ref());
    let selected = ui.resolve(&rows);
    if view.peers.is_empty() {
        let msg = vec![
            Line::raw(""),
            Line::from(Span::styled("  No other devices yet.", pal.bold)),
            Line::raw(""),
            Line::from(Span::styled("  On this device run   nerve setup", pal.text)),
            Line::from(Span::styled("  then run the command it prints on the other one.", pal.muted)),
        ];
        frame.render_widget(Paragraph::new(msg), body);
        hints(frame, foot, theme, view, &[("Esc", "close")]);
        return;
    }
    let detail = if body.width < NARROW {
        let [tabs, rest] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);
        render_tabs(frame, tabs, view, ui, &pal);
        rest
    } else {
        let [list, gap, rest] = Layout::horizontal([Constraint::Length(30), Constraint::Length(1), Constraint::Min(0)]).areas(body);
        render_device_list(frame, list, view, ui, &pal);
        frame.render_widget(Paragraph::new(vec![Line::styled("│", pal.muted); gap.height as usize]), gap);
        rest
    };
    if let Some(p) = &peer {
        render_device(frame, detail, theme, &pal, p, &rows, ui.cursor);
    }

    if let Mode::Compose { device, buf, via_mod, .. } = &ui.mode {
        render_compose(frame, area, theme, &pal, device, buf, *via_mod);
        hints(frame, foot, theme, view, &[("Enter", "send"), ("Esc", "cancel")]);
        return;
    }
    let base: &[(&str, &str)] = match &selected {
        Some(Row::Request { req, .. }) if req.kind == "question" => &[("1-9", "answer"), ("x", "decline"), ("Tab", "device"), ("r", "sync"), ("Esc", "close")],
        Some(Row::Request { .. }) => &[("a", "allow"), ("x", "deny"), ("Tab", "device"), ("r", "sync"), ("Esc", "close")],
        Some(Row::Session { injectable: true, .. }) => &[("m", "message"), ("d", "diff"), ("l", "log"), ("g", "agents"), ("Tab", "device"), ("Esc", "close")],
        Some(Row::Session { .. }) => &[("d", "diff"), ("l", "log"), ("g", "agents"), ("Tab", "device"), ("Esc", "close")],
        _ => &[("Tab", "device"), ("r", "sync now"), ("A", "away"), ("Esc", "close")],
    };
    hints(frame, foot, theme, view, base);
}

fn render_header(frame: &mut Frame, area: Rect, view: &SyncView, pal: &Palette) {
    let now = now_secs();
    let mut spans = vec![Span::raw(" ")];
    if view.running {
        spans.push(Span::styled("● ", Style::default().fg(pal.ok)));
        spans.push(Span::styled(san(&view.me), pal.bold));
        if let Some(t) = view.last_publish {
            spans.push(Span::styled(format!("  published {} ago", age_short(now.saturating_sub(t))), pal.muted));
        }
        if view.mod_sessions > 0 {
            spans.push(Span::styled(format!("  · {} session(s) via mod", view.mod_sessions), pal.muted));
        }
        if view.away {
            spans.push(Span::styled("  · away", Style::default().fg(pal.warn)));
        }
    } else {
        spans.push(Span::styled("● sync off ", Style::default().fg(pal.bad)));
        spans.push(Span::styled(san(view.error.as_deref().unwrap_or("")), pal.muted));
    }
    // Relays as dots, right-aligned: ● connected, ○ down.
    let up = view.relays.iter().filter(|r| r.connected).count();
    let relays = format!("relays {}", view.relays.iter().map(|r| if r.connected { '●' } else { '○' }).collect::<String>());
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(used + relays.chars().count() + 1);
    spans.push(Span::raw(" ".repeat(pad)));
    let rc = if view.relays.is_empty() || up == 0 { pal.bad } else if up < view.relays.len() { pal.warn } else { pal.ok };
    spans.push(Span::styled(relays, Style::default().fg(rc)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_device_list(frame: &mut Frame, area: Rect, view: &SyncView, ui: &DevicesState, pal: &Palette) {
    let now = now_secs();
    let mut lines = vec![Line::raw("")];
    for p in &view.peers {
        let sel = ui.device.as_deref() == Some(p.name.as_str());
        let (dot, fresh) = freshness(p, pal, now);
        let base = if sel { pal.bold.add_modifier(Modifier::REVERSED) } else { pal.bold };
        lines.push(Line::from(vec![
            Span::styled(if sel { "▌" } else { " " }, Style::default().fg(pal.accent)),
            Span::styled("● ", Style::default().fg(dot)),
            Span::styled(format!("{:<20}", truncate_width(&san(&p.name), 20)), base),
        ]));
        let mut meta = vec![Span::raw("    "), Span::styled(fresh, pal.muted)];
        if let Some(st) = &p.state {
            let waiting = st.requests.len();
            if waiting > 0 {
                meta.push(Span::styled(format!(" · {waiting} waiting"), Style::default().fg(pal.warn)));
            }
            lines.push(Line::from(meta));
            let busy = st.sessions.iter().filter(|s| s.state != "Idle").count();
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{} session{}", st.sessions.len(), if st.sessions.len() == 1 { "" } else { "s" }), pal.muted),
                Span::styled(if busy > 0 { format!(", {busy} active") } else { String::new() }, pal.muted),
            ]));
        } else {
            lines.push(Line::from(meta));
        }
        lines.push(Line::raw(""));
    }
    for (t, done, of) in &view.transfers {
        lines.push(Line::from(Span::styled(format!(" {}", truncate_width(&san(t), 28)), pal.muted)));
        lines.push(progress_bar(*done, *of, 26, pal));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_tabs(frame: &mut Frame, area: Rect, view: &SyncView, ui: &DevicesState, pal: &Palette) {
    let now = now_secs();
    let mut spans = vec![Span::raw(" ")];
    for p in &view.peers {
        let sel = ui.device.as_deref() == Some(p.name.as_str());
        let (dot, _) = freshness(p, pal, now);
        let waiting = p.state.as_ref().map(|s| s.requests.len()).unwrap_or(0);
        spans.push(Span::styled("●", Style::default().fg(dot)));
        let label = if waiting > 0 { format!(" {} ({waiting}) ", san(&p.name)) } else { format!(" {} ", san(&p.name)) };
        spans.push(Span::styled(label, if sel { pal.bold.add_modifier(Modifier::REVERSED) } else { pal.muted }));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(vec![Line::from(spans), Line::raw("")]), area);
}

fn progress_bar(done: usize, of: usize, width: usize, pal: &Palette) -> Line<'static> {
    let filled = (done * width).checked_div(of).unwrap_or(width).min(width);
    Line::from(vec![
        Span::raw(" "),
        Span::styled("█".repeat(filled), Style::default().fg(pal.accent)),
        Span::styled("░".repeat(width - filled), pal.muted),
        Span::styled(format!(" {done}/{of}"), pal.muted),
    ])
}

/// The selected device: what needs you, sessions, jobs, and the selected
/// session's recent transcript.
fn render_device(frame: &mut Frame, area: Rect, theme: &Theme, pal: &Palette, p: &PeerView, rows: &[Row], cursor: usize) {
    let now = now_secs();
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    let (dot, fresh) = freshness(p, pal, now);
    let mut head = vec![Span::styled(" ● ", Style::default().fg(dot)), Span::styled(san(&p.name), pal.bold), Span::styled(format!("  {fresh}"), pal.muted)];
    let Some(st) = &p.state else {
        head.push(Span::styled("  waiting for its first update", pal.muted));
        lines.push(Line::from(head));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("   Is nerve running there, on the same relays? Try `nerve sync doctor` on it.", pal.muted)));
        lines.push(Line::from(Span::styled(format!("   trusted key {}", truncate_width(&p.npub, width.saturating_sub(16))), pal.muted)));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    };
    if width > 60 {
        head.push(Span::styled(format!("  {} {}", san(&st.os), san(&st.arch)), pal.muted));
    }
    if st.away {
        head.push(Span::styled("  away", Style::default().fg(pal.warn)));
    }
    lines.push(Line::from(head));
    if width > 60 {
        lines.push(Line::from(Span::styled(format!("   can run: {}", truncate_width(&san(&st.caps.join(" ")), width.saturating_sub(14))), pal.muted)));
    }

    let mut cursor_line = 0usize;
    let mut i = 0usize;
    let mut select = |lines: &mut Vec<Line<'static>>, i: usize, mut l: Line<'static>| {
        if i == cursor {
            cursor_line = lines.len();
            l.spans.insert(0, Span::styled(" ▶ ", Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)));
            l = l.style(Style::default().bg(theme.selected_bg));
        } else {
            l.spans.insert(0, Span::raw("   "));
        }
        lines.push(l);
    };

    if !st.requests.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(" NEEDS YOU", Style::default().fg(pal.warn).add_modifier(Modifier::BOLD))));
        for r in &st.requests {
            let session = st.sessions.iter().find(|s| s.id == r.session).map(|s| san(&s.name)).unwrap_or_default();
            let age = if r.created_at == 0 { String::new() } else { age_short(now.saturating_sub(r.created_at)) };
            let what = if r.kind == "question" {
                format!("? {}", san(&r.summary))
            } else {
                format!("{} wants to run {}", if session.is_empty() { "a session".into() } else { session.clone() }, san(&r.tool))
            };
            select(&mut lines, i, Line::from(vec![Span::styled(truncate_width(&what, width.saturating_sub(12)), pal.bold), Span::styled(format!("  {age}"), pal.muted)]));
            i += 1;
            if r.kind == "question" {
                if let Some(q) = r.questions.first() {
                    let opts: Vec<Span> = q
                        .options
                        .iter()
                        .enumerate()
                        .flat_map(|(n, o)| [Span::styled(format!("[{}]", n + 1), Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)), Span::styled(format!(" {}   ", san(o)), pal.text)])
                        .collect();
                    lines.push(Line::from([vec![Span::raw("      ")], opts].concat()));
                }
            } else {
                lines.push(Line::from(vec![Span::raw("      "), Span::styled(truncate_width(&san(&r.summary), width.saturating_sub(8)), pal.muted)]));
            }
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(format!(" SESSIONS  {}", st.sessions.len()), pal.muted.add_modifier(Modifier::BOLD))));
    let selected_session = match rows.get(cursor) {
        Some(Row::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    for s in ordered_sessions(st) {
        let name_w = (width / 3).clamp(14, 40);
        let state = s.waiting_for.as_ref().map(|w| format!("waiting: {}", san(w))).unwrap_or_else(|| san(&s.state).to_lowercase());
        let mut l = vec![
            Span::styled("● ", Style::default().fg(state_color(&s.state, theme))),
            Span::styled(format!("{:<w$} ", truncate_width(&san(&s.name), name_w), w = name_w), pal.bold),
            Span::styled(format!("{:<18}", truncate_width(&state, 18)), Style::default().fg(state_color(&s.state, theme))),
        ];
        if width > 70 {
            let mut meta = String::new();
            if let Some(b) = &s.branch {
                meta.push_str(&format!(" ⎇ {}", truncate_width(&san(b), 18)));
            }
            if s.cost_usd >= 0.01 {
                meta.push_str(&format!("  ${:.2}", s.cost_usd));
            }
            if s.subagents > 0 {
                meta.push_str(&format!("  {} agents", s.subagents));
            }
            l.push(Span::styled(meta, pal.muted));
        }
        let reach = if s.via_mod { "mod" } else if s.injectable { "tty" } else { "view" };
        l.push(Span::styled(format!("  {reach}"), if s.via_mod { Style::default().fg(pal.accent) } else { pal.muted }));
        select(&mut lines, i, Line::from(l));
        i += 1;
        if selected_session.as_deref() == Some(s.id.as_str()) {
            let mut sub = format!("      {}", short_path(&s.cwd));
            if let Some(t) = &s.last_turn {
                sub.push_str(&format!(
                    "  · last turn {} ago, {:.1}s, {}k tokens in / {} out",
                    age_short(now.saturating_sub(t.ended_at)),
                    t.ms as f64 / 1000.0,
                    t.input_tokens / 1000,
                    t.output_tokens
                ));
            }
            lines.push(Line::from(Span::styled(truncate_width(&sub, width.saturating_sub(1)), pal.muted)));
            for t in &s.tail {
                lines.push(Line::from(vec![Span::styled("      │ ", pal.muted), Span::styled(truncate_width(&san(t), width.saturating_sub(9)), pal.text)]));
            }
            if !s.injectable {
                let note = "      view only: no terminal or mod to send input through";
                lines.push(Line::from(Span::styled(truncate_width(note, width.saturating_sub(1)), pal.muted)));
            }
        }
    }
    if st.sessions.is_empty() {
        lines.push(Line::from(Span::styled("    no Claude sessions running", pal.muted)));
    }

    if !st.jobs.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(" JOBS & RUNS", pal.muted.add_modifier(Modifier::BOLD))));
        for j in &st.jobs {
            let (mark, c) = match (j.phase.as_str(), j.verified) {
                ("done", Some(true)) => ("✓ verified", pal.ok),
                ("done", _) => ("✓ done", pal.ok),
                ("failed", _) | (_, Some(false)) => ("✗ failed", pal.bad),
                (p, _) => (p, pal.warn),
            };
            select(
                &mut lines,
                i,
                Line::from(vec![
                    Span::styled(format!("{:<28} ", truncate_width(&san(&j.title), 28)), pal.text),
                    Span::styled(format!("{mark:<12}"), Style::default().fg(c)),
                    Span::styled(
                        if j.kind == "run" {
                            let code = j.exit_code.map(|c| format!("exit {c} · ")).unwrap_or_default();
                            format!(" {code}{} lines{}", j.lines, j.tail.last().map(|l| format!(" · {}", truncate_width(&san(l), 40))).unwrap_or_default())
                        } else {
                            format!(" {} turns · ${:.3}", j.turns, j.cost_usd)
                        },
                        pal.muted,
                    ),
                ]),
            );
            i += 1;
        }
    }
    // Scroll only as far as needed to keep the selected row (and the line
    // under it) on screen.
    let h = area.height as usize;
    let top = (cursor_line + 2).saturating_sub(h);
    frame.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), area);
}

fn render_blob(frame: &mut Frame, body: Rect, foot: Rect, theme: &Theme, view: &SyncView, scroll: &mut usize) {
    let pal = Palette::new(theme);
    if let Some((t, got, of)) = view.incoming.first() {
        let lines = vec![Line::raw(""), Line::from(Span::styled(format!("  receiving {}", san(t)), pal.text)), progress_bar(*got, *of, 30, &pal)];
        frame.render_widget(Paragraph::new(lines), body);
        hints(frame, foot, theme, view, &[("Esc", "back")]);
        return;
    }
    let Some((t, text)) = view.blobs.back().cloned() else {
        frame.render_widget(Paragraph::new(Line::from(Span::styled("  asking the device…", pal.muted))), body);
        hints(frame, foot, theme, view, &[("Esc", "back")]);
        return;
    };
    let total = text.lines().count();
    let max = total.saturating_sub(body.height.saturating_sub(2) as usize);
    *scroll = (*scroll).min(max);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {}", san(&t)), pal.bold),
        Span::styled(format!("   lines {}-{} of {total}", *scroll + 1, (*scroll + body.height as usize).min(total)), pal.muted),
    ])];
    lines.push(Line::raw(""));
    lines.extend(text.lines().skip(*scroll).take(body.height as usize).map(|l| {
        let style = if l.starts_with("@@") {
            Style::default().fg(theme.waiting)
        } else if l.starts_with('+') && !l.starts_with("+++") {
            Style::default().fg(theme.processing)
        } else if l.starts_with('-') && !l.starts_with("---") {
            Style::default().fg(theme.error)
        } else {
            pal.text
        };
        Line::from(Span::styled(l.replace('\t', "    "), style))
    }));
    frame.render_widget(Paragraph::new(lines), body);
    hints(frame, foot, theme, view, &[("j/k", "scroll"), ("PgUp/PgDn", "page"), ("Esc", "back")]);
}

fn render_compose(frame: &mut Frame, area: Rect, theme: &Theme, pal: &Palette, device: &str, buf: &str, via_mod: bool) {
    let box_area = super::centered(area, area.width.saturating_sub(6).min(96), 8);
    frame.render_widget(Clear, box_area);
    let b = Block::default()
        .title(Span::styled(format!(" message to {} ", san(device)), pal.bold))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.processing));
    let i = b.inner(box_area);
    frame.render_widget(b, box_area);
    let how = if via_mod {
        "sent through the nerve mod: queued if Claude is busy, as if typed there"
    } else {
        "typed into the session's terminal and submitted"
    };
    let [text, note] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(i);
    frame.render_widget(Paragraph::new(format!("{buf}▏")).wrap(Wrap { trim: false }), text);
    frame.render_widget(Paragraph::new(Span::styled(how, pal.muted)), note);
}

fn hints(frame: &mut Frame, area: Rect, theme: &Theme, view: &SyncView, keys: &[(&str, &str)]) {
    let pal = Palette::new(theme);
    let last = view.log.back().map(|l| truncate_width(&san(l), area.width.saturating_sub(2) as usize)).unwrap_or_default();
    let mut spans = vec![];
    for (k, d) in keys {
        spans.push(Span::styled(format!(" {k}"), Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {d} "), pal.muted));
    }
    frame.render_widget(Paragraph::new(vec![Line::from(Span::styled(format!(" {last}"), pal.muted)), Line::from(spans)]), area);
}

/// "Continue on which device?" — peers numbered 1-9.
pub(crate) fn render_handoff_picker(frame: &mut Frame, theme: &Theme, view: &SyncView) {
    let pal = Palette::new(theme);
    let area = super::centered(frame.area(), 64, (view.peers.len() as u16 + 8).min(16));
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(" continue this session on… ", pal.bold))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.processing));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let now = now_secs();
    let mut lines = vec![
        Line::from(Span::styled(" Moves the commits it lacks, your uncommitted changes and the", pal.muted)),
        Line::from(Span::styled(" conversation; the session resumes there in tmux. Repeat hand-offs", pal.muted)),
        Line::from(Span::styled(" only send what changed.", pal.muted)),
        Line::raw(""),
    ];
    for (i, p) in view.peers.iter().enumerate().take(9) {
        let (dot, fresh) = freshness(p, &pal, now);
        lines.push(Line::from(vec![
            Span::styled(format!(" [{}] ", i + 1), Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
            Span::styled("● ", Style::default().fg(dot)),
            Span::styled(format!("{:<18}", san(&p.name)), pal.bold),
            Span::styled(fresh, pal.muted),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(" [Esc] cancel", pal.muted)));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Status-bar summary: "⇄ pi 12s · 1 needs you".
pub(crate) fn status_hint(view: &SyncView) -> Option<String> {
    if !view.running && view.error.is_none() {
        return None;
    }
    if !view.running {
        return Some("⇄ sync off".into());
    }
    let now = now_secs();
    let peers: Vec<String> = view
        .peers
        .iter()
        .map(|p| match p.state_generated_at {
            Some(g) => format!("{} {}", san(&p.name), age_short(now.saturating_sub(g))),
            None => format!("{} –", san(&p.name)),
        })
        .collect();
    let pending: usize = view.peers.iter().filter_map(|p| p.state.as_ref()).map(|s| s.requests.len()).sum();
    let mut s = format!("⇄ {}", peers.join(" "));
    if pending > 0 {
        s.push_str(&format!(" · {pending} need{} you", if pending == 1 { "s" } else { "" }));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::protocol::{DeviceState, RemoteSession};

    fn view() -> SyncView {
        let st = DeviceState {
            sessions: vec![
                RemoteSession { id: "s1".into(), name: "demo".into(), state: "Idle".into(), injectable: true, ..Default::default() },
                RemoteSession { id: "s2".into(), name: "busy".into(), state: "Waiting".into(), via_mod: true, injectable: true, ..Default::default() },
            ],
            requests: vec![PendingRequest { id: "r1".into(), kind: "permission".into(), tool: "Write".into(), session: "s2".into(), created_at: now_secs() - 30, ..Default::default() }],
            generated_at: now_secs(),
            ..Default::default()
        };
        SyncView {
            running: true,
            peers: vec![
                PeerView { name: "mac".into(), ..Default::default() },
                PeerView { name: "pi".into(), state_generated_at: Some(now_secs() - 5), state: Some(st), ..Default::default() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn opens_on_the_device_that_needs_you_and_cycles() {
        let v = view();
        let mut ui = DevicesState::new();
        assert_eq!(ui.current(&v).unwrap().name, "pi");
        ui.cycle(&v, true);
        assert_eq!(ui.current(&v).unwrap().name, "mac");
        ui.cycle(&v, false);
        assert_eq!(ui.current(&v).unwrap().name, "pi");
    }

    #[test]
    fn requests_come_first_then_waiting_sessions() {
        let v = view();
        let r = rows(v.peers.get(1));
        assert!(matches!(&r[0], Row::Request { req, .. } if req.id == "r1"));
        assert!(matches!(&r[1], Row::Session { id, via_mod: true, .. } if id == "s2"));
        assert!(matches!(&r[2], Row::Session { id, .. } if id == "s1"));
        assert!(rows(v.peers.first()).is_empty(), "a device without state has no rows");
    }

    // Regression: a republished state with reordered sessions moved the
    // selection under the user, and `m` messaged a different session.
    #[test]
    fn selection_follows_the_row_not_the_index() {
        let mut v = view();
        let mut ui = DevicesState::new();
        ui.cursor = 2;
        assert!(matches!(ui.resolve(&rows(v.peers.get(1))), Some(Row::Session { ref id, .. }) if id == "s1"));
        let st = v.peers[1].state.as_mut().unwrap();
        st.sessions.insert(0, RemoteSession { id: "s0".into(), state: "Waiting".into(), ..Default::default() });
        assert!(matches!(ui.resolve(&rows(v.peers.get(1))), Some(Row::Session { ref id, .. }) if id == "s1"));
        assert_eq!(ui.cursor, 3);
    }

    #[test]
    fn status_hint_shows_age_and_pending() {
        let mut v = view();
        v.peers.remove(0);
        assert_eq!(status_hint(&v).unwrap(), "⇄ pi 5s · 1 needs you");
        assert_eq!(status_hint(&SyncView::default()), None);
    }

    #[test]
    fn renders_wide_and_phone_sized() {
        let theme = crate::tui::theme::Theme::catalog(None)[0].clone();
        for (w, h) in [(40, 12), (60, 30), (89, 30), (140, 40)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut term = ratatui::Terminal::new(backend).unwrap();
            let mut ui = DevicesState::new();
            term.draw(|f| render(f, &theme, &view(), &mut ui)).unwrap();
            let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
            assert!(text.contains("pi"), "{w}x{h}");
            assert!(text.contains("NEEDS YOU"), "{w}x{h} shows what needs you");
            if w <= 60 {
                assert!(!text.contains("can run:"), "{w}: phone layout drops the detail line");
            }
            if w < NARROW {
                assert!(text.contains("● mac") && text.contains("pi (1)"), "{w}: tab strip with the waiting count");
            }
        }
        let backend = ratatui::backend::TestBackend::new(60, 20);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &theme, &SyncView { running: true, ..Default::default() }, &mut DevicesState::new())).unwrap();
        let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("nerve setup"), "empty state says what to do");
    }
}
