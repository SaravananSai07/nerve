//! The agent's side of synced folders (`super::super::folders` holds the
//! engine): keeping each folder's index fresh, pushing what members
//! lack, landing what they send, and the join/leave messages. Long-lived
//! agents only; a viewing TUI sees the result through local.json.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::PublicKey;
use notify::RecommendedWatcher;
use serde::{Deserialize, Serialize};

use super::approvals::by_prefix;
use super::{Agent, Done};
use crate::state::session::SessionState;
use crate::sync::fleet::{Fleet, FolderCfg};
use crate::sync::folders::{self, Book, Entry, Moved, Moving, Others, Plan, View, When};
use crate::sync::protocol::{self, Asked, Body, Envelope, FolderPush, FolderSet, FolderStatus, XferOffer, XferWhat};
use crate::util::spawn_named;

/// A peer whose state is older than this is offline: changes wait.
const OFFLINE_SECS: u64 = 180;
/// Quiet time after a file changes before a watched folder rescans.
const SETTLE: Duration = Duration::from_secs(2);
const RETRY_FIRST: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
/// A transfer with no answer for this long is given up on.
const GIVE_UP: Duration = Duration::from_secs(30 * 60);

/// What the UI and CLI ask about synced folders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum FolderReq {
    /// Start syncing `path` with these devices. `yes`: a person asked for
    /// it (the TUI), so the others need not ask anyone.
    Add { path: PathBuf, with: Vec<String>, when: When, others: Others, yes: bool },
    /// Change what the devices sharing it may sync next without asking.
    Others { name: String, others: Others },
    /// Stop syncing it here; the others carry on without this device.
    Remove { name: String },
    /// Send our changes now and ask the others for theirs.
    Now { name: String },
}

/// One folder this device syncs.
pub(super) struct Live {
    pub(super) cfg: FolderCfg,
    pub(super) base: PathBuf,
    book: Book,
    scanning: bool,
    /// Scan again once the running scan is in.
    rescan: bool,
    /// Paths written by arrivals while a scan ran: its view of them is stale.
    touched: BTreeSet<String>,
    dirty_at: Option<Instant>,
    sending: Option<Sending>,
    /// Peer (hex) → when to try again, and the wait after that.
    retry: HashMap<String, (Instant, Duration)>,
    error: Option<String>,
    /// Member (hex) → files it lacks.
    pending: BTreeMap<String, u32>,
    /// Members to tell about it once the first scan says how big it is.
    announce: Vec<(String, PublicKey, FolderSet)>,
    /// Members that refused it: nothing more goes to them.
    refused: BTreeSet<String>,
    _watcher: Option<RecommendedWatcher>,
}

struct Sending {
    id: String,
    peer: String,
    device: String,
    push: FolderPush,
    direct: bool,
    /// Files the peer said it already had.
    skipped: usize,
    started: Instant,
}

/// A transfer into one of our folders, from offer to landing.
pub(super) struct Arrival {
    folder: String,
    from: PublicKey,
    device: String,
    push: FolderPush,
    plan: Plan,
    direct: bool,
    started: Instant,
}

/// "notes.md", "12 files", "3 files, 1 deleted".
fn describe(files: &[String], gone: usize) -> String {
    match files {
        [one] if gone == 0 => one.clone(),
        _ => describe_count(files.len(), gone),
    }
}

/// `describe` for `n` files whose names are not at hand.
fn describe_count(n: usize, gone: usize) -> String {
    let mut parts = Vec::new();
    match n {
        0 => {}
        1 => parts.push("1 file".to_string()),
        n => parts.push(format!("{n} files")),
    }
    if gone > 0 {
        parts.push(format!("{gone} deleted"));
    }
    parts.join(", ")
}

impl Agent {
    fn me_hex(&self) -> String {
        self.t.keys.public_key().to_hex()
    }

    /// The other members of a folder that this device knows.
    pub(super) fn members(&self, cfg: &FolderCfg) -> Vec<(String, PublicKey)> {
        cfg.with
            .iter()
            .filter_map(|n| self.peer_key(n).filter(|k| *k != self.t.keys.public_key()).map(|k| (n.clone(), k)))
            .collect()
    }

    fn online(&self, device: &str) -> bool {
        let now = crate::util::now_secs();
        self.view
            .lock()
            .ok()
            .and_then(|v| v.peers.iter().find(|p| p.name == device).and_then(|p| p.state_generated_at))
            .is_some_and(|g| now.saturating_sub(g) < OFFLINE_SECS)
    }

