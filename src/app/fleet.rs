//! The TUI's side of sync: sessions on the other devices, and acting on
//! any session (answer, send, stop, notify, keep private) through the
//! agent. Before any device is connected, these fall back to what a
//! single device can do.

use std::collections::BTreeSet;

use super::App;
use crate::notify::{NotifyBook, NotifyPrefs};
use crate::sync::agent::{AgentHandle, FleetReq, PeerView, Request, SyncView};
use crate::sync::fleet::PrivateTarget;
use crate::sync::folders::Others;
use crate::sync::handoff::PROMPT_MAX;
use crate::sync::protocol::{Answer, Approval, Asked, Body, Decision, StartSpec};
use crate::tui::dialog::{Connect, Dialog, Landing, ReviewStep, StartStep, WhoMode};
use crate::tui::editor::Editor;
use crate::tui::model::{self, Ask, LocalFacts, Row, RowKey, Status, OFFLINE_AFTER_SECS};
use crate::util::now_secs;
use crate::util::sanitize::clean;

pub(super) const CONNECT_FIRST: &str = "Connect another device first: press c in Settings.";

/// Is the peer reporting? A device that went quiet counts as offline.
pub(super) fn online(p: &PeerView) -> bool {
    age(p.state_generated_at).is_some_and(|a| a < OFFLINE_AFTER_SECS)
}

/// Local sessions as the agent sees them: what their mods ask, whether a
/// prompt can reach them, and what is private.
struct SyncFacts<'a>(&'a SyncView);

impl LocalFacts for SyncFacts<'_> {
    fn ask(&self, id: &str) -> Option<Ask> {
        self.0.local.as_ref()?.requests.iter().find(|r| r.session == id).map(model::ask_of)
    }
    fn private(&self, id: &str, cwd: &std::path::Path) -> bool {
        self.0.private_sessions.iter().any(|s| s == id) || crate::sync::fleet::inside(&self.0.private, cwd)
    }
    fn notify(&self, id: &str) -> bool {
        !self.0.notify.finished_to(id).is_empty()
    }
    /// Until the agent has written this device's state (a viewing TUI
    /// started before it, or an agent from before that file), a session
    /// counts as reachable: the agent refuses a prompt it can't deliver.
    fn can_send(&self, id: &str) -> bool {
        self.0.local.as_ref().is_none_or(|l| l.sessions.iter().any(|s| s.id == id && s.injectable))
    }
    /// In a folder some other device has said yes to.
    fn synced(&self, cwd: &std::path::Path) -> bool {
        self.0.local.as_ref().is_some_and(|l| {
            l.folders
                .iter()
                .any(|f| cwd.starts_with(&f.path) && f.mine.as_ref().is_none_or(|v| v.held.len() < v.with.len()))
        })
    }
}

/// Before sync is set up: nothing to answer or type through, nothing
/// private, and finish notifications kept by the TUI itself.
struct BookFacts<'a>(&'a NotifyBook);

impl LocalFacts for BookFacts<'_> {
    fn ask(&self, _: &str) -> Option<Ask> {
        None
    }
    fn private(&self, _: &str, _: &std::path::Path) -> bool {
        false
    }
    fn notify(&self, id: &str) -> bool {
        !self.0.finished_to(id).is_empty()
    }
    fn can_send(&self, _: &str) -> bool {
        false
    }
    fn synced(&self, _: &std::path::Path) -> bool {
        false
    }
}

/// Seconds since a peer's state was made, by its clock.
pub(super) fn age(generated_at: Option<u64>) -> Option<u64> {
    generated_at.map(|g| now_secs().saturating_sub(g))
}

