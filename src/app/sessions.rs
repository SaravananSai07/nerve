//! The Sessions page, end to end: its keys, search and prompt box, the
//! actions on the session under the cursor (here or on another device),
//! and its footer hints. Sync-only parts live in `fleet.rs`.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};

use super::{App, Confirm};
use crate::detect::claude;
use crate::platform::SessionTarget;
use crate::state::session::{SessionKind, SessionState};
use crate::tui::dialog::Dialog;
use crate::tui::editor::{Edit, Editor};
use crate::tui::model::{Band, RowKey, Status};

/// Lines one wheel notch scrolls the transcript.
pub(super) const WHEEL: usize = 3;
/// Lines PgUp and PgDn scroll it.
const PAGE: usize = 12;

/// `site` here, `site on studio` elsewhere: how messages name a session.
fn label(project: &str, key: &RowKey) -> String {
    match &key.device {
        Some(d) => format!("{project} on {d}"),
        None => project.to_string(),
    }
}

/// Bytes a prompt typed into the panel can run to.
const COMPOSE_MAX: usize = 16 * 1024;

impl App {
    pub(super) fn sessions_key(&mut self, code: KeyCode) {
        // A request under the cursor takes y, n and ↵.
        if let Some(key) = self.cur_row().filter(|r| r.status == Status::Request).map(|r| r.key.clone()) {
            match code {
                KeyCode::Char('y') => return self.decide(&key, true, None, None),
                KeyCode::Char('n') => return self.decide(&key, false, None, None),
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => return self.review(key),
                _ => {}
            }
        }
        // A session still starting takes nothing a running one does; one
        // that didn't start takes r, e and x.
        if let Some(row) = self.cur_row().filter(|r| matches!(r.status, Status::Starting | Status::NotStarted)) {
            let (key, failed) = (row.key.clone(), row.status == Status::NotStarted);
            match code {
                KeyCode::Char('r') if failed => return self.retry_start(&key),
                KeyCode::Char('e') if failed => return self.edit_start(&key),
                KeyCode::Char('x') if failed => return self.dismiss_start(&key),
                KeyCode::Char('1'..='9' | 'i' | 'n' | 'p' | 't' | 'x' | 'r' | 'P' | 'H') => {
                    return self.say(if failed {
                        "It didn't start: r retries it, e edits the prompt, x dismisses it."
                    } else {
                        "It hasn't started yet."
                    });
                }
                _ => {}
            }
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right if self.ui.cur.is_some() => self.ui.open = true,
            KeyCode::Char('h') | KeyCode::Left => self.ui.open = false,
            KeyCode::PageUp if self.ui.open => self.scroll_by(PAGE as isize),
            KeyCode::PageDown if self.ui.open => self.scroll_by(-(PAGE as isize)),
            KeyCode::Home if self.ui.open => self.ui.scroll.set(usize::MAX),
            KeyCode::End if self.ui.open => self.ui.scroll.set(0),
            KeyCode::Esc => {
                if self.ui.screen.take().is_none() {
                    if self.ui.open {
                        self.ui.open = false;
                    } else {
                        self.ui.find = None;
                    }
                }
            }
            KeyCode::Char('/') => {
                self.ui.find.get_or_insert_with(String::new);
                self.ui.finding = true;
                self.ui.open = false;
            }
            KeyCode::Char(c @ '1'..='9') => self.answer(c as usize - '1' as usize),
            KeyCode::Char('i') => self.start_compose(),
            KeyCode::Char('n') => self.open_notify(),
            KeyCode::Char('p') => self.private_key(),
            KeyCode::Char('t') => self.jump(),
            KeyCode::Char('x') => self.ask_stop(),
            KeyCode::Char('r') => self.start_rename(),
            KeyCode::Char('P') => self.ask_capture(),
            KeyCode::Char('H') => self.start_handoff(),
            KeyCode::Char('s') => self.start_session_dialog(),
            _ => {}
        }
    }