    /// Follow fleet.toml: start, change or stop folders.
    pub(super) fn folders_follow_fleet(&mut self) {
        if !self.ctx.long_lived() {
            return;
        }
        let file = Fleet::file(&self.ctx.sync_dir);
        let cfgs = self.ctx.fleet.folders.clone();
        self.live.retain(|name, _| cfgs.iter().any(|c| c.name == *name));
        for cfg in cfgs {
            let base = match self.ctx.fleet.place(&cfg.name, &file) {
                Ok(b) => b,
                Err(e) => {
                    self.note(format!("folder {}: {e}", cfg.name));
                    continue;
                }
            };
            if let Some(l) = self.live.get_mut(&cfg.name) {
                let rewatch = l.cfg.when != cfg.when || l.base != base;
                l.cfg = cfg.clone();
                if !rewatch {
                    continue;
                }
                l.base = base;
                l._watcher = None;
            } else {
                let book = Book::load(&self.ctx.sync_dir, &cfg.name);
                self.live.insert(
                    cfg.name.clone(),
                    Live {
                        cfg: cfg.clone(),
                        base,
                        book,
                        scanning: false,
                        rescan: false,
                        touched: BTreeSet::new(),
                        dirty_at: None,
                        sending: None,
                        retry: HashMap::new(),
                        error: None,
                        pending: BTreeMap::new(),
                        announce: Vec::new(),
                        refused: BTreeSet::new(),
                        _watcher: None,
                    },
                );
            }
            let name = cfg.name.clone();
            if cfg.when == When::Watch {
                let l = &self.live[&name];
                let (done, n) = (self.done.clone(), name.clone());
                match folders::watch(&l.base, move || {
                    let _ = done.send(Done::Changed(n.clone()));
                }) {
                    Ok(w) => self.live.get_mut(&name).expect("just added")._watcher = Some(w),
                    Err(e) => self.note(format!(
                        "folder {name}: cannot watch {}: {e}; syncing when a turn ends instead",
                        l.base.display()
                    )),
                }
            }
            if cfg.when != When::Manual {
                self.folder_rescan(&name);
            }
        }
    }

    /// Re-read the folder off the loop; its changes go out once it is in.
    fn folder_rescan(&mut self, name: &str) {
        let Some(l) = self.live.get_mut(name) else { return };
        if l.scanning {
            l.rescan = true;
            return;
        }
        l.scanning = true;
        l.touched.clear();
        let (base, old, done, name) = (l.base.clone(), l.book.index.clone(), self.done.clone(), name.to_string());
        let (fail_done, fail_name) = (done.clone(), name.clone());
        if let Err(e) = spawn_named("folder-scan", move || {
            let _ = done.send(Done::Scanned(name, folders::scan(&base, &old)));
        }) {
            let _ = fail_done.send(Done::Scanned(fail_name, Err(e)));
        }
    }

    /// Sessions that just finished a turn: their folders sync.
    pub(super) fn folder_turns(&mut self) {
        let busy: HashSet<String> = self
            .snapshots
            .iter()
            .filter(|s| matches!(s.detected_state, SessionState::Processing | SessionState::ToolRunning(_)))
            .map(|s| s.id.as_str().to_string())
            .collect();
        let ended: Vec<PathBuf> = self
            .snapshots
            .iter()
            .filter(|s| self.busy_sessions.contains(s.id.as_str()) && !busy.contains(s.id.as_str()))
            .map(|s| s.cwd.clone())
            .collect();
        self.busy_sessions = busy;
        let due: Vec<String> = self
            .live
            .iter()
            .filter(|(_, l)| l.cfg.when == When::Turn && ended.iter().any(|cwd| cwd.starts_with(&l.base)))
            .map(|(n, _)| n.clone())
            .collect();
        for name in due {
            self.folder_rescan(&name);
        }
    }

    /// Something in a watched folder changed: rescan once it settles.
    pub(super) fn folder_changed(&mut self, name: &str) {
        if let Some(l) = self.live.get_mut(name) {
            l.dirty_at = Some(Instant::now());
        }
    }

    /// Every tick: rescan settled folders, give up on stuck transfers, and
    /// push what members lack.
    pub(super) async fn folders_tick(&mut self) {
        let names: Vec<String> = self.live.keys().cloned().collect();
        for name in names {
            let settled = self.live[&name].dirty_at.is_some_and(|t| t.elapsed() >= SETTLE);
            let again = self.live[&name].rescan && !self.live[&name].scanning;
            if settled || again {
                if let Some(l) = self.live.get_mut(&name) {
                    l.dirty_at = None;
                    l.rescan = false;
                }
                self.folder_rescan(&name);
            }
            self.folder_check_sending(&name).await;
            self.folder_push(&name).await;
        }
        let stale: Vec<String> =
            self.arriving.iter().filter(|(_, a)| a.started.elapsed() > 4 * GIVE_UP).map(|(re, _)| re.clone()).collect();
        for re in stale {
            self.incoming.remove(&re);
            let _ = self.folder_landed(&re, false);
        }
    }