impl App {
    pub(super) fn local_facts(&self) -> Box<dyn LocalFacts + '_> {
        match &self.sync {
            Some(_) => Box::new(SyncFacts(&self.agent)),
            None => Box::new(BookFacts(&self.book)),
        }
    }

    pub(super) fn peer_rows(&self) -> Vec<Row> {
        self.agent
            .peers
            .iter()
            .filter_map(|p| Some(model::remote_rows(&p.name, age(p.state_generated_at)?, p.state.as_ref()?)))
            .flatten()
            .collect()
    }

    fn name_of(&self, hex: &str) -> String {
        self.agent.names.iter().find(|(h, _)| h == hex).map_or_else(|| "A device".to_string(), |(_, n)| n.clone())
    }

    /// Every approval, here (holder None) and on the other devices.
    fn approvals(&self) -> impl Iterator<Item = (Option<&str>, &Approval)> {
        let here = self.agent.local.iter().flat_map(|l| l.approvals.iter().map(|a| (None, a)));
        here.chain(self.agent.peers.iter().flat_map(|p| {
            p.state.iter().flat_map(move |s| s.approvals.iter().map(move |a| (Some(p.name.as_str()), a)))
        }))
    }

    /// What waits for a yes, as rows of Needs you.
    pub(super) fn approval_rows(&self) -> Vec<Row> {
        let now = now_secs();
        self.approvals()
            .map(|(holder, a)| model::approval_row(a, &self.name_of(&a.from), holder, &self.me, now))
            .collect()
    }

    /// Folders this device shares or is offered that wait for a yes:
    /// (folder, "mini → laptop").
    pub(super) fn folder_waits(&self) -> Vec<(String, String)> {
        let me = self.agent.names.iter().find(|(_, n)| *n == self.me).map(|(h, _)| h.clone()).unwrap_or_default();
        self.approvals()
            .filter_map(|(holder, a)| {
                let Asked::Folder { set, .. } = &a.what else { return None };
                match holder {
                    None => Some((set.name.clone(), format!("{} → {}", self.name_of(&a.from), self.me))),
                    Some(h) if a.from == me => Some((set.name.clone(), format!("{} → {h}", self.me))),
                    Some(_) => None,
                }
            })
            .map(|(n, r)| (clean(n), r))
            .collect()
    }

    fn approval(&self, key: &RowKey) -> Option<&Approval> {
        let held = match &key.device {
            None => self.agent.local.as_ref(),
            Some(d) => self.agent.peers.iter().find(|p| p.name == *d).and_then(|p| p.state.as_ref()),
        };
        held?.approvals.iter().find(|a| a.id == key.id)
    }

    pub(super) fn fleet_request(&mut self, r: FleetReq) -> bool {
        // Not `running` yet is fine: the agent takes it once it connects.
        match &self.sync {
            Some(h) => {
                h.request(Request::Fleet(r));
                true
            }
            None => {
                self.say("Sync isn't running. Press c in Settings to connect a device.");
                false
            }
        }
    }

    /// `y`, `n` or the review's answer to the request at `key`.
    pub(super) fn decide(&mut self, key: &RowKey, yes: bool, at: Option<String>, others: Option<Others>) {
        let what = self.row(key).map(|r| r.project.clone()).unwrap_or_default();
        if self.fleet_request(FleetReq::Decide(Decision { id: key.id.clone(), yes, at, others })) {
            self.say(if yes { format!("Allowed {what}.") } else { format!("Refused {what}.") });
        }
    }

    /// `↵` on a request: check a joining device's words, or choose where
    /// a folder lands and what may follow it.
    pub(super) fn review(&mut self, key: RowKey) {
        let Some(a) = self.approval(&key) else { return };
        let row = model::approval_row(a, &self.name_of(&a.from), key.device.as_deref(), &self.me, now_secs());
        let (detail, words, folder) = match &a.what {
            Asked::Folder { .. } => {
                (row.why.unwrap_or_default(), None, Some(Landing { lands: row.path.clone(), other: String::new() }))
            }
            Asked::Join { words, join, .. } => {
                let with: Vec<String> = std::iter::once(key.device.clone().unwrap_or_else(|| self.me.clone()))
                    .chain(join.iter().map(clean))
                    .collect();
                (format!("Joins {}.", crate::util::text::list_join(&with)), Some(clean(words)), None)
            }
        };
        self.ui.dialog =
            Some(Dialog::Review { key, title: row.title, detail, words, folder, step: ReviewStep::Where, i: 0 });
    }

    pub(super) fn peer_names(&self) -> Vec<String> {
        self.agent.peers.iter().map(|p| p.name.clone()).collect()
    }

    /// The other devices with the seconds since each last reported.
    pub(super) fn peer_ages(&self) -> Vec<(String, Option<u64>)> {
        self.agent.peers.iter().map(|p| (p.name.clone(), age(p.state_generated_at))).collect()
    }

    pub(super) fn others_online(&self) -> Option<usize> {
        self.sync.as_ref()?;
        Some(self.agent.peers.iter().filter(|p| online(p)).count())
    }

    pub(super) fn can_compose(&self) -> bool {
        self.sync.is_some() && self.agent.running
    }

    pub(super) fn can_private(&self) -> bool {
        self.sync.is_some()
    }

    /// Hand `body` to the session's own device: this one's agent for a
    /// local session (private ones included), a peer's for the rest.
    fn send(&mut self, key: &RowKey, body: Body) -> bool {
        let Some(h) = &self.sync else {
            self.say(CONNECT_FIRST);
            return false;
        };
        h.request(match &key.device {
            None => Request::Local(body),
            Some(device) => Request::To { device: device.clone(), body },
        });
        true
    }

    pub(super) fn send_answer(&mut self, key: &RowKey, request: String, permission: bool, choice: &str) -> bool {
        let answer = match (permission, choice) {
            (true, "Allow") => Answer::Allow,
            (true, _) => Answer::Deny { reason: format!("Denied from {}", self.me) },
            (false, _) => Answer::Choose { labels: vec![choice.to_string()] },
        };
        self.send(key, Body::Answer { request, answer })
    }

    pub(super) fn send_text(&mut self, key: &RowKey, text: String) -> bool {
        self.send(key, Body::Send { session: key.id.clone(), text })
    }

    pub(super) fn stop_remote(&mut self, key: &RowKey) {
        self.send(key, Body::Stop { session: key.id.clone() });
    }

    pub(super) fn notify_book(&self) -> &NotifyBook {
        if self.sync.is_some() {
            &self.agent.notify
        } else {
            &self.book
        }
    }

    pub(super) fn notify_prefs(&self) -> NotifyPrefs {
        self.notify_book().prefs.clone()
    }

    pub(super) fn set_notify_prefs(&mut self, prefs: NotifyPrefs) {
        match &self.sync {
            Some(h) => {
                h.request(Request::SetNotifyPrefs(prefs.clone()));
                // Shown now; the agent's copy follows within a tick.
                self.agent.notify.prefs = prefs;
            }
            None => {
                self.book.prefs = prefs;
                self.save_book();
            }
        }
    }

    pub(super) fn devices(&self) -> Vec<(String, &'static str)> {
        let mut v = vec![(self.me.clone(), "This device")];
        for p in &self.agent.peers {
            v.push((p.name.clone(), if online(p) { "" } else { "Offline, gets it when back" }));
        }
        v
    }

    pub(super) fn notify_set(&self, key: &RowKey) -> BTreeSet<String> {
        match &key.device {
            None => self.notify_book().finished_to(&key.id).clone(),
            Some(dev) => self
                .agent
                .peers
                .iter()
                .find(|p| p.name == *dev)
                .and_then(|p| p.state.as_ref()?.sessions.iter().find(|s| s.id == key.id))
                .map(|s| s.notify.iter().cloned().collect())
                .unwrap_or_default(),
        }
    }

    pub(super) fn set_notify(&mut self, key: &RowKey, set: BTreeSet<String>) -> bool {
        if self.sync.is_none() {
            self.book.set_session(&key.id, set);
            return self.save_book();
        }
        if key.device.is_none() {
            self.agent.notify.set_session(&key.id, set.clone());
        }
        self.send(key, Body::SetNotify { session: key.id.clone(), devices: set.into_iter().collect() })
    }

    pub(super) fn save_book(&mut self) -> bool {
        match self.book.save(self.paths.config_dir()) {
            Ok(()) => true,
            Err(e) => {
                self.say(format!("Couldn't save notify.toml: {e}"));
                false
            }
        }
    }

    pub(super) fn private_key(&mut self) {
        let Some(row) = self.cur_row() else { return };
        if let Some(dev) = &row.key.device {
            return self.say(format!("Privacy for this session is set on {dev}."));
        }
        if self.sync.is_none() {
            return self.say("Nothing leaves this device until you connect another: press c in Settings.");
        }
        let (key, project, private) = (row.key.clone(), row.project.clone(), row.private);
        let Some(cwd) = self.registry.get(&key.id).map(|s| s.cwd.clone()) else { return };
        if private {
            let target = if self.agent.private_sessions.contains(&key.id) {
                PrivateTarget::Session(key.id.clone())
            } else {
                let rule =
                    self.agent.private.iter().find(|p| crate::sync::fleet::inside(std::slice::from_ref(p), &cwd));
                let Some(rule) = rule.cloned() else { return };
                PrivateTarget::Folder(crate::paths::untilde(&rule))
            };
            self.set_private(target, false);
            return self.say(format!("Your other devices can see {project} again."));
        }
        let path = crate::paths::tilde(&cwd.to_string_lossy());
        let synced = self
            .folders()
            .iter()
            .find(|f| cwd.starts_with(&f.path) || std::path::Path::new(&f.path).starts_with(&cwd))
            .map(|f| crate::paths::tilde(&f.path));
        self.ui.dialog = Some(Dialog::Keep { key, project, path, folder: false, synced });
    }

    pub(super) fn keep_private(&mut self, d: Dialog) {
        let Dialog::Keep { key, project, path, folder, .. } = d else { return };
        let target = if folder {
            let Some(cwd) = self.registry.get(&key.id).map(|s| s.cwd.clone()) else { return };
            PrivateTarget::Folder(cwd)
        } else {
            PrivateTarget::Session(key.id.clone())
        };
        self.set_private(target, true);
        self.say(format!("Only this device can see {} now.", if folder { path } else { project }));
    }

    pub(super) fn share_again(&mut self, i: usize) {
        let folders = self.agent.private.len();
        let (target, what) = match self.agent.private.get(i) {
            Some(f) => (PrivateTarget::Folder(crate::paths::untilde(f)), crate::paths::tilde(f)),
            None => match self.agent.private_sessions.get(i - folders) {
                Some(id) => (PrivateTarget::Session(id.clone()), self.session_label(id)),
                None => return,
            },
        };
        self.set_private(target, false);
        self.say(format!("Your other devices can see {what} again."));
    }

    fn set_private(&mut self, target: PrivateTarget, on: bool) {
        if let Some(h) = &self.sync {
            h.request(Request::SetPrivate { target, on });
        }
    }

    pub(super) fn session_label(&self, id: &str) -> String {
        self.registry.get(id).map(|s| s.name().as_str().to_string()).unwrap_or_else(|| format!("session {id:.8}"))
    }

    pub(super) fn start_handoff(&mut self) {
        let Some(row) = self.cur_row() else { return };
        if row.key.device.is_some() {
            return self.say("Hand it off from the device it runs on.");
        }
        if self.sync.is_none() {
            return self.say(CONNECT_FIRST);
        }
        let peers: Vec<String> = self.peer_names();
        if peers.is_empty() {
            return self.say("No other devices yet: press c in Settings to connect one.");
        }
        let (key, project) = (row.key.clone(), row.project.clone());
        self.ui.dialog = Some(Dialog::Handoff { key, project, peers });
    }

    pub(super) fn handoff(&mut self, d: Dialog, peer: String) {
        let Dialog::Handoff { key, project, .. } = d else { return };
        let (Some(h), Some(cwd)) = (&self.sync, self.registry.get(&key.id).map(|s| s.cwd.clone())) else { return };
        h.request(Request::Handoff {
            session_id: key.id.clone(),
            cwd,
            device: peer.clone(),
            continue_with: None,
            place: None,
            force: false,
        });
        self.say(format!("Handing {project} to {peer}…"));
    }

    /// `s`: start a session on a device, in a folder there, with a prompt.
    pub(super) fn start_session_dialog(&mut self) {
        if self.sync.is_none() || !self.agent.running {
            return self.say(CONNECT_FIRST);
        }
        let devices = self.start_devices();
        self.ui.dialog = Some(Dialog::Start {
            step: StartStep::Device,
            devices,
            i: 0,
            dir: "~".into(),
            prompt: Editor::new(PROMPT_MAX),
            replaces: None,
        });
    }

    fn start_devices(&self) -> Vec<(String, &'static str)> {
        self.devices()
            .into_iter()
            .map(|(d, note)| (d, if note.is_empty() || note == "This device" { note } else { "Offline" }))
            .collect()
    }

    pub(super) fn start_session(&mut self, d: Dialog) {
        let Dialog::Start { devices, i, dir, mut prompt, replaces, .. } = d else { return };
        let Some((device, _)) = devices.into_iter().nth(i) else { return };
        if let Some(old) = replaces {
            self.ui.starting.retain(|s| s.spec.session != old);
        }
        let spec = StartSpec { session: String::new(), dir: dir.trim().to_string(), prompt: prompt.take() };
        self.send_start(device, spec);
    }

    /// Ask `device` to start `spec` under a fresh session id, and show it
    /// in the list at once, under the cursor, until the device reports it.
    fn send_start(&mut self, device: String, mut spec: StartSpec) {
        let Some(h) = &self.sync else { return };
        spec.session = crate::sync::protocol::new_session_id();
        let request = h.request(Request::To { device: device.clone(), body: Body::Start(spec.clone()) });
        let key = RowKey { device: (device != self.me).then(|| device.clone()), id: spec.session.clone() };
        self.ui.starting.push(Starting { request, device, spec, at: now_secs(), phase: Phase::Sent });
        self.ui.cur = Some(key);
    }

    /// Follow each start: what its device answered, whether it reports
    /// the session yet (then the real row takes over), and how long it
    /// has gone unanswered.
    pub(super) fn follow_start(&mut self) {
        let now = now_secs();
        let mut starting = std::mem::take(&mut self.ui.starting);
        starting.retain(|s| !self.reported(&s.device, &s.spec.session));
        for s in &mut starting {
            let outcome = || self.agent.log.iter().rev().find_map(|l| crate::sync::ipc::outcome_of(l, &s.request));
            s.phase = match &s.phase {
                Phase::Sent => match outcome() {
                    Some((true, _)) => Phase::Accepted(now),
                    Some((false, msg)) => Phase::Failed(format!("{} didn't start it: {}", s.device, clean(msg))),
                    // A device refuses a start older than this.
                    None if now.saturating_sub(s.at) > START_ANSWER_SECS => {
                        Phase::Failed(format!("{} didn't answer within 5 minutes, so it won't start it now.", s.device))
                    }
                    None => Phase::Sent,
                },
                Phase::Accepted(at) if now.saturating_sub(*at) > START_REPORT_SECS => Phase::Failed(format!(
                    "{} started it, but hasn't shown it since. Claude may have stopped right away; \
                     look in tmux on {} before you retry.",
                    s.device, s.device
                )),
                p => p.clone(),
            };
        }
        self.ui.starting = starting;
    }

    /// Does `device` (this one included) show session `id` yet?
    fn reported(&self, device: &str, id: &str) -> bool {
        if device == self.me {
            return self.registry.get(id).is_some();
        }
        self.agent
            .peers
            .iter()
            .find(|p| p.name == device)
            .and_then(|p| p.state.as_ref())
            .is_some_and(|st| st.sessions.iter().any(|s| s.id == id))
    }

    /// Starts not reported yet, as rows.
    pub(super) fn start_rows(&self) -> Vec<Row> {
        let now = now_secs();
        self.ui
            .starting
            .iter()
            .map(|s| {
                let (status, why, since) = match &s.phase {
                    Phase::Sent if !self.agent.peers.iter().any(|p| p.name == s.device && online(p)) => (
                        Status::Starting,
                        format!("Sent. {} is offline; it starts once {} is back.", s.device, s.device),
                        s.at,
                    ),
                    Phase::Sent => (Status::Starting, format!("Sent to {}, waiting for it to answer.", s.device), s.at),
                    Phase::Accepted(_) => {
                        (Status::Starting, format!("{} started it. It shows here in a moment.", s.device), s.at)
                    }
                    Phase::Failed(e) => (Status::NotStarted, e.clone(), s.at),
                };
                model::start_row(
                    RowKey { device: (s.device != self.me).then(|| s.device.clone()), id: s.spec.session.clone() },
                    &s.spec,
                    status,
                    why,
                    now.saturating_sub(since),
                )
            })
            .collect()
    }

    fn start_of(&self, key: &RowKey) -> Option<usize> {
        self.ui.starting.iter().position(|s| s.spec.session == key.id)
    }

    /// `r` on a start that failed: send it again.
    pub(super) fn retry_start(&mut self, key: &RowKey) {
        let Some(i) = self.start_of(key) else { return };
        let s = self.ui.starting.remove(i);
        self.send_start(s.device, s.spec);
    }

    /// `e` on a start that failed: back to its prompt, to change it first.
    pub(super) fn edit_start(&mut self, key: &RowKey) {
        let Some(s) = self.start_of(key).map(|i| &self.ui.starting[i]) else { return };
        let devices = self.start_devices();
        let i = devices.iter().position(|(d, _)| *d == s.device).unwrap_or(0);
        self.ui.dialog = Some(Dialog::Start {
            step: StartStep::Prompt,
            devices,
            i,
            dir: s.spec.dir.clone(),
            prompt: Editor::with_text(&s.spec.prompt, PROMPT_MAX),
            replaces: Some(s.spec.session.clone()),
        });
    }

    /// `x` on a start that failed: drop it from the list.
    pub(super) fn dismiss_start(&mut self, key: &RowKey) {
        if let Some(i) = self.start_of(key) {
            self.ui.starting.remove(i);
        }
    }

    /// Keep the open session's whole transcript when it runs elsewhere:
    /// its device's state carries only a short tail, so ask for the rest
    /// on opening it and whenever that tail changes, one fetch at a time.
    pub(super) fn follow_transcript(&mut self) {
        let Some(h) = &self.sync else { return };
        if let Some((key, id, at)) = &self.remote.asked {
            if let Some(text) = h.blob(id) {
                let lines: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
                self.remote.said.insert(key.clone(), model::conversation_of_lines(&lines));
                self.remote.asked = None;
            } else if at.elapsed() > FETCH_WAIT || self.agent.log.iter().any(|l| crate::sync::ipc::failed(l, id)) {
                self.remote.asked = None;
            }
        }
        self.remote.said.retain(|k, _| self.rows.iter().any(|r| r.key == *k));
        let open = self.ui.open && self.ui.page == crate::tui::Page::Sessions;
        let Some(row) = self.cur_row().filter(|r| open && !r.is_local()) else { return };
        if matches!(row.status, Status::Offline | Status::Starting | Status::NotStarted) {
            return;
        }
        if self.remote.asked.as_ref().is_some_and(|(k, ..)| *k == row.key)
            || self.remote.last.is_some_and(|t| t.elapsed() < FETCH_GAP)
        {
            return;
        }
        let seen = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (&row.tail, row.status.word()).hash(&mut hasher);
            (row.key.clone(), hasher.finish())
        };
        if self.remote.seen.as_ref() == Some(&seen) {
            return;
        }
        let (Some(device), Some(h)) = (row.key.device.clone(), &self.sync) else { return };
        let what = crate::sync::protocol::FetchWhat::Transcript;
        let id = h.request(Request::To { device, body: Body::Fetch { session: row.key.id.clone(), what } });
        self.remote.asked = Some((row.key.clone(), id, std::time::Instant::now()));
        self.remote.last = Some(std::time::Instant::now());
        self.remote.seen = Some(seen);
    }

    /// The open session's transcript from its device, once it came.
    pub(super) fn remote_transcript(&self, key: &RowKey) -> Option<&[crate::tui::model::Said]> {
        self.remote.said.get(key).map(Vec::as_slice)
    }

    // ---- devices ----

    /// `c` in Settings.
    pub(super) fn start_connect(&mut self) {
        self.ui.dialog = Some(Dialog::Connect(Connect::Choose(0)));
    }

    /// After "Show a code": who it joins, when there is anyone to join.
    pub(super) fn connect_who(&mut self) {
        let devices: Vec<String> = self.peer_names();
        if devices.is_empty() {
            return self.invite("all".into());
        }
        let set = devices.iter().cloned().collect();
        self.ui.dialog = Some(Dialog::Who { link: None, devices, set, mode: WhoMode::AllLater, i: 0 });
    }

    /// The code the agent shows, through this device's state.
    fn shown_code(&self) -> Option<String> {
        self.agent.local.as_ref()?.invite.clone()
    }

    pub(super) fn invite(&mut self, with: String) {
        let old = self.shown_code();
        if self.fleet_request(FleetReq::Invite { with }) {
            self.ui.dialog = Some(Dialog::Connect(Connect::Code { code: None, since: now_secs(), old }));
        }
    }

    pub(super) fn uninvite(&mut self) {
        if let Some(Ok(inv)) = self.shown_code().map(|c| crate::sync::connect::Invite::decode(&c)) {
            self.fleet_request(FleetReq::Uninvite { secret: inv.s });
        }
    }

    /// The code dialog follows the agent: the code once made, then the
    /// join it brought, to allow.
    pub(super) fn follow_connect(&mut self) {
        while let Some(ev) = self.conn.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match ev {
                ConnEvent::Ready(res) => {
                    let preparing = matches!(self.ui.dialog, Some(Dialog::Connect(Connect::Preparing)));
                    match res {
                        Ok(h) => {
                            self.attach(Some(h));
                            if preparing {
                                self.connect_who();
                            }
                        }
                        Err(e) => {
                            if preparing {
                                self.ui.dialog = None;
                            }
                            self.say(format!("Couldn't set this device up: {e}"));
                        }
                    }
                }
                ConnEvent::Words(w) => {
                    if let Some(Dialog::Connect(Connect::Joining { words, .. })) = &mut self.ui.dialog {
                        *words = Some(w);
                    }
                }
                ConnEvent::Joined(res, h) => {
                    self.attach(h);
                    match res {
                        Ok(names) => {
                            if matches!(self.ui.dialog, Some(Dialog::Connect(_))) {
                                self.ui.dialog = None;
                            }
                            self.say(format!(
                                "Connected to {}. Their sessions show here as they report.",
                                crate::util::text::list_join(&names)
                            ));
                        }
                        Err(e) => match &mut self.ui.dialog {
                            Some(Dialog::Connect(Connect::Joining { err, .. })) => *err = Some(e),
                            _ => self.say(e),
                        },
                    }
                }
            }
        }
        let shown = self.shown_code();
        let Some(Dialog::Connect(Connect::Code { code, since, old })) = &mut self.ui.dialog else { return };
        if code.is_none() && shown != *old {
            *code = shown;
        }
        let since = *since;
        let join = self
            .agent
            .local
            .iter()
            .flat_map(|l| &l.approvals)
            .find(|a| a.at >= since && matches!(a.what, Asked::Join { .. }))
            .map(|a| a.id.clone());
        if let Some(id) = join {
            self.review(RowKey { device: None, id });
        }
    }

    fn attach(&mut self, h: Option<AgentHandle>) {
        if let Some(h) = h {
            let view = h.snapshot();
            self.me = view.me.clone();
            self.agent = view;
            self.sync = Some(h);
        }
    }

    /// "Show a code" on a device not set up yet: set it up first, off the
    /// UI thread. (Entering a code sets it up as part of joining.)
    pub(super) fn prepare_first(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.conn = Some(rx);
        let (paths, sound) = (self.paths.clone(), self.config.notifications.sound);
        self.ui.dialog = Some(Dialog::Connect(Connect::Preparing));
        let spawned = std::thread::Builder::new().name("nerve-prepare".into()).spawn(move || {
            let res = crate::sync::connect::prepare(&paths, &Default::default(), None, &|_, _| {}).and_then(|_| {
                crate::sync::start_after_setup(&paths, sound)
                    .ok_or_else(|| "its background agent didn't start".to_string())
            });
            let _ = tx.send(ConnEvent::Ready(res));
        });
        if spawned.is_err() {
            self.ui.dialog = None;
            self.conn = None;
            self.say("Couldn't start setting this device up.");
        }
    }

    /// Join the devices `code` offers: `want` (None: all of them).
    pub(super) fn join(&mut self, code: String, want: Option<Vec<String>>) {
        let Ok(inv) = crate::sync::connect::Invite::decode(&code) else { return };
        let (tx, rx) = std::sync::mpsc::channel();
        self.conn = Some(rx);
        let (paths, sound, fresh) = (self.paths.clone(), self.config.notifications.sound, self.sync.is_none());
        self.ui.dialog = Some(Dialog::Connect(Connect::Joining { to: clean(&inv.n), words: None, err: None }));
        let spawned = std::thread::Builder::new().name("nerve-join".into()).spawn(move || {
            let dir = crate::sync::fleet::sync_dir(&paths);
            let keys = crate::sync::connect::prepare(
                &paths,
                &crate::sync::connect::Opts { service: fresh, claude: fresh, ..Default::default() },
                Some(&inv),
                &|_, _| {},
            )
            .map(|(_, k)| k);
            let res = keys.and_then(|keys| {
                let _ =
                    tx.send(ConnEvent::Words(crate::sync::connect::words(&inv.k, &keys.public_key().to_hex(), &inv.s)));
                crate::sync::connect::join(&dir, &keys, &inv, want.as_deref())
            });
            let h = (fresh && res.is_ok()).then(|| crate::sync::start_after_setup(&paths, sound)).flatten();
            let _ = tx.send(ConnEvent::Joined(res, h));
        });
        if spawned.is_err() {
            self.ui.dialog = None;
            self.conn = None;
            self.say("Couldn't start joining.");
        }
    }
}

