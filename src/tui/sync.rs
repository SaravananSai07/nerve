//! The Sync page: the folders kept the same across devices, how each
//! stands, and the last files that moved.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::model::{short_age, RowKey, OFFLINE_AFTER_SECS};
use super::paint::para;
use super::paint::{paint, window, Ink, Ln};
use super::theme::Theme;
use crate::sync::folders::{self, Moved};
use crate::sync::protocol::FolderStatus;
use crate::util::sanitize::clean;

pub(crate) struct SyncPage<'a> {
    pub(crate) me: &'a str,
    /// Sync is set up and its agent running.
    pub(crate) running: bool,
    /// This device's folders, as its agent last reported them.
    pub(crate) folders: &'a [FolderStatus],
    pub(crate) cursor: usize,
    pub(crate) bulk_over: u64,
    /// The other devices: name and seconds since each last reported.
    pub(crate) peers: Vec<(String, Option<u64>)>,
    pub(crate) now: u64,
    /// Folders waiting for a person's yes: (folder, "mini → laptop").
    pub(crate) waiting: Vec<(String, String)>,
}

/// `━━━━──────` for a share done.
fn bar(l: &mut Ln, x: u16, n: u16, done: f64, ink: &Ink) -> u16 {
    let k = ((f64::from(n) * done).round() as u16).min(n);
    l.put(x, "━".repeat(usize::from(k)), ink.accent);
    l.put(x + k, "─".repeat(usize::from(n - k)), ink.dim);
    x + n
}

/// Folders waiting for a person's yes, answered under Needs you.
fn waiting(p: &SyncPage, ink: &Ink, c_dev: u16, c_when: u16) -> Vec<Ln> {
    if p.waiting.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Ln::new(), Ln::text(3, "Waiting for a yes", ink.dim)];
    for (name, route) in &p.waiting {
        let mut l = Ln::new();
        l.fit(3, name, 12, ink.text);
        let x = l.fit(c_dev, route, c_when - c_dev - 2, ink.dim);
        l.put(x.max(c_when), "Answer it under Needs you", ink.attention);
        lines.push(l);
    }
    lines
}

/// No folders yet: the devices there are, and what to do next.
fn empty(p: &SyncPage, ink: &Ink) -> Vec<Ln> {
    let mut out = vec![Ln::new(), Ln::text(3, "Devices", ink.dim)];
    let mut device = |name: &str, state: &str, style| {
        let mut l = Ln::new();
        l.fit(3, name, 12, style);
        l.put(17, state, ink.dim);
        out.push(l);
    };
    device(&clean(p.me), "this device", ink.bold);
    for (name, age) in &p.peers {
        let state = match age {
            None => "not seen yet".to_string(),
            Some(a) if *a < OFFLINE_AFTER_SECS => "online".to_string(),
            Some(a) => format!("last seen {} ago", short_age(*a)),
        };
        device(&clean(name), &state, ink.text);
    }
    if p.peers.is_empty() {
        out.extend([Ln::new(), Ln::text(3, "Press c in Settings to connect a device.", ink.dim)]);
    }
    let note = if p.running { "No folders are synced yet." } else { "Sync is not running." };
    out.extend([Ln::new(), Ln::text(3, note, ink.text)]);
    out
}