    pub(super) async fn folder_scanned(&mut self, name: &str, res: Result<BTreeMap<String, Entry>, String>) {
        let Some(l) = self.live.get_mut(name) else { return };
        l.scanning = false;
        let (files, bytes) = (l.book.index.len() as u32, l.book.index.values().map(|e| e.size).sum());
        match res {
            Ok(mut index) => {
                // Arrivals during the scan know better than it does.
                for rel in std::mem::take(&mut l.touched) {
                    match l.book.index.get(&rel) {
                        Some(e) => index.insert(rel, e.clone()),
                        None => index.remove(&rel),
                    };
                }
                l.book.index = index;
                l.error = None;
            }
            Err(e) => l.error = Some(e),
        }
        let (files, bytes) = if l.error.is_none() {
            (l.book.index.len() as u32, l.book.index.values().map(|e| e.size).sum())
        } else {
            (files, bytes)
        };
        for (dev, pk, set) in std::mem::take(&mut l.announce) {
            self.send_set(pk, &dev, FolderSet { files, bytes, ..set }).await;
        }
        self.folder_save(name);
    }

    /// Save the book and recount what each member lacks.
    fn folder_save(&mut self, name: &str) {
        let members: Vec<String> = match self.live.get(name) {
            Some(l) => self.members(&l.cfg).iter().map(|(_, k)| k.to_hex()).collect(),
            None => return,
        };
        let Some(l) = self.live.get_mut(name) else { return };
        l.pending = members.into_iter().map(|m| (m.clone(), l.book.changes_for(&m).count() as u32)).collect();
        if let Err(e) = l.book.save(&self.ctx.sync_dir, name) {
            l.error = Some(format!("could not save its state: {e}"));
        }
        self.publish_now = true;
    }

    /// A transfer that ended without our hooks hearing (the relay gave
    /// up, the pack failed) or that never got an answer.
    async fn folder_check_sending(&mut self, name: &str) {
        let Some(s) = self.live.get(name).and_then(|l| l.sending.as_ref()) else { return };
        let (id, gone, late) = (s.id.clone(), !self.outgoing.contains_key(&s.id), s.started.elapsed() > GIVE_UP);
        if late {
            if let Some(o) = self.outgoing.remove(&id) {
                self.forget_events(o.events).await;
            }
        }
        if gone || late {
            self.folder_sent(&id, false, if late { "no answer; trying again later" } else { "the transfer stopped" });
        }
    }

    /// Start the next transfer to a member that lacks something and can
    /// take it. One transfer per folder at a time.
    async fn folder_push(&mut self, name: &str) {
        let Some(l) = self.live.get(name) else { return };
        if l.scanning || l.sending.is_some() {
            return;
        }
        let now = Instant::now();
        // Nothing goes to a member before it answers the join: it may be
        // waiting for a person's yes.
        let asked = |dev: &str| self.joins.values().any(|(n, d)| n == name && d == dev) || l.refused.contains(dev);
        let next = self.members(&l.cfg).into_iter().find(|(dev, k)| {
            let hex = k.to_hex();
            l.pending.get(&hex).is_some_and(|n| *n > 0)
                && l.retry.get(&hex).is_none_or(|(at, _)| now >= *at)
                && self.online(dev)
                && !asked(dev)
        });
        let Some((device, pk)) = next else { return };
        let hex = pk.to_hex();
        let changes = l.book.changes_for(&hex);
        let bulk_over = self.ctx.fleet.xfer.bulk_over;
        let push = folders::next_batch(&changes, bulk_over);
        let direct = push.bytes >= bulk_over;
        let id = protocol::new_id();
        let offer = XferOffer {
            place: name.to_string(),
            what: XferWhat::Files { roots: push.files.iter().map(|(r, _)| r.clone()).collect(), delete: false },
            folder: Some(push.clone()),
            ..Default::default()
        };
        if let Some(l) = self.live.get_mut(name) {
            l.sending = Some(Sending {
                id: id.clone(),
                peer: hex,
                device: device.clone(),
                push,
                direct,
                skipped: 0,
                started: now,
            });
        }
        if let Err(e) = self.start_xfer(pk, &device, &id, offer).await {
            // Most likely a file changed or went since the scan.
            self.folder_sent(&id, false, &e);
            self.folder_rescan(name);
        }
    }

    /// Sender: the peer said which offered files it already has.
    pub(super) fn folder_skipped(&mut self, re: &str, n: usize) {
        if let Some(s) = self.live.values_mut().filter_map(|l| l.sending.as_mut()).find(|s| s.id == re) {
            s.skipped = n;
        }
    }

