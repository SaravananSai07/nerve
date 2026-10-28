//! Key handling for the Devices and hand-off overlays.

use crossterm::event::KeyCode;

use super::{App, Overlay};
use crate::sync::agent::Request;
use crate::sync::protocol::{Answer, Body, FetchWhat};
use crate::tui::devices::{rows, DevicesState, Mode, Row};

impl App {
    pub(super) fn sync_hint(&self) -> Option<String> {
        self.sync.as_ref().and_then(|h| crate::tui::devices::status_hint(&h.snapshot()))
    }

    pub(super) fn open_devices(&mut self) {
        if self.sync.is_none() {
            self.set_status_info("sync is not set up: run `nerve setup`");
            return;
        }
        self.overlay = Overlay::Devices(DevicesState::new());
    }

    pub(super) fn open_handoff(&mut self) {
        let Some(h) = &self.sync else {
            self.set_status_info("sync is not set up: run `nerve setup`");
            return;
        };
        if h.snapshot().peers.is_empty() {
            self.set_status_info("no other devices yet: run `nerve setup` to add one");
            return;
        }
        if let Some(s) = self.nth_filtered(self.selected) {
            self.overlay = Overlay::Handoff { id: s.id.clone(), cwd: s.cwd.clone() };
        }
    }

    pub(super) fn handle_handoff_key(&mut self, code: KeyCode) {
        let Overlay::Handoff { id, cwd } = &self.overlay else { return };
        match code {
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let (id, cwd) = (id.clone(), cwd.clone());
                let idx = c as usize - '1' as usize;
                let Some(h) = &self.sync else { return };
                let Some(peer) = h.snapshot().peers.get(idx).map(|p| p.name.clone()) else { return };
                h.request(Request::Handoff { session_id: id.as_str().to_string(), cwd, device: peer.clone(), continue_with: None, place: None, force: false });
                self.overlay = Overlay::None;
                self.set_status_info(format!("handing off to {peer}… (progress in D)"));
            }
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            _ => {}
        }
    }

    pub(super) fn handle_devices_key(&mut self, code: KeyCode) {
        let Some(h) = &self.sync else {
            self.overlay = Overlay::None;
            return;
        };
        let view = h.snapshot();
        let Overlay::Devices(ui) = &mut self.overlay else { return };
        let peer = ui.current(&view).cloned();
        let all = rows(peer.as_ref());
        let row = ui.resolve(&all);
        let mut requests = Vec::new();
        let mut note = None;
        match &mut ui.mode {
            Mode::Compose { device, session, buf, .. } => match code {
                KeyCode::Enter => {
                    let text = std::mem::take(buf);
                    if !text.trim().is_empty() {
                        requests.push(Request::To { device: device.clone(), body: Body::Send { session: session.clone(), text } });
                        note = Some(format!("message sent to {device}; the ack shows below"));
                    }
                    ui.mode = Mode::List;
                }
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if buf.chars().count() < 4000 => buf.push(c),
                _ => {}
            },
            Mode::Blob { scroll } => match code {
                KeyCode::Char('j') | KeyCode::Down => *scroll += 1,
                KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => *scroll += 20,
                KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
                KeyCode::Char('g') | KeyCode::Home => *scroll = 0,
                KeyCode::Esc | KeyCode::Char('q') => ui.mode = Mode::List,
                _ => {}
            },
            Mode::List => match (code, row) {
                (KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('D'), _) => {
                    self.overlay = Overlay::None;
                    return;
                }
                (KeyCode::Tab | KeyCode::Right | KeyCode::Char('L'), _) => ui.cycle(&view, true),
                (KeyCode::BackTab | KeyCode::Left | KeyCode::Char('H'), _) => ui.cycle(&view, false),
                (KeyCode::Char('j') | KeyCode::Down, _) => {
                    ui.cursor = (ui.cursor + 1).min(all.len().saturating_sub(1));
                    ui.key = all.get(ui.cursor).map(Row::key);
                }
                (KeyCode::Char('k') | KeyCode::Up, _) => {
                    ui.cursor = ui.cursor.saturating_sub(1);
                    ui.key = all.get(ui.cursor).map(Row::key);
                }
                (KeyCode::Char('r'), _) => {
                    requests.push(Request::SyncNow);
                    note = Some("syncing: republished here and asked every device to".into());
                }
                (KeyCode::Char('A'), _) => requests.push(Request::SetAway(!view.away)),
                (KeyCode::Char('m') | KeyCode::Enter, Some(Row::Session { device, id, injectable, via_mod })) => {
                    if injectable {
                        ui.mode = Mode::Compose { device, session: id, via_mod, buf: String::new() };
                    } else {
                        note = Some("that session has no terminal or mod to send input through".into());
                    }
                }
                (KeyCode::Char(c @ ('d' | 'l' | 'g')), Some(Row::Session { device, id, .. })) => {
                    let what = match c {
                        'd' => FetchWhat::Diff,
                        'l' => FetchWhat::Log,
                        _ => FetchWhat::Subagents,
                    };
                    requests.push(Request::To { device, body: Body::Fetch { session: id, what } });
                    ui.mode = Mode::Blob { scroll: 0 };
                }
                (KeyCode::Char('a') | KeyCode::Enter, Some(Row::Request { device, req })) if req.kind != "question" => {
                    requests.push(Request::To { device: device.clone(), body: Body::Answer { request: req.id, answer: Answer::Allow } });
                    note = Some(format!("allowed on {device}"));
                }
                (KeyCode::Char('x'), Some(Row::Request { device, req })) => {
                    let answer = Answer::Deny { reason: format!("denied from {}", view.me) };
                    requests.push(Request::To { device: device.clone(), body: Body::Answer { request: req.id, answer } });
                    note = Some(format!("denied on {device}"));
                }
                (KeyCode::Char(c), Some(Row::Request { device, req })) if req.kind == "question" && c.is_ascii_digit() => {
                    let i = (c as usize).wrapping_sub('1' as usize);
                    if let Some(label) = req.questions.first().and_then(|q| q.options.get(i)) {
                        note = Some(format!("answered '{label}' on {device}"));
                        let answer = Answer::Choose { labels: vec![label.clone()] };
                        requests.push(Request::To { device, body: Body::Answer { request: req.id, answer } });
                    }
                }
                _ => {}
            },
        }
        if let Some(h) = &self.sync {
            for r in requests {
                h.request(r);
            }
        }
        if let Some(n) = note {
            self.set_status_info(n);
        }
    }
}
