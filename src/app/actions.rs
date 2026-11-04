//! What the keys do. Each action works on the session under the cursor,
//! here or on another device; the sync-only parts live in `fleet.rs`.

use std::time::Instant;

use super::input::toggle;
use super::{App, Confirm};
use crate::detect::claude;
use crate::platform::SessionTarget;
use crate::state::session::{SessionKind, SessionState};
use crate::tui::dialog::Dialog;
use crate::tui::model::{Band, RowKey, Status};

/// `site` here, `site on studio` elsewhere: how messages name a session.
fn label(project: &str, key: &RowKey) -> String {
    match &key.device {
        Some(d) => format!("{project} on {d}"),
        None => project.to_string(),
    }
}

impl App {
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
                self.ui.compose = Some(String::new());
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

    pub(super) fn cycle_theme(&mut self, forward: bool) {
        let n = self.themes.len();
        self.theme_index = if forward { (self.theme_index + 1) % n } else { (self.theme_index + n - 1) % n };
        self.prefs.theme = Some(self.themes[self.theme_index].name.as_str().to_string());
        self.prefs.save(&self.paths);
    }

    /// Flip the focused column of a device's row on the Settings page.
    pub(super) fn toggle_notify_pref(&mut self, device: &str) {
        let mut prefs = self.notify_prefs();
        if self.ui.settings_col == 0 {
            toggle(&mut prefs.quiet, device);
        } else {
            toggle(&mut prefs.finished, device);
        }
        self.set_notify_prefs(prefs);
    }
}