    /// Sender: a transfer of ours ended (nothing happens if it was not one).
    pub(super) fn folder_sent(&mut self, re: &str, ok: bool, msg: &str) {
        let me = self.ctx.fleet.device.clone();
        let Some((name, l)) = self.live.iter_mut().find(|(_, l)| l.sending.as_ref().is_some_and(|s| s.id == re)) else {
            return;
        };
        let name = name.clone();
        let s = l.sending.take().expect("found above");
        if ok {
            l.book.holds(&s.peer, &s.push);
            l.retry.remove(&s.peer);
            l.error = None;
            let sent: Vec<String> = s.push.files.iter().map(|(r, _)| r.clone()).collect();
            if sent.len() > s.skipped || !s.push.gone.is_empty() {
                let what = if s.skipped == 0 {
                    describe(&sent, s.push.gone.len())
                } else {
                    describe_count(sent.len() - s.skipped, s.push.gone.len())
                };
                l.book.record(Moved {
                    at: crate::util::now_secs(),
                    what,
                    from: me,
                    to: s.device,
                    bytes: s.push.bytes,
                    direct: s.direct,
                });
            } else {
                l.book.last_sync = crate::util::now_secs();
            }
        } else {
            let wait = l.retry.get(&s.peer).map(|(_, w)| (*w * 2).min(RETRY_MAX)).unwrap_or(RETRY_FIRST);
            l.retry.insert(s.peer.clone(), (Instant::now() + wait, wait));
            l.error = Some(format!("{}: {msg}", s.device));
        }
        self.folder_save(&name);
    }

    /// Receiver: may `from` send into place `place`, and what do we know
    /// it holds? (`Err`: not a synced folder here, or not a member.)
    pub(super) fn folder_had(&self, place: &str, from: &PublicKey) -> Result<(BTreeMap<String, String>, bool), String> {
        let l = self.live.get(place).ok_or_else(|| format!("{place} is not synced on {}", self.ctx.fleet.device))?;
        if !self.members(&l.cfg).iter().any(|(_, k)| k == from) {
            return Err(format!("{} does not sync {place} with this device", self.peer_name(from).unwrap_or_default()));
        }
        let had = l.book.peers.get(&from.to_hex()).cloned().unwrap_or_default();
        Ok((had, self.me_hex() < from.to_hex()))
    }

    /// Receiver: a transfer into a synced folder is on its way.
    pub(super) fn folder_arriving(&mut self, re: &str, from: PublicKey, device: String, offer: &XferOffer, plan: Plan) {
        let Some(push) = offer.folder.clone() else { return };
        let (folder, direct) = (offer.place.clone(), offer.bulk.is_some());
        self.arriving
            .insert(re.to_string(), Arrival { folder, from, device, push, plan, direct, started: Instant::now() });
    }

    /// Receiver: a transfer into a synced folder is written (or failed).
    /// Finishes the conflicts and deletions, and records what the sender
    /// now holds. `None`: not a folder transfer.
    pub(super) fn folder_landed(&mut self, re: &str, ok: bool) -> Option<Result<String, String>> {
        let a = self.arriving.remove(re)?;
        let me = self.ctx.fleet.device.clone();
        let versions = self.versions();
        let Some(l) = self.live.get_mut(&a.folder) else {
            if let Some(base) = self.ctx.fleet.places.get(&a.folder) {
                folders::undo(Path::new(base), &a.plan);
            }
            return Some(Err(format!("{} is no longer synced here", a.folder)));
        };
        if !ok {
            folders::undo(&l.base, &a.plan);
            return Some(Err("transfer failed".into()));
        }
        let why = format!("Deleted by {}", a.device);
        if let Err(e) = folders::finish(&l.base, &a.plan, |rel| {
            let _ = versions.keep(&a.folder, &l.base, rel, &why);
        }) {
            folders::undo(&l.base, &a.plan);
            return Some(Err(e));
        }
        // What is here now, so the next scan doesn't send it back. The
        // ids are the sender's; -1 makes the next scan check them.
        let entry = |id: &str| Entry { size: 0, mtime_ns: -1, id: id.to_string() };
        let incoming: BTreeMap<&str, &str> = a.push.files.iter().map(|(r, i)| (r.as_str(), i.as_str())).collect();
        for (rel, id) in &a.push.files {
            if !a.plan.held.iter().any(|(h, _, _)| h == rel) {
                l.book.index.insert(rel.clone(), entry(id));
                l.touched.insert(rel.clone());
            }
        }
        for (rel, _, to) in &a.plan.held {
            l.book.index.insert(to.clone(), entry(incoming[rel.as_str()]));
            l.touched.insert(to.clone());
        }
        for (rel, _) in &a.plan.delete {
            if !l.base.join(rel).exists() {
                l.book.index.remove(rel);
                l.touched.insert(rel.clone());
            }
        }
        l.book.holds(&a.from.to_hex(), &a.push);
        let written: Vec<String> =
            a.push.files.iter().filter(|(r, _)| !a.plan.skip.contains(r)).map(|(r, _)| r.clone()).collect();
        if !written.is_empty() || !a.plan.delete.is_empty() {
            l.book.record(Moved {
                at: crate::util::now_secs(),
                what: describe(&written, a.plan.delete.len()),
                from: a.device.clone(),
                to: me,
                bytes: a.push.bytes,
                direct: a.direct,
            });
        } else {
            l.book.last_sync = crate::util::now_secs();
        }
        let mut more = Vec::new();
        if !a.plan.delete.is_empty() {
            more.push(format!("{} deleted as on {}", a.plan.delete.len(), a.device));
        }
        if a.plan.conflicts() > 0 {
            more.push(format!("{} conflict(s) kept as .conflict- copies", a.plan.conflicts()));
        }
        let name = a.folder.clone();
        if !a.plan.copied.is_empty() {
            // Our versions, now conflict copies: let the next scan add them.
            self.folder_rescan(&name);
        }
        self.folder_save(&name);
        Some(Ok(more.join("; ")))
    }

