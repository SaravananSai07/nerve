//! The TUI's side of sync: sessions on the other devices, and acting on
//! any session (answer, send, stop, notify, keep private) through the
//! agent. Before any device is connected, these fall back to what a
//! single device can do.

use std::collections::BTreeSet;

use super::App;
use crate::notify::{NotifyBook, NotifyPrefs};
use crate::sync::agent::{AgentHandle, FleetReq, FolderReq, PeerView, Request, SyncView};
use crate::sync::fleet::PrivateTarget;
use crate::sync::folders::{Others, When};
use crate::sync::protocol::{Answer, Approval, Asked, Body, Decision, FolderStatus, StartSpec};
use crate::tui::dialog::{parent_of, Connect, Dialog, FolderStep, Landing, ReviewStep, StartStep, WhoMode};
use crate::tui::model::{self, short_age, Ask, LocalFacts, Row, RowKey, OFFLINE_AFTER_SECS};
use crate::util::now_secs;
use crate::util::sanitize::clean;

const CONNECT_FIRST: &str = "Connect another device first: press c in Settings.";

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

    fn fleet_request(&mut self, r: FleetReq) -> bool {
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
        let devices = self
            .devices()
            .into_iter()
            .map(|(d, note)| (d, if note.is_empty() || note == "This device" { note } else { "Offline" }))
            .collect();
        self.ui.dialog =
            Some(Dialog::Start { step: StartStep::Device, devices, i: 0, dir: "~".into(), prompt: String::new() });
    }

    pub(super) fn start_session(&mut self, d: Dialog) {
        let Dialog::Start { devices, i, dir, prompt, .. } = d else { return };
        let (Some((device, _)), Some(h)) = (devices.into_iter().nth(i), &self.sync) else { return };
        let spec = StartSpec { session: crate::sync::protocol::new_session_id(), dir: dir.trim().to_string(), prompt };
        let id = h.request(Request::To { device: device.clone(), body: Body::Start(spec) });
        self.say(format!("Starting a session on {device}…"));
        self.ui.starting = Some((id, device));
    }

    /// Say how the last start ended, once its device answers.
    pub(super) fn follow_start(&mut self) {
        let Some((id, device)) = &self.ui.starting else { return };
        let Some((ok, msg)) = self.agent.log.iter().rev().find_map(|l| crate::sync::ipc::outcome_of(l, id)) else {
            return;
        };
        let text = if ok {
            format!("Started on {device}. It shows here once {device} reports it.")
        } else {
            format!("{device} didn't start it: {msg}")
        };
        self.ui.starting = None;
        self.say(text);
    }

    // ---- the Sync page ----

    /// This device's synced folders, as its agent last reported them.
    pub(super) fn folders(&self) -> &[FolderStatus] {
        self.agent.local.as_ref().map(|l| l.folders.as_slice()).unwrap_or_default()
    }

    /// `o`: what the devices sharing the folder may sync next.
    pub(super) fn start_rules(&mut self) {
        let Some(f) = self.sync_folder() else { return };
        let (name, parent) = (f.name.clone(), parent_of(&crate::paths::tilde(&f.path)));
        let i = f.mine.as_ref().map_or(0, |v| Others::ALL.iter().position(|o| *o == v.others).unwrap_or(0));
        self.ui.dialog = Some(Dialog::Rules { name, parent, i });
    }

    pub(super) fn set_rules(&mut self, name: String, i: usize) {
        let others = Others::ALL[i.min(Others::ALL.len() - 1)];
        if self.folder_request(FolderReq::Others { name: name.clone(), others }) {
            self.say(format!("{name}: {}.", others.label("its parent folder").to_lowercase()));
        }
    }

    /// `v`: the files a sync replaced in the folder.
    pub(super) fn start_versions(&mut self) {
        let Some(f) = self.sync_folder() else { return };
        let now = now_secs();
        let list = f
            .mine
            .iter()
            .flat_map(|v| v.versions.iter())
            .map(|(id, at, rel, why)| (id.clone(), format!("{:>4}  {rel}  {why}", short_age(now.saturating_sub(*at)))))
            .collect();
        self.ui.dialog = Some(Dialog::Versions { folder: f.name.clone(), list, i: 0 });
    }

    pub(super) fn restore(&mut self, id: String) {
        if self.fleet_request(FleetReq::Restore { id }) {
            self.say("Putting it back. It syncs to the other devices like any change.");
        }
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

    /// `f` on a device in Settings.
    pub(super) fn ask_forget(&mut self, device: String) {
        self.ui.confirm = Some(super::Confirm::Forget { device });
    }

    pub(super) fn forget(&mut self, device: String) {
        if self.fleet_request(FleetReq::Forget { device: device.clone() }) {
            self.say(format!("Forgot {device}. None of your devices trust it now."));
        }
    }

    /// `l` on a device in Settings: connect it to more of your devices.
    pub(super) fn start_link(&mut self, device: String) {
        let devices: Vec<String> = self.peer_names().into_iter().filter(|n| *n != device).collect();
        self.ui.dialog = Some(Dialog::Who {
            link: Some(device),
            set: devices.iter().cloned().collect(),
            devices,
            mode: WhoMode::AllLater,
            i: 0,
        });
    }

    pub(super) fn link(&mut self, device: String, with: Vec<String>) {
        let all = with.is_empty();
        if self.fleet_request(FleetReq::Link { device: device.clone(), with }) {
            self.say(if all {
                format!("Connecting {device} to all your devices, and ones you add later.")
            } else {
                format!("Connecting {device}.")
            });
        }
    }

    fn sync_folder(&self) -> Option<&FolderStatus> {
        let f = self.folders();
        f.get(self.ui.sync_cursor.min(f.len().saturating_sub(1)))
    }

    pub(super) fn sync_move(&mut self, by: isize) {
        let n = self.folders().len();
        self.ui.sync_cursor = (self.ui.sync_cursor.min(n.saturating_sub(1)) as isize + by)
            .clamp(0, n.saturating_sub(1) as isize) as usize;
    }

    fn folder_request(&mut self, r: FolderReq) -> bool {
        match &self.sync {
            Some(h) if self.agent.running => {
                h.request(Request::Folder(r));
                true
            }
            _ => {
                self.say(CONNECT_FIRST);
                false
            }
        }
    }

    pub(super) fn sync_now(&mut self) {
        let Some(name) = self.sync_folder().map(|f| f.name.clone()) else { return };
        if self.folder_request(FolderReq::Now { name: name.clone() }) {
            self.say(format!("Syncing {name} now."));
        }
    }

    pub(super) fn ask_stop_sync(&mut self) {
        let Some(f) = self.sync_folder() else { return };
        let (name, path) = (f.name.clone(), crate::paths::tilde(&f.path));
        self.ui.confirm = Some(super::Confirm::StopSync { name, path });
    }

    pub(super) fn stop_sync(&mut self, name: String, path: String) {
        if self.folder_request(FolderReq::Remove { name }) {
            self.say(format!("{path} stays here and stops syncing. Your other devices keep their copies."));
        }
    }

    /// Step one of "Sync a folder": the folders sessions here work in,
    /// not private and not synced yet.
    pub(super) fn start_add_folder(&mut self) {
        if self.sync.is_none() || !self.agent.running {
            return self.say(CONNECT_FIRST);
        }
        if self.agent.peers.is_empty() {
            return self.say("No other devices yet: press c in Settings to connect one.");
        }
        let mut cands: Vec<(std::path::PathBuf, String, String)> = Vec::new();
        for r in self.rows.iter().filter(|r| r.is_local() && !r.private && !r.synced) {
            let Some(cwd) = self.registry.get(&r.key.id).map(|s| s.cwd.clone()) else { continue };
            if !cands.iter().any(|c| c.0 == cwd) {
                cands.push((cwd.clone(), crate::paths::tilde(&cwd.to_string_lossy()), r.project.clone()));
            }
        }
        let devices = self
            .devices()
            .into_iter()
            .map(|(d, note)| {
                (d, if note.is_empty() || note == "This device" { note } else { "Offline, catches up when back" })
            })
            .collect();
        let set = BTreeSet::from([self.me.clone()]);
        self.ui.dialog =
            Some(Dialog::AddFolder { step: FolderStep::Folder, cands, pick: 0, devices, set, when: 0, i: 0 });
    }

    pub(super) fn add_folder(&mut self, d: Dialog) {
        let Dialog::AddFolder { cands, pick, set, when, i, .. } = d else { return };
        let Some((path, shown, _)) = cands.into_iter().nth(pick) else { return };
        let with: Vec<String> = set.into_iter().filter(|d| *d != self.me).collect();
        let (when, others) = (When::ALL[when.min(When::ALL.len() - 1)], Others::ALL[i.min(Others::ALL.len() - 1)]);
        // Started here by a person: the other devices need not ask anyone.
        if self.folder_request(FolderReq::Add { path, with: with.clone(), when, others, yes: true }) {
            self.say(format!("Syncing {shown} with {}.", crate::util::text::list_join(&with)));
            self.ui.page = crate::tui::Page::Sync;
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
