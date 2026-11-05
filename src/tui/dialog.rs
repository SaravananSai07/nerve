//! Small boxes over the page: who to notify, keeping a session private,
//! renaming, handing off, starting a session on a device, syncing a
//! folder and its rules, answering a request, connecting a device, and
//! the list of keys. The page behind
//! dims.

use std::collections::BTreeSet;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::hints;
use super::model::RowKey;
use super::paint::para as paragraph;
use super::paint::{paint, Ink, Ln};
use crate::sync::folders::Others;

pub(crate) enum Dialog {
    Notify {
        key: RowKey,
        project: String,
        /// Every device, with a note (`This device`, `Offline, …`).
        devices: Vec<(String, &'static str)>,
        set: BTreeSet<String>,
        /// The devices ticked under Finished in Settings: choosing these
        /// follows Settings.
        settings: BTreeSet<String>,
        i: usize,
    },
    Keep {
        key: RowKey,
        project: String,
        path: String,
        folder: bool,
        /// The synced folder that keeping the folder private would stop.
        synced: Option<String>,
    },
    Rename {
        key: RowKey,
        buf: String,
    },
    Handoff {
        key: RowKey,
        project: String,
        peers: Vec<String>,
    },
    /// Start a session: on which device, in which folder there, and
    /// the prompt.
    Start {
        step: StartStep,
        /// Every device, with a note (`This device`, `Offline`).
        devices: Vec<(String, &'static str)>,
        i: usize,
        /// On the chosen device: a path, `~/…` or a place name.
        dir: String,
        prompt: super::editor::Editor,
        /// The failed start this one replaces, by session id.
        replaces: Option<String>,
    },
    AddFolder {
        step: FolderStep,
        /// (path, as shown, the project using it)
        cands: Vec<(std::path::PathBuf, String, String)>,
        pick: usize,
        /// Every device, with a note (`This device`, `Offline, …`).
        devices: Vec<(String, &'static str)>,
        set: BTreeSet<String>,
        when: usize,
        i: usize,
    },
    /// A request under the cursor: a joining device, to check by its
    /// `words`, or a folder, with where it lands and then its rules.
    Review {
        key: RowKey,
        title: String,
        detail: String,
        words: Option<String>,
        folder: Option<Landing>,
        step: ReviewStep,
        i: usize,
    },
    /// What the devices sharing a folder may sync next without asking.
    Rules {
        name: String,
        parent: String,
        i: usize,
    },
    /// Files a sync replaced in a folder: (version id, line).
    Versions {
        folder: String,
        list: Vec<(String, String)>,
        i: usize,
    },
    /// Who a new device joins (`link`: None) or an existing one meets.
    Who {
        link: Option<String>,
        devices: Vec<String>,
        set: BTreeSet<String>,
        mode: WhoMode,
        i: usize,
    },
    Connect(Connect),
    Keys,
}

/// Syncing a folder: which folder, which devices, when, and what those
/// devices may sync next without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderStep {
    Folder,
    Devices,
    When,
    Rules,
}

impl FolderStep {
    /// The step Esc goes back to; None from the first.
    pub(crate) fn back(self) -> Option<Self> {
        match self {
            Self::Folder => None,
            Self::Devices => Some(Self::Folder),
            Self::When => Some(Self::Devices),
            Self::Rules => Some(Self::When),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartStep {
    Device,
    Folder,
    Prompt,
}

/// A folder request: where it lands here, then what may follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewStep {
    Where,
    Rules,
}

/// Which devices `Dialog::Who` picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WhoMode {
    /// Every device, and ones added later.
    AllLater,
    /// Every device there is now.
    AllNow,
    /// The ticked ones.
    Picked,
}

/// Where a folder from another device lands here.
pub(crate) struct Landing {
    pub(crate) lands: String,
    /// Typed instead of `lands`.
    pub(crate) other: String,
}

impl Landing {
    /// The folder chosen: `lands`, or the typed one (option 1).
    pub(crate) fn spot(&self, i: usize) -> &str {
        if i == 1 && !self.other.trim().is_empty() {
            self.other.trim()
        } else {
            &self.lands
        }
    }
}

pub(crate) enum Connect {
    /// Show a code (0) or enter one (1).
    Choose(usize),
    /// Setting this device up before it shows a code: identity, Claude
    /// mod, background agent.
    Preparing,
    /// The code once the agent has made it (not `old`, the one it showed
    /// before). A join asked after `since` is the one this code brought.
    Code {
        code: Option<String>,
        since: u64,
        old: Option<String>,
    },
    Enter {
        buf: String,
        err: Option<String>,
    },
    /// Which of the devices the code offers to join.
    Pick {
        code: String,
        /// The device that showed the code: always joined.
        from: String,
        offer: Vec<String>,
        set: BTreeSet<String>,
        i: usize,
    },
    Joining {
        to: String,
        words: Option<String>,
        err: Option<String>,
    },
}

/// The keys list, also the source for `?`.
pub(crate) const KEYS: &[(&str, &str)] = &[
    ("j k  ↑ ↓", "Move"),
    ("↵", "Open the session"),
    ("PgUp PgDn", "Scroll the transcript"),
    ("1 to 9", "Answer the question"),
    ("i", "Write a prompt"),
    ("n", "Who hears when it finishes"),
    ("p", "Keep private"),
    ("t", "Jump to its terminal"),
    ("x", "Stop it"),
    ("r", "Rename"),
    ("P", "Capture its screen"),
    ("H", "Hand it to another device"),
    ("s", "Start a session on a device"),
    ("y  n", "Allow or refuse a request"),
    ("c", "Connect a device (Settings)"),
    ("/", "Find"),
    ("Tab", "Sessions, Sync, Settings"),
    ("Esc", "Back"),
    ("q", "Quit"),
];

pub(crate) fn render(buf: &mut Buffer, area: Rect, d: &Dialog, ink: &Ink) {
    // Dim the page behind: the box is the only thing to read now.
    for y in area.y + 1..area.y + area.height - 1 {
        for x in area.x..area.x + area.width {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_style(
                    Style::default().fg(dim_of(ink)).bg(ratatui::style::Color::Reset).remove_modifier(Modifier::BOLD),
                );
            }
        }
    }
    let width = match d {
        Dialog::Keys => 48,
        // Room to read and edit a long prompt.
        Dialog::Start { step: StartStep::Prompt, .. } => 84,
        _ => 62,
    }
    .min(area.width.saturating_sub(4));
    let iw = width.saturating_sub(4);
    let (title, body, foot) = content(d, ink, iw);
    let height = (body.len() + 4 + usize::from(foot.is_some())) as u16;
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height) / 2).max(2),
        width,
        height: height.min(area.height.saturating_sub(2)),
    };
    let mut lines = Vec::new();
    let mut top = Ln::new();
    let end = top.put(0, "╭─ ", ink.dim);
    let end = top.put(end, title, ink.bold);
    top.put(end, format!(" {}╮", "─".repeat(usize::from(width.saturating_sub(end + 2)))), ink.dim);
    lines.push(top);
    let side = |mut l: Ln| {
        l.put(0, "│", ink.dim);
        l.put(width - 1, "│", ink.dim);
        l
    };
    lines.push(side(Ln::new()));
    for b in body {
        lines.push(side(b));
    }
    lines.push(side(Ln::new()));
    if let Some(f) = foot {
        lines.push(side(f));
    }
    lines.push(Ln::text(0, format!("╰{}╯", "─".repeat(usize::from(width - 2))), ink.dim));
    // Clear the box first so the page doesn't show through blank cells.
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.reset();
            }
        }
    }
    paint(buf, rect, &lines);
}