    /// Our state's folders: counts for peers, the whole picture for us.
    pub(super) fn folder_statuses(&self) -> Vec<FolderStatus> {
        let me_hex = self.me_hex();
        let versions = self.versions().list();
        let peers = self.view.lock().map(|v| v.peers.clone()).unwrap_or_default();
        self.live
            .iter()
            .map(|(name, l)| {
                let members = self.members(&l.cfg);
                let behind = members
                    .iter()
                    .filter_map(|(dev, _)| {
                        let st = peers.iter().find(|p| p.name == *dev)?.state.as_ref()?;
                        let n = *st.folders.iter().find(|f| f.name == *name)?.pending.get(&me_hex)?;
                        (n > 0).then(|| (dev.clone(), n))
                    })
                    .collect();
                let moving = l.sending.as_ref().map(|s| {
                    let (done, total) =
                        self.outgoing.get(&s.id).map(|o| (o.stats.sent_chunks, o.stats.chunks)).unwrap_or_default();
                    let files: Vec<String> = s.push.files.iter().map(|(r, _)| r.clone()).collect();
                    Moving {
                        what: describe(&files, s.push.gone.len()),
                        from: self.ctx.fleet.device.clone(),
                        to: s.device.clone(),
                        bytes: s.push.bytes,
                        done,
                        total,
                        direct: s.direct,
                    }
                });
                let moving = moving.or_else(|| {
                    let (re, a) = self.arriving.iter().find(|(_, a)| a.folder == *name)?;
                    let (done, total) = self
                        .incoming
                        .get(re)
                        .map(|i| {
                            let all = i
                                .pack
                                .as_ref()
                                .map(|p| crate::sync::chunks::needed(&p.files(), &Default::default()))
                                .unwrap_or_default();
                            (all.iter().filter(|c| self.store.has(c)).count(), all.len())
                        })
                        .unwrap_or_default();
                    let files: Vec<String> = a.push.files.iter().map(|(r, _)| r.clone()).collect();
                    Some(Moving {
                        what: describe(&files, a.push.gone.len()),
                        from: a.device.clone(),
                        to: self.ctx.fleet.device.clone(),
                        bytes: a.push.bytes,
                        done,
                        total,
                        direct: a.direct,
                    })
                });
                FolderStatus {
                    name: name.clone(),
                    path: l.base.display().to_string(),
                    pending: l.pending.clone(),
                    mine: Some(View {
                        when: l.cfg.when,
                        others: l.cfg.others,
                        held: self.held_by(name),
                        with: l.cfg.with.clone(),
                        offline: members.iter().filter(|(d, _)| !self.online(d)).map(|(d, _)| d.clone()).collect(),
                        behind,
                        unsent: l.pending.values().copied().max().unwrap_or(0),
                        moving,
                        error: l.error.clone(),
                        last_sync: l.book.last_sync,
                        log: l.book.log.clone(),
                        versions: versions
                            .iter()
                            .filter(|v| v.folder == *name)
                            .take(20)
                            .map(|v| (v.id.clone(), v.at, v.rel.clone(), v.why.clone()))
                            .collect(),
                    }),
                }
            })
            .collect()
    }

    // ---- joining and leaving ----

