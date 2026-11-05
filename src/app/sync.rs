//! The Sync page, end to end: its keys, view, footer hints, and the
//! folder actions (add, rules, versions, sync now, stop) through the agent.

use std::collections::BTreeSet;

use crossterm::event::KeyCode;

use super::fleet::CONNECT_FIRST;
use super::App;
use crate::sync::agent::{FleetReq, FolderReq, Request};
use crate::sync::folders::{Others, When};
use crate::sync::protocol::FolderStatus;
use crate::tui::dialog::{parent_of, Dialog, FolderStep};
use crate::tui::model::short_age;
use crate::tui::sync::SyncPage;
use crate::tui::Page;
use crate::util::now_secs;

impl App {
    pub(super) fn sync_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.sync_move(1),
            KeyCode::Char('k') | KeyCode::Up => self.sync_move(-1),
            KeyCode::Char('s') => self.sync_now(),
            KeyCode::Char('a') => self.start_add_folder(),
            KeyCode::Char('c') => self.start_connect(),
            KeyCode::Char('x') => self.ask_stop_sync(),
            KeyCode::Char('o') => self.start_rules(),
            KeyCode::Char('v') => self.start_versions(),
            KeyCode::Esc => self.ui.page = Page::Sessions,
            _ => {}
        }
    }

    pub(super) fn sync_page(&self) -> SyncPage<'_> {
        let folders = self.folders();
        SyncPage {
            me: &self.me,
            running: self.sync.is_some() && self.agent.running,
            folders,
            cursor: self.ui.sync_cursor.min(folders.len().saturating_sub(1)),
            bulk_over: self.agent.bulk_over,
            peers: self.peer_ages(),
            now: now_secs(),
            waiting: self.folder_waits(),
        }
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

    /// The footer's hints on the Sync page.
    pub(super) fn sync_hints(page: &SyncPage) -> Vec<(&'static str, String)> {
        let h = |k: &'static str, v: &str| (k, v.to_string());
        let mut v = vec![h("j k", "Move"), h("s", "Sync now"), h("a", "Sync a folder")];
        if !page.running {
            v = vec![h("c", "Connect a device")];
        } else if page.folders.is_empty() {
            v.drain(..2);
        } else {
            v.extend([h("o", "Other folders"), h("v", "Versions"), h("x", "Stop syncing")]);
        }
        v.extend([h("Tab", "Next page"), h("?", "Keys")]);
        v
    }
}
