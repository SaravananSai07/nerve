use crate::detect::claude;
use crate::platform::SessionTarget;
use crate::detect::claude::job_id;
use crate::state::session::SessionKind;
use crate::tui::status::StatusMessage;
use crate::tui::preview::PreviewSource;

use super::{App, Overlay};

impl App {
    pub(super) fn open_preview(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        // Only terminal sessions have a screen to capture. (`claude logs`
        // emits a raw redraw stream that doesn't replay into a readable
        // screen, so background jobs use the log view too.)
        if session.kind != SessionKind::Terminal || !self.bridge.is_active() {
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
        let source = match self.nth_filtered(self.selected) {
            // A parked background job often has no local transcript; its
            // state timeline is the closest thing to a log.
            Some(s) if s.kind == SessionKind::Background && s.jsonl_path.is_none() => {
                job_timeline_source(job_id(s.id.as_str()))
                    .unwrap_or(PreviewSource::LogEntries(Vec::new()))
            }
            Some(s) => PreviewSource::LogEntries(super::load_log_entries(&s.jsonl_path)),
            None => PreviewSource::LogEntries(Vec::new()),
        };
        self.overlay = Overlay::Preview {
            scroll: usize::MAX,
            source,
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
        if session.kind == SessionKind::Desktop {
            self.set_status_error("desktop session: close it in the Claude app");
            return;
        }
        let name = session.name().to_string();
        let id = session.id.clone();
        let kind = session.kind;
        self.overlay = Overlay::ConfirmKill { name, id, kind };
    }

    pub(super) fn execute_kill(&mut self, name: &str, id: &str, kind: SessionKind) {
        // Both paths shell out (`claude stop`, or a fresh `ps` to
        // re-validate the pid), so they run off the UI thread.
        let (name, id) = (name.to_string(), id.to_string());
        let outcome = move |r: Result<String, String>| match r {
            Ok(done) => StatusMessage::success(format!("'{name}': {done}")),
            Err(e) => StatusMessage::error(format!("'{name}': {e}")),
        };
        match kind {
            SessionKind::Background => self.run_action("stopping…".into(), move || {
                outcome(claude::stop_background_job(&id))
            }),
            SessionKind::Terminal => self.run_action("sending SIGTERM…".into(), move || {
                outcome(claude::kill_by_session_id(&id).map(|pid| format!("sent SIGTERM (pid {pid})")))
            }),
            // `start_kill` never opens the dialog for these.
            SessionKind::Desktop => {}
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

fn job_timeline_source(job: &str) -> Option<PreviewSource> {
    claude::job_timeline(job, 50)
        .map(|text| PreviewSource::JobTimeline(text.lines().map(str::to_string).collect()))
}