    async fn send_set(&mut self, to: PublicKey, device: &str, set: FolderSet) {
        let name = set.name.clone();
        let env = Envelope::new(&self.ctx.fleet.device, Body::FolderSet(set));
        self.joins.insert(env.id.clone(), (name, device.to_string()));
        if let Err(e) = self.t.send(&to, &env).await {
            self.note(format!("could not tell {device} about the folder: {e}"));
        }
    }

    /// Save fleet.toml changes and follow them.
    pub(super) async fn edit_fleet<T>(&mut self, f: impl FnOnce(&mut Fleet) -> Result<T, String>) -> Result<T, String> {
        let dir = self.ctx.sync_dir.clone();
        let mut fleet = Fleet::load(&dir).ok_or("no fleet.toml")?;
        let msg = f(&mut fleet)?;
        fleet.save(&dir).map_err(|e| e.to_string())?;
        self.fleet_mtime = None;
        self.reload_fleet().await;
        Ok(msg)
    }

    pub(super) async fn folder_request(&mut self, req: FolderReq) -> Result<String, String> {
        match req {
            FolderReq::Add { path, with, when, others, yes } => self.folder_add(path, with, when, others, yes).await,
            FolderReq::Others { name, others } => self.folder_others(&name, others).await,
            FolderReq::Remove { name } => self.folder_remove(&name, true).await,
            FolderReq::Now { name } => match self.live.get(&name).map(|l| self.members(&l.cfg)) {
                None => Err(format!("{name} is not synced here")),
                Some(members) => {
                    self.folder_rescan(&name);
                    if let Some(l) = self.live.get_mut(&name) {
                        l.retry.clear();
                    }
                    for (_, pk) in members {
                        self.reply(&pk, Body::FolderNow { name: name.clone() }).await;
                    }
                    Ok(format!("syncing {name} now"))
                }
            },
        }
    }

    async fn folder_add(
        &mut self,
        path: PathBuf,
        with: Vec<String>,
        when: When,
        others: Others,
        yes: bool,
    ) -> Result<String, String> {
        let path = std::fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !path.is_dir() {
            return Err(format!("{} is not a folder", path.display()));
        }
        self.may_sync(&path)?;
        let mut members = Vec::new();
        for d in &with {
            members.push((
                d.clone(),
                self.peer_key(d)
                    .filter(|k| *k != self.t.keys.public_key())
                    .ok_or_else(|| format!("unknown device {d}"))?,
            ));
        }
        if members.is_empty() {
            return Err("choose at least one other device".into());
        }
        let p = path.display().to_string();
        let file = Fleet::file(&self.ctx.sync_dir);
        let fleet = &self.ctx.fleet;
        if let Some(c) = fleet.folders.iter().find(|c| fleet.place(&c.name, &file).is_ok_and(|b| b == path)) {
            return Err(format!("{p} is already synced as {}", c.name));
        }
        let name = match fleet.places.iter().find(|(_, v)| Path::new(v) == path) {
            Some((n, _)) => n.clone(),
            None => {
                let taken: BTreeSet<String> =
                    fleet.places.keys().chain(fleet.folders.iter().map(|c| &c.name)).cloned().collect();
                folders::name_for(&path, &taken)
            }
        };
        let cfg = FolderCfg { name: name.clone(), with: with.clone(), when, others };
        let (n, pp, c) = (name.clone(), p.clone(), cfg.clone());
        self.edit_fleet(move |f| {
            f.places.insert(n, pp);
            f.folders.push(c);
            Ok(String::new())
        })
        .await?;
        let home_rel = crate::paths::tilde(&path.to_string_lossy()).strip_prefix("~/").map(String::from);
        let mut all: Vec<String> = members.iter().map(|(_, k)| k.to_hex()).collect();
        all.push(self.me_hex());
        let set = FolderSet {
            name: name.clone(),
            members: all,
            when,
            src_path: p.clone(),
            home_rel,
            others,
            yes,
            ..Default::default()
        };
        if let Some(l) = self.live.get_mut(&name) {
            l.announce = members.into_iter().map(|(dev, pk)| (dev, pk, set.clone())).collect();
        }
        self.folder_rescan(&name);
        let ask = if yes {
            ""
        } else {
            ". A device whose folder rules don't cover it asks for a yes first: see `nerve requests`"
        };
        Ok(format!("syncing {p} as {name} with {}{ask}", with.join(", ")))
    }

