use crate::detect::claude;
use crate::platform::SessionTarget;
use crate::tui::preview::PreviewSource;

use super::{App, Overlay};

impl App {
    pub(super) fn open_preview(&mut self) {
        if !self.bridge.is_active() {
            self.open_log_preview();
            return;
        }
        if self.prefs.preview_flicker_accepted {
            self.execute_preview_capture();
            return;
        }
        self.overlay = Overlay::ConfirmPreview;
    }

    pub(super) fn execute_preview_capture(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };

        let target = SessionTarget::from(session);
        let jsonl_path = session.jsonl_path.clone();

        let source = match self.bridge.capture_screen(&target) {
            Some(text) => PreviewSource::TerminalCapture(
                // Symmetric with the LogEntries path: every line of capture
                // crossing into the host terminal gets sanitised. Bridge
                // text is usually pre-rendered, but tmux/ghostty don't
                // guarantee the absence of every C0/C1 byte.
                text.lines()
                    .map(|l| crate::util::sanitize::strip_ansi(l).into_owned())
                    .collect(),
            ),
            None => PreviewSource::LogEntries(super::load_log_entries(&jsonl_path)),
        };
        self.overlay = Overlay::Preview {
            scroll: usize::MAX,
            source,
        };
    }

    pub(super) fn open_log_preview(&mut self) {
        let entries = match self.nth_filtered(self.selected) {
            Some(s) => super::load_log_entries(&s.jsonl_path),
            None => Vec::new(),
        };
        self.overlay = Overlay::Preview {
            scroll: usize::MAX,
            source: PreviewSource::LogEntries(entries),
        };
    }

    pub(super) fn start_kill(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        if session.state().is_terminal() {
            self.set_status_error("session is already gone or dormant");
            return;
        }
        let name = session.name().to_string();
        let id = session.id.clone();
        self.overlay = Overlay::ConfirmKill { name, id };
    }

    pub(super) fn execute_kill(&mut self, name: &str, id: &str) {
        // Single function handles resolve + re-validate + signal so
        // a recycled pid can't slip through between the lookup and
        // the kill.
        match claude::kill_by_session_id(id) {
            Ok(pid) => {
                self.set_status_success(format!("sent SIGTERM to '{name}' (pid {pid})"));
            }
            Err(e) => {
                self.set_status_error(format!("'{name}': {e}"));
            }
        }
    }

    pub(super) fn start_rename(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        self.overlay = Overlay::Rename(session.name().to_string());
    }

    pub(super) fn commit_rename(&mut self, new_name: String) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        let id = session.id.clone();
        if self.registry.name_taken(&new_name, id.as_str()) {
            self.set_status_error(format!("name '{}' is already taken", new_name));
            return;
        }
        if let Some(session) = self.registry.get_mut(id.as_str()) {
            session.rename_to(new_name);
        }
        // Name change affects both filter (matches_query) and Name-sort
        // ordering, so the FilteredView cache must rebuild.
        self.registry.bump_version();
        self.apply_filter();
    }
}