pub(crate) fn render(buf: &mut Buffer, area: Rect, p: &SyncPage, ink: &Ink, theme: &Theme) {
    let w = area.width;
    let wide = w >= 100;
    let mut lines = vec![Ln::text(3, "Sync", ink.bold)];
    if p.folders.is_empty() {
        lines.extend(empty(p, ink));
        lines.extend(waiting(p, ink, 17, 42));
        return paint(buf, area, &window(lines, usize::from(area.height)));
    }
    lines.extend([
        Ln::text(3, "These folders stay the same on the devices you choose.", ink.dim),
        Ln::new(),
        Ln::text(3, "Folders", ink.dim),
    ]);
    let (c_dev, c_when) = (17, if wide { 42 } else { 40 });
    let c_state = if wide { 62 } else { 40 };
    for (i, f) in p.folders.iter().enumerate() {
        let Some(v) = &f.mine else { continue };
        let key = RowKey { device: None, id: format!("folder:{}", f.name) };
        let sel = i == p.cursor;
        let mut l = Ln::new().with_key(&key);
        if sel {
            l.select(ink, theme.selection);
        }
        l.fit(3, &clean(&f.name), 12, if sel { ink.bold } else { ink.text });
        let devices: Vec<String> = std::iter::once(p.me.to_string()).chain(v.with.iter().map(clean)).collect();
        l.fit(c_dev, &devices.join(" ⇄ "), c_when - c_dev - 2, ink.dim);
        if wide {
            l.put(c_when, v.when.label(), ink.dim);
        }
        let mut extra = Vec::new();
        if let Some(m) = &v.moving {
            if m.total > 0 {
                let share = m.done as f64 / m.total as f64;
                let x = bar(&mut l, c_state, if wide { 20 } else { 14 }, share, ink);
                l.put(x + 2, format!("{}%", (share * 100.0).round()), ink.text);
            } else {
                l.put(c_state, if m.direct { "Copying directly" } else { "Starting" }, ink.dim);
            }
            let mut b = Ln::new().with_key(&key);
            b.bg = l.bg;
            let what = format!(
                "{}  {}  {} → {}",
                clean(&m.what),
                crate::util::text::format_bytes(m.bytes),
                clean(&m.from),
                clean(&m.to)
            );
            b.fit(c_dev, &what, w.saturating_sub(c_dev + 2), ink.dim);
            extra.push(b);
        } else {
            let (text, attention) = folders::status(v, p.now);
            l.fit(
                c_state,
                &clean(&text),
                w.saturating_sub(c_state + 14),
                if attention { ink.attention } else { ink.dim },
            );
            if sel && (!v.behind.is_empty() || v.when == folders::When::Manual) {
                let x = l.rput(w - 2, "Sync now", ink.dim);
                l.put(x - 2, "s", ink.text);
            }
        }
        if !wide && sel {
            let mut b = Ln::new().with_key(&key);
            b.bg = l.bg;
            b.put(c_dev, v.when.label(), ink.dim);
            extra.push(b);
        }
        if sel {
            let mut b = Ln::new().with_key(&key);
            b.bg = l.bg;
            let parent = std::path::Path::new(&f.path)
                .parent()
                .map(|p| crate::paths::tilde(&p.display().to_string()))
                .unwrap_or_default();
            b.fit(c_dev, &clean(v.others.short(&parent)), w.saturating_sub(c_dev + 14), ink.dim);
            let x = b.rput(w - 2, "Change", ink.dim);
            b.put(x - 2, "o", ink.text);
            extra.push(b);
        }
        lines.push(l);
        lines.extend(extra);
    }
    lines.extend(waiting(p, ink, c_dev, c_when));
    lines.push(Ln::new());
    lines.push(Ln::text(3, "What went where", ink.dim));
    let chosen = p.folders.get(p.cursor).map(|f| f.name.as_str());
    // `None`: changes held back for a device that is away, shown first.
    let mut recent: Vec<(&str, Option<&Moved>)> = Vec::new();
    for f in p.folders {
        let Some(v) = &f.mine else { continue };
        if v.unsent > 0 && !v.offline.is_empty() {
            recent.push((f.name.as_str(), None));
        }
        recent.extend(v.log.iter().map(|m| (f.name.as_str(), Some(m))));
    }
    recent.sort_by_key(|(_, m)| std::cmp::Reverse(m.map_or(u64::MAX, |m| m.at)));
    if recent.is_empty() {
        lines.push(Ln::text(3, "Nothing has moved yet.", ink.dim));
    }
    let (c_what, c_fold) = (9, if wide { 40 } else { 0 });
    let c_route = if wide { 52 } else { 36 };
    let c_size = c_route + if wide { 30 } else { 22 };
    for (name, m) in recent.into_iter().take(8) {
        let mut l = Ln::new();
        let base = if chosen == Some(name) { ink.text } else { ink.dim };
        match m {
            None => {
                let v = p.folders.iter().find(|f| f.name == name).and_then(|f| f.mine.as_ref());
                let n = v.map_or(0, |v| v.unsent);
                l.put(c_what, if n == 1 { "1 change".to_string() } else { format!("{n} changes") }, base);
                let to = v.map(|v| v.offline.iter().map(clean).collect::<Vec<_>>()).unwrap_or_default();
                let x = l.put(c_route, format!("{} → {}", p.me, to.join(", ")), base);
                l.put(
                    x + 2,
                    if wide { format!("Waiting for {}", crate::util::text::list_join(&to)) } else { "Waiting".into() },
                    ink.attention,
                );
            }
            Some(m) => {
                l.rput(7, short_age(p.now.saturating_sub(m.at)), ink.dim);
                l.fit(c_what, &clean(&m.what), (if wide { c_fold } else { c_route }) - c_what - 2, base);
                l.fit(c_route, &format!("{} → {}", clean(&m.from), clean(&m.to)), c_size - c_route - 2, base);
                l.rput(c_size + 8, crate::util::text::format_bytes(m.bytes), base);
                if m.direct {
                    l.put(c_size + 10, "Direct", ink.dim);
                }
            }
        }
        if wide {
            l.fit(c_fold, &clean(name), 10, ink.dim);
        }
        lines.push(l);
    }
    lines.push(Ln::new());
    lines.extend(para(
        &format!(
            "Files over {} go directly between devices. Smaller changes travel through the relays.",
            crate::util::text::format_bytes(p.bulk_over)
        ),
        3,
        w - 6,
        ink.dim,
    ));
    paint(buf, area, &window(lines, usize::from(area.height)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::folders::{Moving, View, When};

    fn screen(p: &SyncPage, w: u16) -> String {
        let theme = &Theme::catalog(None)[0];
        let area = Rect::new(0, 0, w, 24);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, p, &Ink::new(theme), theme);
        (0..area.height)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn folders_show_their_devices_state_and_what_moved() {
        let moved = |at, what: &str, from: &str, to: &str, bytes, direct| Moved {
            at,
            what: what.into(),
            from: from.into(),
            to: to.into(),
            bytes,
            direct,
        };
        let site = View {
            when: When::Turn,
            with: vec!["studio".into()],
            last_sync: 880,
            log: vec![moved(880, "header.css", "studio", "laptop", 6 << 10, false)],
            ..Default::default()
        };
        let ios = View {
            when: When::Watch,
            with: vec!["mini".into()],
            behind: vec![("mini".into(), 3)],
            ..Default::default()
        };
        let train = View {
            when: When::Manual,
            with: vec!["server".into()],
            moving: Some(Moving {
                what: "run.bin".into(),
                from: "server".into(),
                to: "laptop".into(),
                bytes: 3 << 30,
                done: 1,
                total: 4,
                direct: false,
            }),
            log: vec![moved(500, "big.bin", "server", "laptop", 2 << 30, true)],
            ..Default::default()
        };
        let media = View {
            when: When::Watch,
            with: vec!["pi".into()],
            offline: vec!["pi".into()],
            unsent: 2,
            ..Default::default()
        };
        let folders: Vec<FolderStatus> = [("site", site), ("ios-app", ios), ("train", train), ("media", media)]
            .into_iter()
            .map(|(n, v)| FolderStatus { name: n.into(), path: format!("/w/{n}"), mine: Some(v), ..Default::default() })
            .collect();
        let waiting = vec![("app-runs".to_string(), "mini → laptop".to_string())];
        let p = SyncPage {
            me: "laptop",
            running: true,
            folders: &folders,
            cursor: 1,
            bulk_over: 64 << 20,
            peers: Vec::new(),
            now: 1000,
            waiting,
        };
        let s = screen(&p, 120);
        assert!(s.contains("laptop ⇄ studio") && s.contains("When a turn ends"), "{s}");
        assert!(s.contains("In sync, 2m ago"), "{s}");
        assert!(
            s.contains("3 newer files on mini") && s.contains("s Sync now"),
            "the chosen folder offers to sync:\n{s}"
        );
        assert!(s.contains("25%") && s.contains("run.bin  3.0 GiB  server → laptop"), "{s}");
        assert!(s.contains("Waiting for pi"), "{s}");
        assert!(s.contains("Direct") && s.contains("header.css"), "{s}");
        assert!(s.contains("Files over 64.0 MiB go directly"), "{s}");
        assert!(s.contains("Other folders ask first") && s.contains("o Change"), "the chosen folder's rules:\n{s}");
        assert!(
            s.contains("Waiting for a yes") && s.contains("mini → laptop") && s.contains("Answer it under Needs you"),
            "{s}"
        );
        // Narrow: the when moves under the chosen folder.
        assert!(screen(&p, 80).contains("When files change"));
    }

    #[test]
    fn empty_page_lists_this_device_and_the_others() {
        let p = SyncPage {
            me: "laptop",
            running: true,
            folders: &[],
            cursor: 0,
            bulk_over: 1 << 20,
            peers: Vec::new(),
            now: 0,
            waiting: Vec::new(),
        };
        let s = screen(&p, 60);
        assert!(
            s.contains("laptop        this device") && s.contains("Press c in Settings to connect a device."),
            "{s}"
        );
        assert!(s.contains("No folders are synced yet.") && !s.contains("Choose a folder"), "{s}");
        let p = SyncPage { peers: vec![("studio".into(), Some(5)), ("pi".into(), Some(600))], ..p };
        let s = screen(&p, 60);
        assert!(s.contains("studio        online") && s.contains("pi            last seen 10m ago"), "{s}");
        assert!(!s.contains("Press c"), "{s}");
        let p = SyncPage { running: false, ..p };
        assert!(screen(&p, 60).contains("Sync is not running."));
    }
}