    pub(super) async fn folder_others(&mut self, name: &str, others: Others) -> Result<String, String> {
        let l = self.live.get(name).ok_or_else(|| format!("{name} is not synced here"))?;
        let members = self.members(&l.cfg);
        let mut all: Vec<String> = members.iter().map(|(_, k)| k.to_hex()).collect();
        all.push(self.me_hex());
        let set = FolderSet {
            name: name.to_string(),
            members: all,
            when: l.cfg.when,
            src_path: l.base.display().to_string(),
            others,
            ..Default::default()
        };
        let n = name.to_string();
        self.edit_fleet(move |f| {
            f.folders.iter_mut().filter(|c| c.name == n).for_each(|c| c.others = others);
            Ok(String::new())
        })
        .await?;
        for (dev, pk) in members {
            self.send_set(pk, &dev, set.clone()).await;
        }
        let parent =
            self.live.get(name).and_then(|l| l.base.parent().map(|p| p.display().to_string())).unwrap_or_default();
        Ok(format!("{name}: {}", others.label(&parent).to_lowercase()))
    }

    /// Stop syncing `name` here; `tell` the others to carry on without us.
    async fn folder_remove(&mut self, name: &str, tell: bool) -> Result<String, String> {
        let l = self.live.get(name).ok_or_else(|| format!("{name} is not synced here"))?;
        let members = self.members(&l.cfg);
        let (path, when) = (l.base.display().to_string(), l.cfg.when);
        let n = name.to_string();
        self.edit_fleet(move |f| {
            f.folders.retain(|c| c.name != n);
            Ok(String::new())
        })
        .await?;
        Book::forget(&self.ctx.sync_dir, name);
        if tell {
            let rest: Vec<String> = members.iter().map(|(_, k)| k.to_hex()).collect();
            for (dev, pk) in members {
                let set = FolderSet {
                    name: name.to_string(),
                    members: rest.clone(),
                    when,
                    src_path: path.clone(),
                    ..Default::default()
                };
                self.send_set(pk, &dev, set).await;
            }
        }
        Ok(format!("stopped syncing {path}"))
    }