/// What a worker connecting this device reports back.
pub(super) enum ConnEvent {
    /// Set up, with the agent to talk to, or why not.
    Ready(Result<AgentHandle, String>),
    /// The words to check on the device that showed the code.
    Words(String),
    /// The devices joined, or why not; the agent, on a device set up just now.
    Joined(Result<Vec<String>, String>, Option<AgentHandle>),
}

/// Seconds a device has to answer a start: it refuses older ones.
const START_ANSWER_SECS: u64 = 5 * 60;
/// Seconds after a device started a session for it to show up.
const START_REPORT_SECS: u64 = 2 * 60;

/// A session asked of a device, shown until the device reports it.
pub(super) struct Starting {
    /// The request's id: its outcome line is the device's answer.
    request: String,
    device: String,
    spec: StartSpec,
    /// When it was sent (Unix seconds).
    at: u64,
    phase: Phase,
}

#[derive(Clone)]
enum Phase {
    Sent,
    /// The device started it, at this time; it hasn't reported it yet.
    Accepted(u64),
    Failed(String),
}

/// How long a transcript fetch may take before another may go, and the
/// least time between two.
const FETCH_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
const FETCH_GAP: std::time::Duration = std::time::Duration::from_secs(2);

/// Transcripts of sessions on other devices, fetched while open.
#[derive(Default)]
pub(super) struct Remote {
    said: std::collections::HashMap<RowKey, Vec<crate::tui::model::Said>>,
    /// The fetch on its way: (session, request id, when).
    asked: Option<(RowKey, String, std::time::Instant)>,
    last: Option<std::time::Instant>,
    /// The session and its tail's hash at the last fetch: the next waits
    /// for a change.
    seen: Option<(RowKey, u64)>,
}