    fn move_cursor(&mut self, by: isize) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let i = self.ui.cur.as_ref().and_then(|k| order.iter().position(|o| o == k)).unwrap_or(0);
        let next = (i as isize + by).clamp(0, order.len() as isize - 1) as usize;
        if self.ui.cur.as_ref() != Some(&order[next]) {
            self.ui.cur = Some(order[next].clone());
            self.ui.screen = None;
        }
    }

    pub(super) fn find_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.ui.find = None;
                self.ui.finding = false;
            }
            KeyCode::Enter => {
                self.ui.finding = false;
                if self.ui.find.as_deref() == Some("") {
                    self.ui.find = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(q) = self.ui.find.as_mut() {
                    q.pop();
                }
            }
            KeyCode::Char(c) => self.ui.find.get_or_insert_with(String::new).push(c),
            _ => {}
        }
    }

    pub(super) fn compose_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(editor) = self.ui.compose.as_mut() else { return };
        match code {
            KeyCode::Esc => self.ui.compose = None,
            _ => {
                if editor.key(code, mods) == Edit::Submit {
                    let text = editor.take();
                    self.ui.compose = None;
                    if !text.trim().is_empty() {
                        self.send_prompt(text);
                    }
                }
            }
        }
    }

    /// Scroll the open panel up (positive) or down; the panel clamps it.
    pub(super) fn scroll_by(&mut self, by: isize) {
        self.ui.scroll.set(self.ui.scroll.get().saturating_add_signed(by));
    }

    pub(super) fn answer(&mut self, i: usize) {
        let Some(row) = self.cur_row().filter(|r| r.status.band() == Band::Need) else { return };
        let Some(ask) = &row.ask else {
            let msg = if row.is_local() {
                "Answer it in its terminal: press t."
            } else {
                "Answer it in its terminal on its device."
            };
            return self.say(msg);
        };
        let Some(choice) = ask.options.get(i).cloned() else { return };
        if self.ui.sending.as_ref().is_some_and(|(k, ..)| *k == row.key) {
            return;
        }
        let (key, request, permission, project) =
            (row.key.clone(), ask.request.clone(), ask.permission, row.project.clone());
        let to = key.device.clone().unwrap_or_else(|| "this device".into());
        if self.send_answer(&key, request, permission, &choice) {
            self.ui.sending = Some((key.clone(), format!("Sending to {to}…"), Instant::now()));
            self.say(format!("Answered {}: {choice}", label(&project, &key)));
        }
    }

    pub(super) fn start_compose(&mut self) {
        let Some(row) = self.cur_row() else { return };
        let msg = match (row.status, self.can_compose(), row.can_send) {
            (Status::Offline, ..) => {
                Some(format!("Can't reach {}. It's offline.", row.key.device.as_deref().unwrap_or("it")))
            }
            (_, false, _) => Some("Sending prompts needs sync: press c in Settings to connect a device.".to_string()),
            (_, true, false) => Some("This session has no terminal or mod to send a prompt through.".to_string()),
            _ => None,
        };
        match msg {
            Some(m) => self.say(m),
            None => {
                self.ui.open = true;
                self.ui.compose = Some(Editor::new(COMPOSE_MAX));
            }
        }
    }

    pub(super) fn send_prompt(&mut self, text: String) {
        let Some(row) = self.cur_row() else { return };
        let (key, project, working) = (row.key.clone(), row.project.clone(), row.status == Status::Working);
        if self.send_text(&key, text) {
            self.say(if working {
                "Queued. It sends when this turn ends.".to_string()
            } else {
                format!("Sent to {}", label(&project, &key))
            });
        }
    }

    pub(super) fn open_notify(&mut self) {
        let Some(row) = self.cur_row() else { return };
        let (key, project) = (row.key.clone(), row.project.clone());
        let set = self.notify_set(&key);
        let settings = self.notify_prefs().finished;
        let devices = self.devices();
        self.ui.dialog = Some(Dialog::Notify { key, project, devices, set, settings, i: 0 });
    }

    pub(super) fn save_notify(&mut self, d: Dialog) {
        let Dialog::Notify { key, project, set, .. } = d else { return };
        let on = !set.is_empty();
        let who: Vec<String> =
            set.iter().map(|d| if *d == self.me { "this device".to_string() } else { d.clone() }).collect();
        if self.set_notify(&key, set) {
            self.say(if on {
                format!("You'll hear when {project} finishes, on {}.", crate::util::text::list_join(&who))
            } else {
                format!("No notification when {project} finishes.")
            });
        }
    }

    pub(super) fn jump(&mut self) {
        let Some(row) = self.cur_row() else { return };
        if let Some(dev) = &row.key.device {
            return self.say(format!("Runs on {dev}, so its terminal is there."));
        }
        let Some(session) = self.registry.get(&row.key.id) else { return };
        match session.kind {
            SessionKind::Terminal => {
                let target = SessionTarget::from(session);
                if let Err(e) = self.bridge.go_to_session(&target) {
                    self.say(e.to_string());
                }
            }
            SessionKind::Desktop => {
                if let Err(e) = crate::platform::focus_desktop_app() {
                    self.say(e.to_string());
                }
            }
            SessionKind::Background => {
                // No tab to jump to: open `claude attach` in a split where
                // the terminal allows it, else hand over the command.
                let cmd = claude::attach_command(session.id.as_str(), &session.claude_root);
                let job = claude::job_id(session.id.as_str()).to_string();
                match self.bridge.open_command(&[&cmd]) {
                    Ok(true) => self.say(format!("Attached to background job {job}.")),
                    Ok(false) if crate::platform::copy_to_clipboard(&cmd) => {
                        self.say(format!("Copied {cmd}. Paste it in any terminal."))
                    }
                    Ok(false) => self.say(format!("Run {cmd} in any terminal.")),
                    Err(e) => self.say(e.to_string()),
                }
            }
        }
    }

    pub(super) fn ask_stop(&mut self) {
        let Some(row) = self.cur_row() else { return };
        let key = row.key.clone();
        let text = label(&row.project, &key);
        if key.device.is_none() {
            let Some(s) = self.registry.get(&key.id) else { return };
            // A Dormant session is still alive (a job parked for days, a
            // prompt left for days), so it can be stopped; the kill
            // re-checks that its pid is still claude.
            if *s.state() == SessionState::Vanished {
                return self.say("That session has already ended.");
            }
            if s.kind == SessionKind::Desktop {
                return self.say("A desktop session: close it in the Claude app.");
            }
        } else if row.status == Status::Offline {
            return self.say(format!("Can't reach {}. It's offline.", key.device.as_deref().unwrap_or("it")));
        }
        self.ui.confirm = Some(Confirm::Stop { key, label: text });
    }

    pub(super) fn stop(&mut self, key: &RowKey) {
        let Some(name) = self.row(key).map(|r| label(&r.project, key)) else { return };
        if key.device.is_some() {
            self.stop_remote(key);
            return self.say(format!("Asked {name} to stop."));
        }
        let Some(s) = self.registry.get(&key.id) else { return };
        let (id, root, kind) = (s.id.as_str().to_string(), s.claude_root.clone(), s.kind);
        // Both paths shell out (`claude stop`, or a fresh `ps` to
        // re-validate the pid), so they run off the UI thread.
        let done = move |r: Result<String, String>| match r {
            Ok(_) => format!("Stopped {name}."),
            Err(e) => format!("Couldn't stop {name}: {e}"),
        };
        match kind {
            SessionKind::Background => {
                self.run_action("Stopping…".into(), move || done(claude::stop_background_job(&id, &root)))
            }
            SessionKind::Terminal => self.run_action("Stopping…".into(), move || {
                done(claude::kill_by_session_id(&id).map(|pid| pid.to_string()))
            }),
            SessionKind::Desktop => {}
        }
    }

    pub(super) fn start_rename(&mut self) {
        let Some(row) = self.cur_row() else { return };
        if let Some(dev) = &row.key.device {
            return self.say(format!("Rename it on {dev}."));
        }
        let key = row.key.clone();
        let buf = row.title.clone();
        self.ui.dialog = Some(Dialog::Rename { key, buf });
    }

    pub(super) fn commit_rename(&mut self, d: Dialog) {
        let Dialog::Rename { key, buf } = d else { return };
        let name = buf.trim().to_string();
        if name.is_empty() {
            return;
        }
        if self.registry.name_taken(&name, &key.id) {
            return self.say(format!("Another session is already called {name}."));
        }
        if let Some(s) = self.registry.get_mut(&key.id) {
            s.rename_to(name);
        }
    }

    pub(super) fn ask_capture(&mut self) {
        let Some(row) = self.cur_row() else { return };
        let key = row.key.clone();
        let terminal =
            key.device.is_none() && self.registry.get(&key.id).is_some_and(|s| s.kind == SessionKind::Terminal);
        if !terminal || !self.bridge.is_active() {
            return self.say("Only a terminal tab on this device has a screen to capture.");
        }
        if self.prefs.preview_flicker_accepted {
            self.capture(&key);
        } else {
            self.ui.confirm = Some(Confirm::Capture { key });
        }
    }

    pub(super) fn capture(&mut self, key: &RowKey) {
        let Some(s) = self.registry.get(&key.id) else { return };
        match self.bridge.capture_screen(&SessionTarget::from(s)) {
            // Captured text crosses into this terminal: strip anything that
            // isn't plain text first.
            Some(text) => {
                let lines = text.lines().map(crate::util::sanitize::clean).collect();
                self.ui.screen = Some((key.clone(), lines));
                self.ui.open = true;
            }
            None => self.say("Couldn't capture that screen."),
        }
    }

    /// The footer's hints on the Sessions page.
    pub(super) fn sessions_hints(&self) -> Vec<(&'static str, String)> {
        let h = |k: &'static str, v: &str| (k, v.to_string());
        if self.ui.compose.is_some() {
            let working = self.cur_row().is_some_and(|r| r.status == Status::Working);
            return vec![h("↵", if working { "Queue" } else { "Send" }), h("⌥↵", "New line"), h("Esc", "Cancel")];
        }
        let Some(row) = self.cur_row() else {
            let mut v = if self.claude_installed { vec![h("/", "Find")] } else { Vec::new() };
            if self.can_compose() {
                v.push(h("s", "Start a session"));
            }
            v.extend([h("Tab", "Next page"), h("?", "Keys"), h("q", "Quit")]);
            return v;
        };
        if matches!(row.status, Status::Starting | Status::NotStarted) {
            let mut v: Vec<(&str, String)> = Vec::new();
            if row.status == Status::NotStarted {
                v.extend(crate::tui::sessions::START_FAILED.map(|(k, l)| h(k, l)));
            }
            v.push(if self.ui.open { h("Esc", "Back") } else { h("↵", "Open") });
            v.extend([h("/", "Find"), h("?", "Keys")]);
            return v;
        }
        if row.status == Status::Request {
            return vec![h("y", "Allow"), h("↵", "Review"), h("n", "Refuse"), h("/", "Find"), h("?", "Keys")];
        }
        let mut v = Vec::new();
        if let Some(ask) = row.ask.as_ref().filter(|_| row.status.band() == Band::Need) {
            let n = ask.options.len();
            v.push((if n > 1 { "1–9" } else { "1" }, "Answer".to_string()));
        }
        if !self.ui.open {
            v.push(h("↵", "Open"));
        } else if self.can_compose() && row.can_send {
            v.push(h("i", "Reply"));
        }
        v.push(h("n", if row.notify { "Notifying" } else { "Notify" }));
        if row.is_local() && self.can_private() {
            v.push(h("p", if row.private { "Share" } else { "Private" }));
        }
        if row.is_local() {
            v.push(h("t", "Terminal"));
        }
        if self.ui.open && row.status == Status::Working {
            v.push(h("x", "Stop"));
        }
        v.push(if self.ui.open || self.ui.screen.is_some() {
            h("Esc", "Back")
        } else if self.ui.find.is_some() {
            h("Esc", "Clear")
        } else {
            h("/", "Find")
        });
        v.extend([h("Tab", "Next page"), h("?", "Keys")]);
        v
    }
}