    /// Folders that overlap one just made private stop syncing.
    pub(super) async fn folders_after_private(&mut self) {
        let private = self.ctx.fleet.private.clone();
        let hit: Vec<String> = self
            .live
            .iter()
            .filter(|(_, l)| {
                crate::sync::fleet::inside(&private, &l.base)
                    || private.iter().any(|p| Path::new(p).starts_with(&l.base))
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in hit {
            match self.folder_remove(&name, true).await {
                Ok(m) => self.note(format!("{m}: it is private now")),
                Err(e) => self.note(format!("folder: {e}")),
            }
        }
    }

    /// A member shares a folder with us, or changes who shares it.
    pub(super) async fn on_folder_set(&mut self, from: &PublicKey, id: &str, set: FolderSet) {
        let me = self.me_hex();
        let known = self.live.get(&set.name).is_some_and(|l| self.members(&l.cfg).iter().any(|(_, k)| k == from));
        let with: Vec<String> = set
            .members
            .iter()
            .filter(|h| **h != me)
            .filter_map(|h| PublicKey::parse(h).ok())
            .filter_map(|k| self.peer_name(&k))
            .collect();
        // Left out, left alone, or the sender left: stop syncing it here,
        // and stop waiting for a yes to it.
        let sender_left = !set.members.contains(&from.to_hex()) && !known;
        if !set.members.contains(&me) || with.is_empty() || sender_left {
            self.unhold(from, &set.name);
            let msg = if known {
                self.folder_remove(&set.name, false).await
            } else {
                Ok(format!("{} was not synced here", set.name))
            };
            return match msg {
                Ok(m) => self.ack(from, id, true, m).await,
                Err(e) => self.ack(from, id, false, e).await,
            };
        }
        if self.live.contains_key(&set.name) && !known {
            return self
                .ack(
                    from,
                    id,
                    false,
                    format!("{} already syncs a different folder called {}", self.ctx.fleet.device, set.name),
                )
                .await;
        }
        let (name, when, others) = (set.name.clone(), set.when, set.others);
        let res = if known {
            self.edit_fleet(move |f| {
                if let Some(c) = f.folders.iter_mut().find(|c| c.name == name) {
                    c.with = with;
                    c.when = when;
                    c.others = others;
                }
                Ok(format!("{name}: updated"))
            })
            .await
        } else {
            let dest = folders::landing(&self.ctx.fleet, &set.name, &set.src_path, set.home_rel.as_deref());
            match dest.and_then(|d| self.may_sync(&d).map(|_| d)) {
                Err(e) => Err(e),
                Ok(dest) => match self.folder_offered(from, id, set, dest).await {
                    Some(res) => res,
                    // Held: the answer comes with a person's.
                    None => return,
                },
            }
        };
        match res {
            Ok(m) => self.ack(from, id, true, m).await,
            Err(e) => self.ack(from, id, false, e).await,
        }
    }

    /// Start syncing a folder another device shares, at `dest`.
    pub(super) async fn folder_join(
        &mut self,
        from: &PublicKey,
        set: FolderSet,
        dest: PathBuf,
    ) -> Result<String, String> {
        self.may_sync(&dest)?;
        if self.live.contains_key(&set.name) {
            return Err(format!("{} already syncs a different folder called {}", self.ctx.fleet.device, set.name));
        }
        std::fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        let me = self.me_hex();
        let mut with: Vec<String> = set
            .members
            .iter()
            .filter(|h| **h != me)
            .filter_map(|h| PublicKey::parse(h).ok())
            .filter_map(|k| self.peer_name(&k))
            .collect();
        if let Some(n) = self.peer_name(from).filter(|n| !with.contains(n)) {
            with.push(n);
        }
        let at = format!("{} keeps {} at {}", self.ctx.fleet.device, set.name, dest.display());
        let cfg = FolderCfg { name: set.name.clone(), with, when: set.when, others: set.others };
        self.edit_fleet(move |f| {
            f.places.insert(cfg.name.clone(), dest.display().to_string());
            f.folders.push(cfg);
            Ok(at)
        })
        .await
    }

    /// Put back a version a sync replaced (an id prefix).
    pub(super) fn restore(&mut self, id: &str) -> Result<String, String> {
        let versions = self.versions();
        let all = versions.list();
        let v = by_prefix(&all, id, |v| &v.id)?;
        let base = self
            .live
            .get(&v.folder)
            .map(|l| l.base.clone())
            .ok_or_else(|| format!("{} is not synced here any more", v.folder))?;
        versions.restore(v, &base)?;
        self.folder_rescan(&v.folder);
        Ok(format!(
            "put back {} in {} as it was {}",
            v.rel,
            v.folder,
            crate::util::text::format_age(crate::util::now_secs().saturating_sub(v.at) as f64)
        ))
    }

    /// A folder new to this device: join it now when a person already
    /// said yes or a folder rule covers it, else hold it.
    async fn folder_offered(
        &mut self,
        from: &PublicKey,
        re: &str,
        set: FolderSet,
        dest: std::path::PathBuf,
    ) -> Option<Result<String, String>> {
        let covered = self
            .live
            .values()
            .any(|l| l.cfg.others.covers(&l.base, &dest) && self.members(&l.cfg).iter().any(|(_, k)| k == from));
        if set.yes || covered {
            return Some(self.folder_join(from, set, dest).await);
        }
        self.hold(from, Asked::Folder { set, lands: dest.display().to_string(), re: re.to_string() });
        None
    }

    /// Is `path` somewhere this device would sync at all?
    fn may_sync(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(d) = folders::never(path, &[self.ctx.paths.config_dir().to_path_buf()]) {
            return Err(format!("nerve never syncs {}", d.display()));
        }
        if crate::sync::fleet::inside(&self.ctx.fleet.private, path) {
            return Err(format!("{} is private on {}", path.display(), self.ctx.fleet.device));
        }
        Ok(())
    }

    /// Our own folders that wait for a yes, by member names.
    fn held_by(&self, name: &str) -> Vec<String> {
        let me = self.t.keys.public_key().to_hex();
        let peers = self.view.lock().map(|v| v.peers.clone()).unwrap_or_default();
        peers
            .iter()
            .filter(|p| {
                p.state.as_ref().is_some_and(|s| {
                    s.approvals
                        .iter()
                        .any(|a| a.from == me && matches!(&a.what, Asked::Folder { set, .. } if set.name == name))
                })
            })
            .map(|p| p.name.clone())
            .collect()
    }

    /// A member asks for our changes now.
    pub(super) fn on_folder_now(&mut self, from: &PublicKey, name: &str) {
        let member = self.live.get(name).is_some_and(|l| self.members(&l.cfg).iter().any(|(_, k)| k == from));
        if member {
            if let Some(l) = self.live.get_mut(name) {
                l.retry.remove(&from.to_hex());
            }
            self.folder_rescan(name);
        }
    }

    /// The answer to a join or leave we sent.
    pub(super) fn folder_set_answered(&mut self, re: &str, ok: bool, msg: &str) {
        let Some((name, device)) = self.joins.remove(re) else { return };
        if let Some(l) = self.live.get_mut(&name) {
            l.error = (!ok).then(|| format!("{device}: {msg}"));
            if !ok {
                l.refused.insert(device);
            }
            // A yes on a folder with nothing to send is its first sync.
            if ok && l.book.last_sync == 0 {
                l.book.last_sync = crate::util::now_secs();
            }
        }
    }
}