/// `~/code` for `~/code/site`: what "folders inside" means for a folder.
pub(crate) fn parent_of(path: &str) -> String {
    std::path::Path::new(path).parent().map(|p| p.display().to_string()).unwrap_or_default()
}

fn radio_mark(on: bool) -> &'static str {
    if on {
        "(•)"
    } else {
        "( )"
    }
}

fn check_mark(on: bool) -> &'static str {
    if on {
        "[×]"
    } else {
        "[ ]"
    }
}

fn dim_of(ink: &Ink) -> ratatui::style::Color {
    ink.dim.fg.unwrap_or(ratatui::style::Color::DarkGray)
}

/// Body lines start at column 2 of the box (inside the border). Only
/// the sync dialogs wrap text to `iw`.
fn content(d: &Dialog, ink: &Ink, iw: u16) -> (String, Vec<Ln>, Option<Ln>) {
    let x = 2;
    let foot = |h: Vec<(&'static str, String)>| {
        let mut l = Ln::new();
        hints(&mut l, x, &h, ink);
        Some(l)
    };
    let choice = |sel: bool, mark: &str, on: bool, label: &str, note: &str| {
        let mut l = Ln::new();
        if sel {
            l.put(x, "›", ink.accent);
        }
        l.put(x + 2, mark, if on { ink.accent } else { ink.dim });
        let e = l.put(x + 6, label, if sel { ink.bold } else { ink.text });
        if !note.is_empty() {
            l.put(e + 2, note, ink.dim);
        }
        l
    };
    let para = |t: &str| paragraph(t, x, iw - 2, ink.dim);
    let radio = |sel: bool, label: &str| choice(sel, radio_mark(sel), sel, label, "");
    let rules = |i: usize, parent: String| -> Vec<Ln> {
        Others::ALL.iter().enumerate().map(|(n, o)| radio(n == i, &o.label(&parent))).collect()
    };
    match d {
        Dialog::Notify { project, devices, set, settings, i, .. } => {
            let mut body = vec![Ln::text(x, "Show the notification on", ink.dim), Ln::new()];
            for (n, (dev, note)) in devices.iter().enumerate() {
                let on = set.contains(dev);
                body.push(choice(n == *i, check_mark(on), on, dev, note));
            }
            body.push(Ln::new());
            body.extend(para(if set == settings {
                "Same as Settings, so it follows any change there."
            } else {
                "For this session only. Tick the same devices as Settings to follow Settings again."
            }));
            let save = if set.is_empty() { "Turn off" } else { "Save" };
            (
                format!("Notify when {project} finishes"),
                body,
                foot(vec![("Space", "Choose".into()), ("↵", save.into()), ("Esc", "Cancel".into())]),
            )
        }
        Dialog::Keep { project, path, folder, synced, .. } => {
            let mut body = para("Your other devices won't see it, and its notifications stay on this device.");
            body.push(Ln::new());
            body.push(choice(!folder, radio_mark(!folder), !folder, &format!("This session of {project}"), ""));
            body.push(choice(*folder, radio_mark(*folder), *folder, &format!("Everything in {path}"), ""));
            if let (true, Some(s)) = (*folder, synced) {
                body.push(Ln::new());
                body.push(Ln::text(x + 6, format!("Stops syncing {s}."), ink.attention));
            }
            ("Keep private".into(), body, foot(vec![("↵", "Keep private".into()), ("Esc", "Cancel".into())]))
        }
        Dialog::Rename { buf, .. } => {
            let mut l = Ln::new();
            let e = l.put(x, buf.as_str(), ink.text);
            l.put(e, " ", ink.text.add_modifier(Modifier::REVERSED));
            ("Rename".into(), vec![l], foot(vec![("↵", "Save".into()), ("Esc", "Cancel".into())]))
        }
        Dialog::Handoff { project, peers, .. } => {
            let mut body = para("It carries on there with its transcript, commits and uncommitted changes.");
            body.push(Ln::new());
            for (n, p) in peers.iter().enumerate().take(9) {
                let mut l = Ln::new();
                l.put(x, (n + 1).to_string(), ink.accent);
                l.put(x + 3, p.as_str(), ink.text);
                body.push(l);
            }
            (format!("Hand {project} to"), body, foot(vec![("1–9", "Hand off".into()), ("Esc", "Cancel".into())]))
        }
        Dialog::Start { step: StartStep::Device, devices, i, .. } => {
            let mut body = vec![Ln::text(x, "Which device should it run on?", ink.dim), Ln::new()];
            for (n, (dev, note)) in devices.iter().enumerate() {
                body.push(choice(n == *i, radio_mark(n == *i), n == *i, dev, note));
            }
            ("Start a session".into(), body, foot(vec![("↵", "Next".into()), ("Esc", "Cancel".into())]))
        }
        Dialog::Start { step: StartStep::Folder, devices, i, dir, .. } => {
            let device = devices.get(*i).map_or("", |d| d.0.as_str());
            let mut body = para(&format!("Which folder on {device}? ~ is its home; a place name works too."));
            body.push(Ln::new());
            let mut l = Ln::new();
            let tail: Vec<char> = dir.chars().rev().take(usize::from(iw.saturating_sub(6))).collect();
            let e = l.put(x, "› ", ink.accent);
            let e = l.put(e, tail.into_iter().rev().collect::<String>(), ink.text);
            l.put(e, " ", ink.text.add_modifier(Modifier::REVERSED));
            body.push(l);
            (format!("Start a session on {device}"), body, foot(vec![("↵", "Next".into()), ("Esc", "Back".into())]))
        }
        Dialog::Start { devices, i, dir, prompt, .. } => {
            let device = devices.get(*i).map_or("", |d| d.0.as_str());
            let mut body = para(&format!("What should Claude do in {dir}?"));
            body.push(Ln::new());
            // The field is a quiet bar down its left edge, as tall as the
            // text (5 to 12 rows); what scrolled off the top is counted under it.
            for mut l in prompt.lines(x, iw.saturating_sub(2), 5, 12, ink) {
                l.put(x, "▎", ink.accent);
                body.push(l);
            }
            body.push(match prompt.above() {
                0 => Ln::new(),
                1 => Ln::text(x + 2, "↑ 1 line above", ink.dim),
                n => Ln::text(x + 2, format!("↑ {n} lines above"), ink.dim),
            });
            (
                format!("Start a session on {device}"),
                body,
                foot(vec![("↵", "Start".into()), ("⌥↵", "New line".into()), ("Esc", "Back".into())]),
            )
        }
        Dialog::AddFolder { step: FolderStep::Folder, cands, i, .. } => {
            let mut body = vec![Ln::text(x, "Which folder?", ink.dim), Ln::new()];
            if cands.is_empty() {
                body.extend(para("Every folder with a session here is already synced or private."));
            }
            for (n, (_, shown, project)) in cands.iter().enumerate() {
                let mut l = Ln::new();
                if n == *i {
                    l.put(x, "›", ink.accent);
                }
                let e = l.fit(x + 2, shown, iw.saturating_sub(24), if n == *i { ink.bold } else { ink.text });
                l.fit(e.max(x + 24) + 2, &format!("Used by {project}"), iw.saturating_sub(e.max(x + 24) + 2), ink.dim);
                body.push(l);
            }
            ("Sync a folder".into(), body, foot(vec![("↵", "Next".into()), ("Esc", "Cancel".into())]))
        }
        Dialog::AddFolder { step: FolderStep::Devices, cands, pick, devices, set, i, .. } => {
            let mut body = vec![Ln::text(x, "Which devices keep a copy?", ink.dim), Ln::new()];
            for (n, (dev, note)) in devices.iter().enumerate() {
                let on = set.contains(dev);
                body.push(choice(n == *i, check_mark(on), on, dev, note));
            }
            (
                format!("Sync {}", cands[*pick].1),
                body,
                foot(vec![("Space", "Choose".into()), ("↵", "Next".into()), ("Esc", "Back".into())]),
            )
        }
        Dialog::AddFolder { step: FolderStep::When, cands, pick, i, .. } => {
            let mut body = vec![Ln::text(x, "When should it sync?", ink.dim), Ln::new()];
            for (n, w) in crate::sync::folders::When::ALL.iter().enumerate() {
                let on = n == *i;
                body.push(choice(on, radio_mark(on), on, w.label(), ""));
            }
            (format!("Sync {}", cands[*pick].1), body, foot(vec![("↵", "Next".into()), ("Esc", "Back".into())]))
        }
        Dialog::AddFolder { cands, pick, set, i, .. } => {
            let path = &cands[*pick].1;
            let devices: Vec<String> = set.iter().cloned().collect();
            let mut body =
                para(&format!("If {} ask to sync other folders between them", crate::util::text::list_join(&devices)));
            body.push(Ln::new());
            body.extend(rules(*i, parent_of(path)));
            body.push(Ln::new());
            body.extend(para(match i {
                0 => "They wait until you say yes, here or on any of your devices.",
                1 => "Folders elsewhere still wait for your yes.",
                _ => "Private folders, and ones nerve never syncs, still stay out.",
            }));
            (format!("Sync {path}"), body, foot(vec![("↵", "Start syncing".into()), ("Esc", "Back".into())]))
        }
        Dialog::Review { title, detail, words, folder: None, .. } => {
            let mut body = para(detail);
            body.push(Ln::new());
            body.extend(para("Connect only if it shows these words:"));
            body.push(Ln::new());
            body.push(Ln::text(x + 4, words.as_deref().unwrap_or_default().replace(' ', "  ·  "), ink.bold));
            body.push(Ln::new());
            body.extend(para("Once connected, it sees every session on your devices that isn't private, and can answer and stop them."));
            (title.clone(), body, foot(vec![("y", "Connect".into()), ("n", "Refuse".into()), ("Esc", "Later".into())]))
        }
        Dialog::Review { title, detail, folder: Some(f), step: ReviewStep::Where, i, .. } => {
            let mut body = para(detail);
            body.push(Ln::new());
            body.push(Ln::text(x, "Where should it go?", ink.dim));
            body.push(Ln::new());
            body.push(choice(*i == 0, radio_mark(*i == 0), *i == 0, &f.lands, ""));
            let mut other = choice(
                *i == 1,
                radio_mark(*i == 1),
                *i == 1,
                if f.other.is_empty() { "Somewhere else" } else { &f.other },
                "",
            );
            if *i == 1 {
                let e = x + 6 + super::paint::width(if f.other.is_empty() { "Somewhere else" } else { &f.other });
                other.put(e, " ", ink.text.add_modifier(Modifier::REVERSED));
            }
            body.push(other);
            (title.clone(), body, foot(vec![("↵", "Next".into()), ("n", "Refuse".into()), ("Esc", "Later".into())]))
        }
        Dialog::Review { title, folder: Some(f), i, .. } => {
            let mut body = para("If its devices ask to sync other folders here later");
            body.push(Ln::new());
            body.extend(rules(*i, parent_of(f.spot(0))));
            (title.clone(), body, foot(vec![("↵", "Allow and sync".into()), ("Esc", "Back".into())]))
        }
        Dialog::Rules { name, parent, i } => {
            let mut body = para("If its devices ask to sync other folders between them");
            body.push(Ln::new());
            body.extend(rules(*i, parent.clone()));
            (format!("Other folders near {name}"), body, foot(vec![("↵", "Save".into()), ("Esc", "Cancel".into())]))
        }
        Dialog::Versions { folder, list, i } => {
            let mut body = para("Each sync keeps what it replaces or deletes for a while. Put one back:");
            body.push(Ln::new());
            if list.is_empty() {
                body.extend(para("Nothing yet."));
            }
            for (n, (_, line)) in list.iter().enumerate().take(12) {
                let mut l = Ln::new();
                if n == *i {
                    l.put(x, "›", ink.accent);
                }
                l.fit(x + 2, line, iw.saturating_sub(4), if n == *i { ink.bold } else { ink.text });
                body.push(l);
            }
            (
                format!("Earlier versions in {folder}"),
                body,
                foot(vec![("↵", "Put back".into()), ("Esc", "Close".into())]),
            )
        }
        Dialog::Who { link, devices, set, mode, i } => {
            let mut body = para(&match link {
                None => "Which of your devices should the new one join?".to_string(),
                Some(d) => format!("Which of your devices should {d} connect to?"),
            });
            body.push(Ln::new());
            let modes = [(WhoMode::AllLater, "All, and devices I add later"), (WhoMode::AllNow, "All my devices now")];
            for (n, (m, label)) in modes.into_iter().enumerate() {
                let on = *mode == m;
                body.push(choice(*i == n, radio_mark(on), on, label, ""));
            }
            for (n, d) in devices.iter().enumerate() {
                let on = *mode == WhoMode::Picked && set.contains(d);
                body.push(choice(*i == n + 2, check_mark(on), on, d, ""));
            }
            let title = link.as_ref().map_or("Connect a device".to_string(), |d| format!("Connect {d}"));
            (title, body, foot(vec![("Space", "Choose".into()), ("↵", "Next".into()), ("Esc", "Cancel".into())]))
        }
        Dialog::Connect(c) => {
            let title = "Connect a device".to_string();
            match c {
                Connect::Choose(i) => {
                    let mut body = para(
                        "Connected devices see and answer each other's sessions, except the ones you keep private.",
                    );
                    body.push(Ln::new());
                    body.push(radio(*i == 0, "Show a code for the other device to enter"));
                    body.push(radio(*i == 1, "Enter a code from the other device"));
                    (title, body, foot(vec![("↵", "Next".into()), ("Esc", "Cancel".into())]))
                }
                Connect::Preparing => (
                    title,
                    para("Setting this device up: its key, the Claude Code mod and the background agent…"),
                    foot(vec![("Esc", "Cancel".into())]),
                ),
                Connect::Code { code: None, .. } => {
                    (title, para("Making a code…"), foot(vec![("Esc", "Cancel".into())]))
                }
                Connect::Code { code: Some(code), .. } => {
                    let mut body = para("On the other device, open nerve, press c in Settings and enter this code. Or run nerve connect with it.");
                    body.push(Ln::new());
                    let chunk = usize::from(iw.saturating_sub(4)).max(1);
                    for part in code.as_bytes().chunks(chunk) {
                        body.push(Ln::text(x + 2, String::from_utf8_lossy(part).into_owned(), ink.accent));
                    }
                    body.push(Ln::new());
                    body.extend(para(
                        "It works once, for 10 minutes. Closing this cancels it. Waiting for the other device…",
                    ));
                    if let Some(note) = crate::sync::connect::Invite::decode(code).ok().and_then(|i| i.reach_note()) {
                        body.push(Ln::new());
                        body.extend(paragraph(&note, x, iw - 2, ink.attention));
                    }
                    (title, body, foot(vec![("c", "Copy".into()), ("Esc", "Cancel".into())]))
                }
                Connect::Enter { buf, err } => {
                    let mut body = para("Paste the code the other device shows.");
                    body.push(Ln::new());
                    // Wrapped whole, so a paste can be checked end to end.
                    let chunk = usize::from(iw.saturating_sub(6)).max(1);
                    let chars: Vec<char> = buf.chars().collect();
                    let mut parts: Vec<String> = chars.chunks(chunk).map(|p| p.iter().collect()).collect();
                    if parts.last().is_none_or(|p| p.chars().count() == chunk) {
                        parts.push(String::new());
                    }
                    let last = parts.len() - 1;
                    for (n, part) in parts.into_iter().enumerate() {
                        let mut l = Ln::new();
                        if n == 0 {
                            l.put(x, "› ", ink.accent);
                        }
                        let e = l.put(x + 2, part, ink.text);
                        if n == last {
                            l.put(e, " ", ink.text.add_modifier(Modifier::REVERSED));
                        }
                        body.push(l);
                    }
                    if let Some(e) = err {
                        body.push(Ln::new());
                        body.extend(paragraph(e, x, iw - 2, ink.attention));
                    }
                    (title, body, foot(vec![("↵", "Connect".into()), ("Esc", "Back".into())]))
                }
                Connect::Pick { from, offer, set, i, .. } => {
                    let mut body = para("Which of these devices should this one join?");
                    body.push(Ln::new());
                    let mut l = Ln::new();
                    l.put(x + 2, check_mark(true), ink.dim);
                    let e = l.put(x + 6, from, ink.dim);
                    l.put(e + 2, "showed the code", ink.dim);
                    body.push(l);
                    for (n, d) in offer.iter().enumerate() {
                        let on = set.contains(d);
                        body.push(choice(n == *i, check_mark(on), on, d, ""));
                    }
                    (
                        title,
                        body,
                        foot(vec![("Space", "Choose".into()), ("↵", "Connect".into()), ("Esc", "Back".into())]),
                    )
                }
                Connect::Joining { to, words, err } => {
                    let mut body = vec![Ln::text(x, format!("Connecting to {to}…"), ink.bold), Ln::new()];
                    if let (Some(w), None) = (words, err) {
                        body.extend(para(&format!("Check that {to} shows these words, and allow it there:")));
                        body.push(Ln::new());
                        body.push(Ln::text(x + 4, w.replace(' ', "  ·  "), ink.bold));
                    }
                    if let Some(e) = err {
                        body.push(Ln::new());
                        body.extend(paragraph(e, x, iw - 2, ink.attention));
                    }
                    (title, body, foot(vec![("Esc", if err.is_some() { "Close" } else { "Hide" }.into())]))
                }
            }
        }
        Dialog::Keys => {
            let body = KEYS
                .iter()
                .map(|(k, v)| {
                    let mut l = Ln::text(x, *k, ink.accent);
                    l.put(x + 12, *v, ink.text);
                    l
                })
                .collect();
            ("Keys".into(), body, foot(vec![("Esc", "Close".into())]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(d: &Dialog) -> String {
        let theme = &super::super::theme::Theme::catalog(None)[0];
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, d, &Ink::new(theme));
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn connecting_shows_the_code_then_the_words_to_check() {
        let s = screen(&Dialog::Connect(Connect::Code {
            code: Some(format!("nerve1{}", "a".repeat(120))),
            since: 0,
            old: None,
        }));
        assert!(s.contains("nerve1aaaa") && s.contains("It works once, for 10 minutes") && s.contains("c Copy"), "{s}");
        let s = screen(&Dialog::Connect(Connect::Joining {
            to: "laptop".into(),
            words: Some("oasis grape garlic".into()),
            err: None,
        }));
        assert!(s.contains("Connecting to laptop") && s.contains("oasis  ·  grape  ·  garlic"), "{s}");
        let s = screen(&Dialog::Connect(Connect::Joining {
            to: "laptop".into(),
            words: Some("oasis grape garlic".into()),
            err: Some("not connected: can't reach laptop's relay".into()),
        }));
        assert!(s.contains("can't reach") && !s.contains("oasis"), "a failed join hides the words: {s}");
        let s = screen(&Dialog::Connect(Connect::Pick {
            code: String::new(),
            from: "laptop".into(),
            offer: vec!["pi".into()],
            set: BTreeSet::new(),
            i: 0,
        }));
        assert!(s.contains("[×] laptop  showed the code") && s.contains("[ ] pi"), "{s}");
        let code = format!("nerve1{}", "b".repeat(200));
        let s = screen(&Dialog::Connect(Connect::Enter { buf: code, err: None }));
        assert_eq!(s.matches('b').count(), 200, "the whole pasted code shows: {s}");
        let key = RowKey { device: None, id: "q".into() };
        let s = screen(&Dialog::Review {
            key,
            title: "pi wants to connect".into(),
            detail: "Joins laptop.".into(),
            words: Some("oasis grape garlic".into()),
            folder: None,
            step: ReviewStep::Where,
            i: 0,
        });
        assert!(
            s.contains("pi wants to connect")
                && s.contains("oasis  ·  grape  ·  garlic")
                && s.contains("y Connect")
                && s.contains("n Refuse"),
            "{s}"
        );
    }

    #[test]
    fn starting_a_session_asks_device_folder_then_prompt() {
        let start = |step, dir: &str, prompt: &str| Dialog::Start {
            step,
            devices: vec![("laptop".into(), "This device"), ("beta".into(), "")],
            i: 1,
            dir: dir.into(),
            prompt: crate::tui::editor::Editor::with_text(prompt, 1000),
            replaces: None,
        };
        let s = screen(&start(StartStep::Device, "~", ""));
        assert!(
            s.contains("Start a session") && s.contains("( ) laptop  This device") && s.contains("(•) beta"),
            "{s}"
        );
        let s = screen(&start(StartStep::Folder, "~/code/app", ""));
        assert!(
            s.contains("Start a session on beta") && s.contains("Which folder on beta?") && s.contains("› ~/code/app"),
            "{s}"
        );
        let s = screen(&start(StartStep::Prompt, "~/code/app", "fix the tests"));
        assert!(s.contains("in ~/code/app?") && s.contains("▎ fix the tests") && s.contains("↵ Start"), "{s}");
        let long = "Read the failing test output and fix whatever broke in the sync agent, \
                    then run the whole suite again.\nCommit when it passes.";
        let s = screen(&start(StartStep::Prompt, "~/code/app", long));
        assert!(
            s.contains("▎ Read the failing")
                && s.contains("▎ agent, then run the whole")
                && s.contains("▎ Commit when it passes."),
            "a long prompt wraps, and keeps its own lines: {s}"
        );
    }

    #[test]
    fn a_folder_request_asks_where_then_what_may_follow() {
        let key = RowKey { device: None, id: "q".into() };
        let folder = |step, i| Dialog::Review {
            key: key.clone(),
            title: "studio wants to sync app".into(),
            detail: "Lands at ~/code/app on laptop.".into(),
            words: None,
            folder: Some(Landing { lands: "~/code/app".into(), other: String::new() }),
            step,
            i,
        };
        let s = screen(&folder(ReviewStep::Where, 0));
        assert!(
            s.contains("Where should it go?") && s.contains("(•) ~/code/app") && s.contains("( ) Somewhere else"),
            "{s}"
        );
        let s = screen(&folder(ReviewStep::Rules, 1));
        assert!(s.contains("(•) Allow folders inside ~/code") && s.contains("Allow and sync"), "{s}");
        let who = Dialog::Who {
            link: None,
            devices: vec!["studio".into()],
            set: BTreeSet::new(),
            mode: WhoMode::AllNow,
            i: 0,
        };
        let s = screen(&who);
        assert!(
            s.contains("( ) All, and devices I add later")
                && s.contains("(•) All my devices now")
                && s.contains("[ ] studio"),
            "{s}"
        );
        assert_eq!(Landing { lands: "~/a".into(), other: " /srv/a ".into() }.spot(1), "/srv/a");
        assert_eq!(Landing { lands: "~/a".into(), other: "  ".into() }.spot(1), "~/a", "nothing typed keeps the offer");
    }
}
