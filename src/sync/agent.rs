//! The sync agent: one per device. Publishes this device's state to every
//! peer, executes commands peers send (type into a session, answer a
//! prompt, hand a session over, start one) and keeps a view of the
//! peers for the TUI and CLI.
//!
//! Runs on its own thread with a single-threaded tokio runtime. It is a
//! worker in nerve's sense: all I/O happens here, the UI only reads
//! `SyncView` and sends `Request`s.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::PublicKey;
use nostr_sdk::prelude::{ClientNotification, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::chunks::{FileRef, Store};
use super::connect;
use super::fleet::{self, Fleet};
use super::folders;
use super::inject;
use super::ipc::{self, Outcome};
use super::mailbox::Mailbox;
use super::modhub::{HubEvent, ModHub};
use super::protocol::{
    self, Blob, Body, DeviceState, Envelope, FetchWhat, HandoffOffer, Reassembly, RunSpec, RunStatus, StartSpec,
    XferOffer,
};
use super::runs;
use super::transport::{Inbound, RelayHealth, Seen, Transport};
use super::versions::Versions;
use crate::detect::claude::{discover_sessions_with, ScanCaches};
use crate::detect::process::ProcessTable;
use crate::lockfile::{LockError, LockFile};
use crate::notify::Notifier;
use crate::notify::{NotifyBook, NotifyPrefs};
use crate::state::session::{DiscoverySnapshot, SessionKind};
use crate::util::sanitize::Sanitised;
use crate::util::spawn_named;

mod approvals;
mod folder_sync;
mod publish;
use crate::detect::jsonl;
use publish::{status_line, tail_lines, Alerts};
mod transfer;
pub(crate) use approvals::FleetReq;
pub(crate) use folder_sync::FolderReq;
use transfer::{Incoming, Landed, Outgoing};

/// What a peer looks like from here. Strings in `state` came off the
/// network: sanitise before rendering.
#[derive(Debug, Clone, Default)]
pub(crate) struct PeerView {
    pub(crate) name: String,
    pub(crate) state: Option<DeviceState>,
    /// When the peer generated the state we hold (its clock).
    pub(crate) state_generated_at: Option<u64>,
    /// When we last heard anything from it (our clock).
    pub(crate) last_heard: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SyncView {
    pub(crate) me: String,
    pub(crate) running: bool,
    pub(crate) error: Option<String>,
    pub(crate) relays: Vec<RelayHealth>,
    pub(crate) peers: Vec<PeerView>,
    /// Recent activity, newest last.
    pub(crate) log: VecDeque<String>,
    /// How many lines were ever noted: `log` stops growing at `LOG_LINES`,
    /// so followers count by this.
    pub(crate) seq: u64,
    /// Hand-offs in either direction: (title, chunks done, chunks total).
    pub(crate) transfers: Vec<(String, usize, usize)>,
    pub(crate) activity: Option<PathBuf>,
    /// fleet.toml `private` / `private_sessions`: what stays on this device.
    pub(crate) private: Vec<String>,
    pub(crate) private_sessions: Vec<String>,
    /// This device's own state, private sessions included: what its mods
    /// are asking, which sessions take input, who hears when they finish.
    pub(crate) local: Option<DeviceState>,
    pub(crate) notify: NotifyBook,
    /// Key (hex) → name, for every device here and this one.
    pub(crate) names: Vec<(String, String)>,
    /// `[xfer] bulk_over`: files this big go over rsync.
    pub(crate) bulk_over: u64,
}

/// Lines of activity `SyncView` keeps.
const LOG_LINES: usize = 200;

impl SyncView {
    /// Peers' text reaches here (ack messages, run titles), so it is
    /// sanitised, and kept to one line so it can't pose as another.
    fn note(&mut self, msg: impl Into<String>) {
        let msg = Sanitised::new(msg.into());
        let line = format!("{} {}", ipc::clock(crate::util::now_secs()), msg.as_str().replace(['\n', '\r'], " "));
        crate::log_info!("sync: {line}");
        if let Some(f) = &self.activity {
            ipc::append_activity(f, &line);
        }
        self.log.push_back(line);
        self.seq += 1;
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// The lines noted after the first `seen` (by `seq`) that the log
    /// still holds, oldest first.
    pub(crate) fn lines_since(&self, seen: u64) -> impl Iterator<Item = &String> {
        let new = (self.seq.saturating_sub(seen) as usize).min(self.log.len());
        self.log.iter().skip(self.log.len() - new)
    }
}

/// Things the UI / CLI ask the agent to do. Adjacently tagged, since
/// `Folder` and `Fleet` carry enums that tag themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c", rename_all = "snake_case")]
pub(crate) enum Request {
    To {
        device: String,
        body: Body,
    },
    SyncNow,
    Handoff {
        session_id: String,
        cwd: PathBuf,
        device: String,
        continue_with: Option<String>,
        place: Option<String>,
        force: bool,
    },
    /// The request's id becomes `spec.id`.
    Run {
        spec: RunSpec,
        device: String,
    },
    /// Files or refs into a place on `device` (`pull`: from it into ours).
    Xfer {
        device: String,
        offer: XferOffer,
        pull: bool,
    },
    SetAway(bool),
    /// Keep a folder or one session on this device, or make it visible
    /// again (fleet.toml `private` / `private_sessions`).
    SetPrivate {
        target: fleet::PrivateTarget,
        on: bool,
    },
    /// Act on a session running here, as a peer would (send, answer,
    /// stop, set notify). Private sessions included.
    Local(Body),
    /// Fleet-wide notification settings; they spread to every device.
    SetNotifyPrefs(NotifyPrefs),
    /// The session a focused TUI has open, as (device, session), or none.
    Viewing(Option<(String, String)>),
    Folder(FolderReq),
    Fleet(FleetReq),
}

/// Fetched artefacts (the fetch's id, title, text), newest last. Apart
/// from `SyncView`, which the TUI clones every tick.
type Blobs = Arc<Mutex<VecDeque<(String, String, String)>>>;

pub(crate) struct AgentHandle {
    pub(crate) view: Arc<Mutex<SyncView>>,
    blobs: Blobs,
    tx: UnboundedSender<(String, Request)>,
}

impl AgentHandle {
    /// File `r`; returns the id its outcome line will name.
    pub(crate) fn request(&self, r: Request) -> String {
        let id = protocol::new_id();
        self.request_as(id.clone(), r);
        id
    }

    /// File `r` under an id the caller chose.
    pub(super) fn request_as(&self, id: String, r: Request) {
        let _ = self.tx.send((id, r));
    }

    pub(crate) fn snapshot(&self) -> SyncView {
        self.view.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// The newest fetched artefact: (title, text).
    pub(crate) fn last_blob(&self) -> Option<(String, String)> {
        self.blobs.lock().ok().and_then(|b| b.back().map(|(_, title, text)| (title.clone(), text.clone())))
    }

    /// What the fetch filed as request `id` brought back, once it is all here.
    pub(crate) fn blob(&self, id: &str) -> Option<String> {
        self.blobs.lock().ok()?.iter().rev().find(|(re, ..)| re == id).map(|(.., text)| text.clone())
    }
}

pub(crate) struct Ctx {
    pub(crate) paths: crate::paths::Paths,
    pub(crate) sync_dir: PathBuf,
    pub(crate) fleet: Fleet,
    pub(crate) statusline_dir: Option<PathBuf>,
    /// `agent.lock`, held by the one long-lived agent per config (the
    /// TUI's or `nerve sync agent`'s). A viewing TUI or one-shot CLI
    /// command runs without it.
    pub(crate) lock: Option<LockFile>,
    /// Shows this device's notifications; long-lived agents only.
    pub(crate) notifier: Option<Notifier>,
}

impl Ctx {
    /// Only the long-lived agent answers hooks and takes spooled requests.
    fn long_lived(&self) -> bool {
        self.lock.is_some()
    }
}

/// This device's full state, for a TUI whose agent only views.
pub(crate) fn local_file(sync_dir: &Path) -> PathBuf {
    sync_dir.join("local.json")
}

pub(crate) fn away_flag(sync_dir: &Path) -> PathBuf {
    sync_dir.join("away")
}

/// Take `agent.lock`: one long-lived agent per config, so the TUI and the
/// background service never both work for this device. An flock, so it
/// goes with the process that held it, crash or not.
pub(crate) fn take_lock(sync_dir: &Path) -> Result<LockFile, LockError> {
    std::fs::create_dir_all(sync_dir).map_err(LockError::Io)?;
    LockFile::at(&sync_dir.join("agent.lock"))
}

fn heartbeat_file(sync_dir: &Path) -> PathBuf {
    sync_dir.join("agent.alive")
}

/// Is a long-lived agent running for this config dir? Hooks only wait
/// for a remote answer when someone will actually forward it. The
/// heartbeat names its pid, so a crashed agent stops counting at once.
pub(crate) fn agent_alive(sync_dir: &Path) -> bool {
    let path = heartbeat_file(sync_dir);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(30));
    let pid = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok());
    fresh && pid.is_some_and(|p| nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None).is_ok())
}

pub(crate) fn spawn(ctx: Ctx) -> std::io::Result<AgentHandle> {
    let view = Arc::new(Mutex::new(SyncView {
        me: ctx.fleet.device.clone(),
        private: ctx.fleet.private.clone(),
        private_sessions: ctx.fleet.private_sessions.clone(),
        notify: NotifyBook::load(ctx.paths.config_dir()),
        bulk_over: ctx.fleet.xfer.bulk_over,
        ..Default::default()
    }));
    let (tx, rx) = unbounded_channel();
    let v = view.clone();
    let blobs = Blobs::default();
    let b = blobs.clone();
    std::thread::Builder::new().name("nerve-sync".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                if let Ok(mut g) = v.lock() {
                    g.error = Some(format!("runtime: {e}"));
                }
                return;
            }
        };
        rt.block_on(run(ctx, v, b, rx));
    })?;
    Ok(AgentHandle { view, blobs, tx })
}

/// What discovery keeps between scans.
struct Scanner {
    table: ProcessTable,
    caches: ScanCaches,
}

struct Agent {
    ctx: Ctx,
    t: Transport,
    view: Arc<Mutex<SyncView>>,
    blobs: Blobs,
    peers: Vec<(String, PublicKey)>,
    mailbox: Mailbox,
    seen: Seen,
    scanner: Arc<Mutex<Scanner>>,
    caps: Vec<String>,
    last_state: Option<String>,
    last_publish: Option<Instant>,
    publish_now: bool,
    snapshots: Vec<DiscoverySnapshot>,
    reassembly: HashMap<String, Reassembly>,
    /// Background threads report here when they finish.
    done: UnboundedSender<Done>,
    /// Runs on this device: id → status, and who to tell.
    runs: BTreeMap<String, (RunStatus, Option<PublicKey>)>,
    store: Arc<Store>,
    /// Hand-offs this device is sending, by offer id.
    outgoing: HashMap<String, Outgoing>,
    /// Hand-offs this device is receiving, by offer id.
    incoming: HashMap<String, Incoming>,
    hub: Option<Arc<ModHub>>,
    /// Prompts handed to a mod, waiting for its ack: command id → sender.
    mod_cmds: HashMap<String, PublicKey>,
    open_sub: Option<nostr_sdk::prelude::SubscriptionId>,
    fleet_mtime: Option<std::time::SystemTime>,
    /// Pulls this device asked for: get id → the peer asked.
    pending_gets: HashMap<String, PublicKey>,
    /// Runs this device submitted: id → output lines already logged.
    my_runs: HashMap<String, u64>,
    book: NotifyBook,
    book_mtime: Option<std::time::SystemTime>,
    alerts: Alerts,
    last_build: Option<Instant>,
    /// What local.json holds (long-lived) or held when last read (viewer).
    local_fp: String,
    local_mtime: Option<std::time::SystemTime>,
    /// Synced folders, by name (long-lived agents only).
    live: BTreeMap<String, folder_sync::Live>,
    /// Transfers into synced folders, by offer id.
    arriving: HashMap<String, folder_sync::Arrival>,
    /// Sessions working at the last scan, to see turns end.
    busy_sessions: HashSet<String>,
    /// Folder joins and leaves we sent: id → (folder, device).
    joins: HashMap<String, (String, String)>,
    /// What waits here for a person's yes.
    approvals: Vec<protocol::Approval>,
    /// The code this device shows, until it is used or expires.
    invite: Option<(String, u64)>,
}

/// How often the loop looks for work: spooled requests, the mod hub,
/// folders that settled.
const TICK: Duration = Duration::from_millis(500);
const SCAN_EVERY: Duration = Duration::from_secs(3);
/// fleet.toml and notify.toml are re-read, and the heartbeat written.
const RELOAD_EVERY: Duration = Duration::from_secs(5);
const HEALTH_EVERY: Duration = Duration::from_secs(10);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// On start, look this far before the last message handled, for any
/// still in flight.
const SINCE_SLACK_SECS: u64 = 120;
/// Fetched artefacts `Blobs` keeps.
const BLOBS_KEPT: usize = 10;
/// `fetch diff` is cut at this many bytes.
const DIFF_MAX: usize = 512 * 1024;
/// Transcript lines for `fetch log`, and per subagent in `fetch subagents`.
const LOG_TAIL: usize = 200;
const SUBAGENT_TAIL: usize = 12;
/// Transcript entries read for `FetchWhat::Transcript`, the longest one
/// sent, and the most bytes sent.
const TRANSCRIPT_ENTRIES: usize = 300;
const ENTRY_CHARS: usize = 12_000;
const TRANSCRIPT_BYTES: usize = 96 * 1024;
/// Instructions older than this are refused rather than typed.
const SEND_MAX_AGE_SECS: u64 = 300;

/// What a background thread finished.
enum Done {
    /// A run's new status.
    Run(RunStatus),
    /// A session started for `from`: (request id, outcome).
    Started(PublicKey, String, Result<String, String>),
    /// A transfer into this device, unpacked (or not).
    Landed(Landed),
    /// A transfer from this device, packed (or not): (offer id, outcome).
    Packed(String, Result<(Vec<FileRef>, FileRef), String>),
    /// A bulk copy this device ran as the sender: (offer id, outcome).
    Bulk(String, Result<String, String>),
    /// A synced folder, re-read.
    Scanned(String, Result<BTreeMap<String, folders::Entry>, String>),
    /// Something changed in a watched folder.
    Changed(String),
}

/// Onto the relays, subscribed to the fleet's messages since `since`.
async fn connect(ctx: &Ctx, since: u64) -> Result<(Transport, Vec<(String, PublicKey)>), String> {
    let keys = fleet::device_keys(&ctx.sync_dir).map_err(|e| format!("device key: {e}"))?;
    let t = Transport::connect(keys, &ctx.fleet.relays).await?;
    let peers = ctx.fleet.peer_keys();
    let mut authors: Vec<PublicKey> = peers.iter().map(|(_, k)| *k).collect();
    authors.push(t.keys.public_key());
    if let Err(e) = t.subscribe(&authors, since).await {
        t.disconnect().await;
        return Err(format!("subscribe: {e}"));
    }
    Ok((t, peers))
}

/// How long to wait before connecting again after `failures` failed
/// tries: doubling from 2 s, at most `RETRY_MAX`.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(2u64.saturating_mul(1 << failures.min(16))).min(RETRY_MAX)
}

const RETRY_MAX: Duration = Duration::from_secs(120);

async fn run(mut ctx: Ctx, view: Arc<Mutex<SyncView>>, blobs: Blobs, mut rx: UnboundedReceiver<(String, Request)>) {
    let since_file = ctx.sync_dir.join("since");
    let since = std::fs::read_to_string(&since_file)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| crate::util::now_secs().saturating_sub(protocol::MSG_TTL_SECS))
        .saturating_sub(SINCE_SLACK_SECS);
    // An unreachable relay is retried until it answers, or until whoever
    // started this agent drops it. Requests wait in `rx` meanwhile.
    let mut failures = 0;
    let (t, peers) = loop {
        match connect(&ctx, since).await {
            Ok(c) => break c,
            Err(e) => {
                let wait = backoff(failures);
                failures += 1;
                fail(&view, e, wait);
                tokio::time::sleep(wait).await;
                if rx.is_closed() {
                    return;
                }
                // A relay added meanwhile counts on the next try.
                if let Some(f) = Fleet::load(&ctx.sync_dir) {
                    ctx.fleet = f;
                }
            }
        }
    };
    if let Ok(mut v) = view.lock() {
        v.running = true;
        v.error = None;
        v.peers = peers.iter().map(|(n, _)| PeerView { name: n.clone(), ..Default::default() }).collect();
        if ctx.long_lived() {
            let f = ipc::activity_file(&ctx.sync_dir);
            // Keep it short: only the tail matters to anyone reading it.
            if std::fs::metadata(&f).is_ok_and(|m| m.len() > 1024 * 1024) {
                let _ = std::fs::remove_file(&f);
            }
            v.activity = Some(f);
        }
        v.note(format!("agent up on {} relay(s), {} peer(s)", ctx.fleet.relays.len(), peers.len()));
    }
    let (done, mut done_rx) = unbounded_channel();
    // Sessions running the nerve mod connect here (long-lived agents only:
    // a one-shot CLI must not take the socket from the real agent).
    let hub = if ctx.long_lived() {
        let hub = Arc::new(ModHub::default());
        let path = super::modhub::socket_path(&ctx.sync_dir);
        let h = hub.clone();
        let v = view.clone();
        tokio::spawn(async move {
            if let Err(e) = super::modhub::serve(path.clone(), h).await {
                if let Ok(mut g) = v.lock() {
                    g.note(format!("mod socket {} unavailable: {e}", path.display()));
                }
            }
        });
        Some(hub)
    } else {
        None
    };
    let ctx_sync_dir = ctx.sync_dir.clone();
    let book = NotifyBook::load(ctx.paths.config_dir());
    let mut a = Agent {
        caps: fleet::detect_caps(&ctx.fleet.caps),
        mailbox: Mailbox::new(&ctx.sync_dir),
        seen: Seen::load(ctx.sync_dir.join("seen")),
        scanner: Arc::new(Mutex::new(Scanner {
            table: ProcessTable::refreshed(),
            caches: ScanCaches::new(ctx.statusline_dir.clone()),
        })),
        ctx,
        t,
        view,
        blobs,
        peers,
        last_state: None,
        last_publish: None,
        publish_now: true,
        snapshots: Vec::new(),
        reassembly: HashMap::new(),
        done,
        runs: BTreeMap::new(),
        store: Arc::new(Store::new(&ctx_sync_dir)),
        outgoing: HashMap::new(),
        incoming: HashMap::new(),
        hub,
        mod_cmds: HashMap::new(),
        open_sub: None,
        fleet_mtime: None,
        pending_gets: HashMap::new(),
        my_runs: HashMap::new(),
        book,
        book_mtime: None,
        alerts: Alerts::default(),
        last_build: None,
        local_fp: String::new(),
        local_mtime: None,
        live: BTreeMap::new(),
        arriving: HashMap::new(),
        busy_sessions: HashSet::new(),
        joins: HashMap::new(),
        approvals: Vec::new(),
        invite: None,
    };
    if a.ctx.long_lived() {
        a.approvals_load();
    }
    a.share_names();
    a.folders_follow_fleet();
    let mut notifications = a.t.client.notifications();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut last_health = Instant::now() - Duration::from_secs(60);
    let mut last_fleet = Instant::now();
    let mut last_prune = Instant::now();
    a.fleet_mtime = fleet_mtime(&a.ctx.sync_dir);
    a.heartbeat();
    loop {
        tokio::select! {
            n = notifications.next() => {
                let Some(ClientNotification::Event { event, .. }) = n else {
                    if n.is_none() { break; }
                    continue;
                };
                if let Some(inb) = a.t.open(&event) {
                    a.handle(inb).await;
                    let _ = std::fs::write(&since_file, crate::util::now_secs().to_string());
                }
            }
            r = rx.recv() => {
                let Some((id, r)) = r else {
                    a.t.disconnect().await;
                    return;
                };
                a.request(id, r).await;
            }
            Some(d) = done_rx.recv() => a.finished(d).await,
            _ = tick.tick() => {
                if a.ctx.long_lived() {
                    for (id, r) in ipc::drain(&a.ctx.sync_dir) {
                        match r {
                            Ok(r) => a.request(id, r).await,
                            Err(e) => a.outcome(&id, Err(e)),
                        }
                    }
                }
                a.drain_hub().await;
                if a.ctx.long_lived() {
                    a.folders_tick().await;
                }
                if a.ctx.long_lived() && last_prune.elapsed() >= PRUNE_EVERY {
                    last_prune = Instant::now();
                    let store = a.store.clone();
                    let versions = a.versions();
                    let keep = a.ctx.fleet.keep_versions_hours * 3600;
                    tokio::task::spawn_blocking(move || {
                        store.prune();
                        versions.prune(keep);
                    });
                }
                if last_fleet.elapsed() >= RELOAD_EVERY {
                    last_fleet = Instant::now();
                    a.heartbeat();
                    a.reload_book();
                    a.reload_fleet().await;
                    a.sync_invite_subscription().await;
                }
                if last_scan.elapsed() >= SCAN_EVERY {
                    last_scan = Instant::now();
                    a.scan().await;
                }
                if last_health.elapsed() >= HEALTH_EVERY {
                    last_health = Instant::now();
                    let h = a.t.health().await;
                    if let Ok(mut v) = a.view.lock() { v.relays = h; }
                }
                a.maybe_publish().await;
            }
        }
    }
}

fn fail(view: &Arc<Mutex<SyncView>>, e: String, retry: Duration) {
    crate::log_warn!("sync: {e}");
    if let Ok(mut v) = view.lock() {
        v.running = false;
        v.note(format!("{e}; trying again in {}s", retry.as_secs()));
        v.error = Some(e);
    }
}

impl Agent {
    fn note(&self, msg: impl Into<String>) {
        if let Ok(mut v) = self.view.lock() {
            v.note(msg);
        }
    }

    /// Tell `agent_alive` this agent runs (long-lived only).
    fn heartbeat(&self) {
        if self.ctx.long_lived() {
            let _ = std::fs::write(heartbeat_file(&self.ctx.sync_dir), std::process::id().to_string());
        }
    }

    async fn finished(&mut self, d: Done) {
        match d {
            Done::Run(st) => self.on_run(st).await,
            Done::Started(to, re, res) => {
                match res {
                    Ok(m) => self.ack(&to, &re, true, m).await,
                    Err(e) => self.ack(&to, &re, false, e).await,
                }
                self.publish_now = true;
            }
            Done::Landed(l) => self.on_landed(l).await,
            Done::Packed(re, res) => self.on_packed(re, res).await,
            Done::Bulk(re, res) => self.on_bulk(re, res).await,
            Done::Scanned(name, res) => self.folder_scanned(&name, res).await,
            Done::Changed(name) => self.folder_changed(&name),
        }
    }

    fn versions(&self) -> Versions {
        Versions::new(&self.ctx.sync_dir, self.ctx.fleet.xfer.bulk_over)
    }

    fn peer_name(&self, pk: &PublicKey) -> Option<String> {
        if *pk == self.t.keys.public_key() {
            return Some(self.ctx.fleet.device.clone());
        }
        self.peers.iter().find(|(_, k)| k == pk).map(|(n, _)| n.clone())
    }

    fn share_names(&self) {
        let me = (self.t.keys.public_key().to_hex(), self.ctx.fleet.device.clone());
        let names = self.ctx.fleet.peer_keys().into_iter().map(|(n, k)| (k.to_hex(), n)).chain([me]).collect();
        if let Ok(mut v) = self.view.lock() {
            v.names = names;
        }
    }

    fn peer_key(&self, name: &str) -> Option<PublicKey> {
        if name == self.ctx.fleet.device {
            return Some(self.t.keys.public_key());
        }
        self.peers.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }

    /// Discovery runs `ps` and reads transcripts: off the loop.
    async fn scan(&mut self) {
        let scanner = self.scanner.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut s = scanner.lock().unwrap_or_else(|e| e.into_inner());
            let Scanner { table, caches } = &mut *s;
            table.refresh_if_stale(Duration::from_secs(5));
            discover_sessions_with(table, caches).sessions
        })
        .await;
        if let Ok(sessions) = found {
            self.snapshots = sessions;
        }
        if self.ctx.long_lived() {
            self.folder_turns();
        }
    }

    async fn reply(&self, to: &PublicKey, body: Body) {
        let env = Envelope::new(&self.ctx.fleet.device, body);
        if let Err(e) = self.t.send(to, &env).await {
            self.note(format!("send failed: {e}"));
        }
    }

    /// How request `id` ended here, for whoever follows it.
    fn outcome(&self, id: &str, res: Result<String, String>) {
        let (ok, msg) = match res {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        };
        self.outcome_as(id, ok.into(), &msg);
    }

    fn outcome_as(&self, id: &str, outcome: Outcome, msg: &str) {
        self.note(ipc::outcome_line(&self.ctx.fleet.device, outcome, id, msg));
    }

    /// Note how `re` ended, and tell `to`.
    async fn ack(&self, to: &PublicKey, re: &str, ok: bool, msg: impl Into<String>) {
        let msg = msg.into();
        self.outcome_as(re, ok.into(), &msg);
        self.reply(to, Body::Ack { re: re.to_string(), ok, msg, relay: false }).await;
    }

    async fn handle(&mut self, inb: Inbound) {
        let Some(from) = self.peer_name(&inb.sender) else {
            // Not a fleet member. The one thing a stranger may say is
            // "here is the invite secret you handed out".
            if let Body::PairRequest { secret, name, relays, want } = &inb.envelope.body {
                if self.ctx.long_lived() && self.seen.first(&inb.envelope.id) {
                    self.on_pair_request(&inb, secret, name, relays, want.as_deref()).await;
                }
            }
            return;
        };
        let id = inb.envelope.id.clone();
        // A one-shot CLI agent only reads; work addressed to this device
        // waits (stored) for the long-lived agent, unmarked as seen.
        let is_work = inb.envelope.body.is_work();
        // Except the replies to a pull this very agent asked for.
        let asked_for = match &inb.envelope.body {
            Body::XferOffer(o) => o.for_req.as_ref().is_some_and(|r| self.pending_gets.get(r) == Some(&inb.sender)),
            Body::HandoffManifest(m) => self.incoming.contains_key(&m.re),
            Body::ChunkData { re, .. } => self.incoming.contains_key(re),
            _ => false,
        };
        if is_work && !asked_for && !self.ctx.long_lived() {
            return;
        }
        if !self.seen.first(&id) {
            return;
        }
        let now = crate::util::now_secs();
        let is_me = inb.sender == self.t.keys.public_key();
        if !is_me {
            if let Ok(mut v) = self.view.lock() {
                if let Some(p) = v.peers.iter_mut().find(|p| p.name == from) {
                    p.last_heard = Some(now);
                }
            }
        }
        match inb.envelope.body {
            Body::State(state) => {
                if let Ok(mut v) = self.view.lock() {
                    if let Some(p) = v.peers.iter_mut().find(|p| p.name == from) {
                        let newer = p.state_generated_at.is_none_or(|g| state.generated_at >= g);
                        if newer {
                            p.state_generated_at = Some(state.generated_at);
                            p.state = Some((*state).clone());
                        }
                    }
                }
                if self.ctx.long_lived() {
                    if state.notify.updated_at > self.book.prefs.updated_at {
                        self.book.prefs = state.notify.clone();
                        self.save_book();
                    }
                    self.alert(&from, &state);
                }
            }
            Body::Stop { session } => {
                let Some(snap) = self.local_session(&session, is_me).cloned() else {
                    self.ack(&inb.sender, &id, false, "no session by that id on this device").await;
                    return;
                };
                let res = tokio::task::spawn_blocking(move || stop_session(&snap))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                match res {
                    Ok(msg) => self.ack(&inb.sender, &id, true, msg).await,
                    Err(e) => self.ack(&inb.sender, &id, false, e).await,
                }
                self.publish_now = true;
            }
            Body::SetNotify { session, devices } => {
                let Some(sid) = self.local_session(&session, is_me).map(|s| s.id.as_str().to_string()) else {
                    self.ack(&inb.sender, &id, false, "no session by that id on this device").await;
                    return;
                };
                self.book.set_session(&sid, devices.into_iter().collect());
                self.save_book();
                self.publish_now = true;
                self.ack(&inb.sender, &id, true, format!("notify settings for {sid:.8} saved")).await;
            }
            Body::SyncNow => {
                self.publish_now = true;
                self.last_publish = None;
                self.note(format!("{from} asked for a sync"));
            }
            Body::Send { session, text } => {
                // Typing is not idempotent: never replay an old instruction,
                // even if the seen-set was lost (fresh install, wiped state).
                if now.saturating_sub(inb.envelope.sent_at.min(inb.created_at)) > SEND_MAX_AGE_SECS {
                    self.ack(&inb.sender, &id, false, "instruction too old; not typed").await;
                    return;
                }
                let Some(snap) = self.local_session(&session, is_me).cloned() else {
                    self.ack(&inb.sender, &id, false, "no session by that id on this device").await;
                    return;
                };
                // The mod submits it inside Claude (queued if a turn is
                // running); its ack comes back through the hub.
                if let Some(hub) = self.hub.as_ref().filter(|h| h.connected(&session)) {
                    if hub.submit(&session, &id, &text) {
                        self.mod_cmds.insert(id.clone(), inb.sender);
                        return;
                    }
                }
                let res = tokio::task::spawn_blocking(move || type_into(&snap, &text))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                match res {
                    Ok(how) => self.ack(&inb.sender, &id, true, format!("typed into {how}")).await,
                    Err(e) => self.ack(&inb.sender, &id, false, e).await,
                }
                self.publish_now = true;
            }
            Body::Answer { request, answer } => {
                if !is_me && !self.peer_may_answer(&request) {
                    self.ack(&inb.sender, &id, false, "no request by that id on this device").await;
                    return;
                }
                if let Some(hub) = self.hub.as_ref().filter(|h| h.has_request(&request)) {
                    let ok = hub.answer(&request, &answer);
                    let msg = if ok { format!("answered {request}") } else { "request already resolved".into() };
                    self.ack(&inb.sender, &id, ok, msg).await;
                    self.publish_now = true;
                    return;
                }
                match self.mailbox.answer(&request, &answer) {
                    Ok(true) => self.ack(&inb.sender, &id, true, format!("answered {request}")).await,
                    Ok(false) => self.ack(&inb.sender, &id, false, "request already resolved").await,
                    Err(e) => self.ack(&inb.sender, &id, false, e.to_string()).await,
                }
                self.publish_now = true;
            }
            Body::Fetch { session, what } => {
                let res = match self.local_session(&session, is_me).cloned() {
                    Some(snap) => tokio::task::spawn_blocking(move || fetch(&snap, what))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string())),
                    None => Err("no session by that id on this device".into()),
                };
                match res {
                    Ok((title, bytes)) => {
                        for b in protocol::chunk(&id, &title, &bytes) {
                            let env = Envelope::new(&self.ctx.fleet.device, Body::Blob(b));
                            if let Err(e) = self.t.send_bulk(&inb.sender, &env).await {
                                self.note(format!("blob send failed: {e}"));
                                break;
                            }
                        }
                        self.note(format!("sent {title} ({} bytes) to {from}", bytes.len()));
                    }
                    Err(e) => self.ack(&inb.sender, &id, false, e).await,
                }
            }
            Body::Blob(b) => self.on_blob(b).await,
            Body::HandoffOffer(offer) => self.on_offer(&inb.sender, &from, id.clone(), offer).await,
            Body::HandoffHave(have) => self.on_have(&inb.sender, have),
            Body::HandoffManifest(m) => self.on_manifest(&inb.sender, m).await,
            Body::ChunkWant { re, ids } => self.on_chunk_want(&inb.sender, &re, ids).await,
            Body::ChunkData { re, chunks, last } => self.on_chunk_data(&inb.sender, &re, chunks, last).await,
            // Already paired (the joiner re-ran setup): nobody new to meet.
            Body::PairRequest { .. } => {
                self.reply(&inb.sender, Body::Welcome { re: id.clone(), devices: Vec::new() }).await;
                self.ack(&inb.sender, &id, true, format!("{} already trusts '{from}'", self.ctx.fleet.device)).await
            }
            Body::Start(spec) => {
                let age = now.saturating_sub(inb.envelope.sent_at.min(inb.created_at));
                self.start_session(inb.sender, id.clone(), age, spec);
            }
            Body::Unknown => {
                let msg = format!(
                    "{} runs nerve {}, which doesn't know that request (headless jobs are gone: use `nerve sync start`)",
                    self.ctx.fleet.device,
                    env!("CARGO_PKG_VERSION")
                );
                self.ack(&inb.sender, &id, false, msg).await;
            }
            Body::RunSubmit(spec) => {
                let age = now.saturating_sub(inb.envelope.sent_at.min(inb.created_at));
                self.accept_run(&inb.sender, &id, age, spec).await;
            }
            Body::XferOffer(offer) => {
                self.on_xfer_offer(&inb.sender, &from, &inb.envelope.from, id.clone(), offer).await
            }
            Body::FolderSet(set) => self.on_folder_set(&inb.sender, &id, set).await,
            Body::FolderNow { name } => self.on_folder_now(&inb.sender, &name),
            Body::Introduce(m) => self.on_met(&from, m).await,
            Body::Welcome { devices, .. } => {
                for m in devices {
                    self.on_met(&from, m).await;
                }
            }
            Body::Forget { key } => self.on_forget(&inb.sender, &key).await,
            Body::Decide(d) => match self.on_decide(&inb.sender, d).await {
                Ok(m) => {
                    self.note(m.clone());
                    self.ack(&inb.sender, &id, true, m).await;
                }
                Err(e) => self.ack(&inb.sender, &id, false, e).await,
            },
            Body::XferGet(offer) => self.on_xfer_get(inb.sender, &from, &id, offer).await,
            Body::RunUpdate(st) => self.on_run_update(&from, &st),
            Body::Ack { re, ok, msg, relay } => {
                let outcome = if relay { Outcome::RelayFailed } else { ok.into() };
                // A finished hand-off: the target now holds every chunk of
                // it, so the next hand-off to it can skip them.
                if let Some(o) = self.outgoing.remove(&re) {
                    if ok {
                        let _ = self.store.mark_known(&o.to.to_hex(), o.ids);
                    }
                    self.refresh_transfers();
                    // Delivered (or given up on): nothing will ask for these
                    // again, so the relay need not hold them for a day.
                    self.forget_events(o.events).await;
                    self.folder_sent(&re, ok, &msg);
                }
                self.folder_set_answered(&re, ok, &msg);
                // Our own ack noted it already.
                if !is_me {
                    self.note(ipc::outcome_line(&from, outcome, &re, &msg));
                }
                // The sender ended a transfer to us: a bulk copy it ran, or
                // giving up.
                if let Some(inc) = self.incoming.remove(&re) {
                    if let Some(Err(e)) = self.folder_landed(&re, ok) {
                        self.note(format!("folder: {e}"));
                    }
                    // A pull we asked for: report it under the pull's id,
                    // which is what the CLI follows.
                    if let Some(get) = inc.job.for_req() {
                        self.pending_gets.remove(&get);
                        self.note(ipc::outcome_line(&from, outcome, &get, &msg));
                    }
                    self.refresh_transfers();
                }
                self.pending_gets.remove(&re);
            }
        }
    }

    /// Peers only see, so only answer, prompts of visible sessions.
    fn peer_may_answer(&self, request: &str) -> bool {
        let mut pending = self.mailbox.pending();
        if let Some(h) = &self.hub {
            pending.extend(h.pending());
        }
        pending
            .iter()
            .find(|q| q.id == request)
            .is_some_and(|q| self.snapshots.iter().any(|s| s.id.as_str() == q.session && !self.private(s)))
    }

    fn private(&self, s: &DiscoverySnapshot) -> bool {
        self.ctx.fleet.is_private(s.id.as_str(), &s.cwd)
    }

    /// A session running here that the sender may reach: any of them for
    /// this device itself, only visible ones for a peer.
    fn local_session(&self, id: &str, is_me: bool) -> Option<&DiscoverySnapshot> {
        self.snapshots
            .iter()
            .filter(|s| is_me || !self.private(s))
            .find(|s| s.id.as_str() == id || s.id.as_str().starts_with(id))
    }

    async fn on_blob(&mut self, b: Blob) {
        let key = b.re.clone();
        let entry = self.reassembly.entry(key.clone()).or_default();
        let done = match entry.add(&b) {
            Ok(d) => d,
            Err(e) => {
                self.note(format!("bad chunk for {key}: {e}"));
                self.reassembly.remove(&key);
                return;
            }
        };
        let Some(bytes) = done else { return };
        let title = self.reassembly.remove(&key).map(|r| r.title).unwrap_or_default();
        let text = crate::util::sanitize::clean(String::from_utf8_lossy(&bytes).into_owned());
        self.note(format!("received {title} ({} bytes)", bytes.len()));
        if let Ok(mut b) = self.blobs.lock() {
            b.push_back((key, title, text));
            while b.len() > BLOBS_KEPT {
                b.pop_front();
            }
        }
    }

    // ---- pairing ----

    /// Pick up fleet.toml edits (pairing, `nerve sync relay`) without a restart.
    async fn reload_fleet(&mut self) {
        let m = fleet_mtime(&self.ctx.sync_dir);
        if m == self.fleet_mtime {
            return;
        }
        self.fleet_mtime = m;
        let Some(f) = Fleet::load(&self.ctx.sync_dir) else { return };
        let peers = f.peer_keys();
        let added: Vec<String> =
            peers.iter().filter(|(_, k)| !self.peers.iter().any(|(_, o)| o == k)).map(|(n, _)| n.clone()).collect();
        for r in f.relays.iter().filter(|r| !self.ctx.fleet.relays.contains(r)) {
            let _ = self.t.client.add_relay(r.as_str()).await;
            let _ = self.t.client.connect_relay(r.as_str()).await;
        }
        if let Ok(mut v) = self.view.lock() {
            v.private = f.private.clone();
            v.private_sessions = f.private_sessions.clone();
            v.bulk_over = f.xfer.bulk_over;
        }
        self.ctx.fleet = f;
        self.folders_follow_fleet();
        self.share_names();
        if !added.is_empty() || peers.len() != self.peers.len() {
            self.peers = peers;
            let mut authors: Vec<PublicKey> = self.peers.iter().map(|(_, k)| *k).collect();
            authors.push(self.t.keys.public_key());
            let since = crate::util::now_secs().saturating_sub(protocol::MSG_TTL_SECS);
            if let Err(e) = self.t.subscribe(&authors, since).await {
                self.note(format!("resubscribe failed: {e}"));
            }
            if let Ok(mut v) = self.view.lock() {
                for (n, _) in &self.peers {
                    if !v.peers.iter().any(|p| p.name == *n) {
                        v.peers.push(PeerView { name: n.clone(), ..Default::default() });
                    }
                }
                let names: Vec<String> = self.peers.iter().map(|(n, _)| n.clone()).collect();
                v.peers.retain(|p| names.contains(&p.name));
            }
            self.publish_now = true;
            self.last_publish = None;
        }
    }

    /// Listen to strangers only while an invite is open.
    async fn sync_invite_subscription(&mut self) {
        if !self.ctx.long_lived() {
            return;
        }
        let open = connect::any_open(&self.ctx.sync_dir);
        match (&self.open_sub, open) {
            (None, true) => match self.t.subscribe_open(crate::util::now_secs().saturating_sub(60)).await {
                Ok(id) => self.open_sub = Some(id),
                Err(e) => self.note(format!("invite listener failed: {e}")),
            },
            (Some(id), false) => {
                self.t.unsubscribe(id).await;
                self.open_sub = None;
            }
            _ => {}
        }
    }

    // ---- mod hub ----

    async fn drain_hub(&mut self) {
        let Some(hub) = self.hub.clone() else { return };
        for ev in hub.drain_events() {
            match ev {
                HubEvent::Changed => self.publish_now = true,
                HubEvent::Ack { cmd, ok, msg } => {
                    if let Some(to) = self.mod_cmds.remove(&cmd) {
                        self.ack(&to, &cmd, ok, msg).await;
                        self.publish_now = true;
                    }
                }
            }
        }
        let pending: usize = hub.pending().len() + self.mailbox.pending().len();
        let peers: Vec<(String, Option<u64>)> = self
            .view
            .lock()
            .map(|v| {
                v.peers
                    .iter()
                    .map(|p| (p.name.clone(), p.state_generated_at.map(|g| crate::util::now_secs().saturating_sub(g))))
                    .collect()
            })
            .unwrap_or_default();
        hub.set_status(status_line(&peers, pending));
    }

    // ---- runs ----

    async fn accept_run(&mut self, from: &PublicKey, id: &str, age_secs: u64, mut spec: RunSpec) {
        // Starting is what expires: a replayed request must not start a
        // command, though a started one may run as long as it needs.
        if age_secs > SEND_MAX_AGE_SECS {
            return self.ack(from, id, false, "run request is older than 5 minutes; not started").await;
        }
        spec.id = id.to_string();
        let plan = match runs::authorize(&self.ctx.fleet, &Fleet::file(&self.ctx.sync_dir), &spec) {
            Ok(p) => p,
            Err(e) => return self.ack(from, id, false, e).await,
        };
        self.ack(from, id, true, format!("run accepted: {} in {}", spec.argv.join(" "), plan.cwd.display())).await;
        let done = self.done.clone();
        let device = self.ctx.fleet.device.clone();
        let queued = runs::queued(&spec, &device);
        self.runs.insert(spec.id.clone(), (queued.clone(), Some(*from)));
        let fail_done = done.clone();
        if let Err(e) = spawn_named("run", move || {
            runs::run(&spec, &plan, &device, &|st| {
                let _ = done.send(Done::Run(st));
            })
        }) {
            let _ = fail_done.send(Done::Run(RunStatus { phase: "failed".into(), result: Some(e), ..queued }));
        }
        self.publish_now = true;
    }

    /// A run this device submitted: log its new output lines and, once,
    /// how it ended.
    fn on_run_update(&mut self, from: &str, st: &RunStatus) {
        let Some(seen) = self.my_runs.get_mut(&st.id) else { return };
        let new = st.lines.saturating_sub(*seen) as usize;
        *seen = (*seen).max(st.lines);
        let shown = new.min(st.tail.len());
        let mut lines = Vec::new();
        if new > shown {
            lines.push(ipc::output_line(from, &st.id, &format!("… {} line(s) not shown", new - shown)));
        }
        for l in &st.tail[st.tail.len() - shown..] {
            lines.push(ipc::output_line(from, &st.id, Sanitised::new(l.clone()).as_str()));
        }
        let finished = st.phase == "done" || st.phase == "failed";
        if finished {
            self.my_runs.remove(&st.id);
            let code = st.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into());
            let why = crate::util::sanitize::clean(st.result.clone().unwrap_or_default());
            let msg = format!("run finished: exit {code} ({why})");
            lines.push(ipc::outcome_line(from, (st.exit_code == Some(0)).into(), &st.id, &msg));
        }
        for l in lines {
            self.note(l);
        }
    }

    /// A run here has news: keep it, and tell whoever sent it.
    async fn on_run(&mut self, st: RunStatus) {
        let (reply_to, phase_changed) = match self.runs.get(&st.id) {
            Some((j, r)) => (*r, j.phase != st.phase),
            None => (None, true),
        };
        if st.phase == "done" || st.phase == "failed" {
            let how = st.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default();
            self.note(format!("run '{}' {}{how}", st.title, st.phase));
        }
        self.runs.insert(st.id.clone(), (st.clone(), reply_to));
        if let Some(pk) = reply_to {
            if pk != self.t.keys.public_key() {
                self.reply(&pk, Body::RunUpdate(st)).await;
            } else {
                let me = self.ctx.fleet.device.clone();
                self.on_run_update(&me, &st);
            }
        }
        // Output alone rides the next regular publish.
        if phase_changed {
            self.publish_now = true;
        }
    }

    /// Carry out request `id`. Each ends in an outcome line naming `id`:
    /// noted here, or by the device that acks it.
    async fn request(&mut self, id: String, r: Request) {
        // Only the long-lived agent owns the mod socket and notify.toml;
        // a viewer hands these to it.
        if !self.ctx.long_lived()
            && matches!(
                r,
                Request::Local(_)
                    | Request::SetNotifyPrefs(_)
                    | Request::Viewing(_)
                    | Request::Folder(_)
                    | Request::Fleet(_)
            )
        {
            if let Err(e) = ipc::put(&self.ctx.sync_dir, &id, &r) {
                self.note(format!("could not reach the sync agent: {e}"));
            }
            return;
        }
        let me = self.ctx.fleet.device.clone();
        match r {
            Request::Folder(req) => {
                let res = self.folder_request(req).await;
                self.outcome(&id, res);
            }
            Request::Fleet(req) => {
                let res = self.fleet_request(req).await;
                self.outcome(&id, res);
            }
            Request::Local(body) => {
                let envelope = Envelope::with_id(&me, &id, body);
                let inb = Inbound { sender: self.t.keys.public_key(), created_at: crate::util::now_secs(), envelope };
                self.handle(inb).await;
            }
            // Nobody follows these: the TUI sends one per toggle.
            Request::SetNotifyPrefs(mut prefs) => {
                prefs.updated_at = crate::util::now_secs().max(self.book.prefs.updated_at + 1);
                self.book.prefs = prefs;
                self.save_book();
                self.publish_now = true;
            }
            Request::Viewing(v) => self.alerts.viewing = v,
            Request::SyncNow => {
                self.publish_now = true;
                self.last_publish = None;
                for (_, pk) in self.peers.clone() {
                    self.reply(&pk, Body::SyncNow).await;
                }
                self.outcome(&id, Ok("manual sync: republished state and asked peers to".into()));
            }
            Request::SetAway(on) => {
                let f = away_flag(&self.ctx.sync_dir);
                let _ = if on { std::fs::write(&f, b"1") } else { std::fs::remove_file(&f) };
                self.publish_now = true;
                let msg = if on { "away mode on: questions route to peers" } else { "away mode off" };
                self.outcome(&id, Ok(msg.into()));
            }
            Request::SetPrivate { target, on } => {
                let dir = self.ctx.sync_dir.clone();
                let res = Fleet::load(&dir).ok_or_else(|| "no fleet.toml".to_string()).and_then(|mut f| {
                    f.set_private(&target, on).and_then(|msg| f.save(&dir).map(|_| msg).map_err(|e| e.to_string()))
                });
                if res.is_ok() {
                    self.fleet_mtime = None;
                    self.reload_fleet().await;
                    if on && matches!(target, fleet::PrivateTarget::Folder(_)) {
                        self.folders_after_private().await;
                    }
                    self.publish_now = true;
                }
                self.outcome(&id, res.map_err(|e| format!("private: {e}")));
            }
            Request::To { device, body } => match self.peer_key(&device) {
                Some(pk) => {
                    let env = Envelope::with_id(&me, &id, body);
                    match self.t.send(&pk, &env).await {
                        Ok(n) => self.note(format!("sent {id} to {device} via {n} relay(s)")),
                        Err(e) => self.outcome(&id, Err(format!("send to {device} failed: {e}"))),
                    }
                }
                None => self.outcome(&id, Err(format!("unknown device {device}"))),
            },
            Request::Handoff { session_id, cwd, device, continue_with, place, force } => {
                let Some(pk) = self.peer_key(&device) else {
                    return self.outcome(&id, Err(format!("unknown device {device}")));
                };
                let asks = HandoffOffer { continue_with, place, force, ..Default::default() };
                self.start_handoff(&id, pk, device, session_id, cwd, asks).await;
            }
            Request::Run { mut spec, device } => {
                let Some(pk) = self.peer_key(&device) else {
                    return self.outcome(&id, Err(format!("unknown device {device}")));
                };
                spec.id = id.clone();
                spec.submitted_by = me.clone();
                self.my_runs.insert(id.clone(), 0);
                let env = Envelope::with_id(&me, &id, Body::RunSubmit(spec));
                match self.t.send(&pk, &env).await {
                    Ok(_) => self.note(format!("sent run {id} to {device}")),
                    Err(e) => {
                        self.my_runs.remove(&id);
                        self.outcome(&id, Err(format!("send to {device} failed: {e}")));
                    }
                }
            }
            Request::Xfer { device, offer, pull } => {
                let Some(pk) = self.peer_key(&device) else {
                    return self.outcome(&id, Err(format!("unknown device {device}")));
                };
                if pull {
                    if let Err(e) = self.ctx.fleet.place(&offer.place, &Fleet::file(&self.ctx.sync_dir)) {
                        return self.outcome(&id, Err(e));
                    }
                    self.pending_gets.insert(id.clone(), pk);
                    let env = Envelope::with_id(&me, &id, Body::XferGet(offer));
                    match self.t.send(&pk, &env).await {
                        Ok(_) => self.note(format!("asked {device} to send {id}")),
                        Err(e) => {
                            self.pending_gets.remove(&id);
                            let msg = format!("relay failed: send to {device} failed: {e}");
                            self.outcome_as(&id, Outcome::RelayFailed, &msg);
                        }
                    }
                } else if let Err(e) = self.start_xfer(pk, &device, &id, offer).await {
                    self.outcome(&id, Err(e));
                }
            }
        }
    }

    /// Start a session here for `from`, off the loop: tmux and the trust
    /// dialog take seconds. Starting is not idempotent, so a stored
    /// request older than 5 minutes is refused, as typing is.
    fn start_session(&mut self, from: PublicKey, id: String, age_secs: u64, spec: StartSpec) {
        let done = self.done.clone();
        if age_secs > SEND_MAX_AGE_SECS {
            let _ =
                done.send(Done::Started(from, id, Err("start request is older than 5 minutes; not started".into())));
            return;
        }
        let (fleet, sync_dir) = (self.ctx.fleet.clone(), self.ctx.sync_dir.clone());
        let config = self.ctx.paths.config_dir().to_path_buf();
        let (fail_done, fail_id) = (done.clone(), id.clone());
        if let Err(e) = spawn_named("start", move || {
            let res = super::handoff::start(&spec, &fleet, &sync_dir, &config);
            let _ = done.send(Done::Started(from, id, res));
        }) {
            let _ = fail_done.send(Done::Started(from, fail_id, Err(e)));
        }
    }
}

/// Stop a session running here. The pid is re-validated right before the
/// signal, so a recycled pid is never hit.
fn stop_session(snap: &DiscoverySnapshot) -> Result<String, String> {
    let id = snap.id.as_str();
    match snap.kind {
        SessionKind::Terminal => {
            crate::detect::claude::kill_by_session_id(id).map(|pid| format!("sent SIGTERM (pid {pid})"))
        }
        SessionKind::Background => crate::detect::claude::stop_background_job(id, &snap.claude_root),
        SessionKind::Desktop => Err("a desktop session: close it in the Claude app".into()),
    }
}

/// Type `text` into a session running here. The pane must still belong
/// to a live claude process right before typing: a recycled tty would
/// type into a shell.
fn type_into(snap: &DiscoverySnapshot, text: &str) -> Result<String, String> {
    let target = inject::resolve(snap, &inject::tmux_panes()).ok_or("session has no pane/tab to type into")?;
    let pid = snap.pid.ok_or("session has no process")?;
    let alive = ProcessTable::refreshed()
        .find_by_pid(pid)
        .is_some_and(|p| crate::detect::process::comm_name(&p.comm) == "claude");
    if !alive {
        return Err("session process is gone".into());
    }
    inject::type_into(&target, text)?;
    Ok(format!("{target:?}"))
}

/// What `fetch` asks of a session running here: (title, text).
fn fetch(snap: &DiscoverySnapshot, what: FetchWhat) -> Result<(String, Vec<u8>), String> {
    let name = snap.name.as_str();
    match what {
        FetchWhat::Diff => {
            let run = |args: &[&str]| {
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&snap.cwd)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default()
            };
            let mut out = format!("# {name} — {}\n\n", snap.cwd.display());
            out.push_str(&run(&["status", "--short", "--branch"]));
            out.push('\n');
            out.push_str(&run(&["diff", "HEAD", "--stat"]));
            out.push('\n');
            let mut diff = run(&["diff", "HEAD"]);
            if diff.len() > DIFF_MAX {
                let mut cut = DIFF_MAX;
                while !diff.is_char_boundary(cut) {
                    cut -= 1;
                }
                diff.truncate(cut);
                diff.push_str("\n… diff truncated at 512 KiB\n");
            }
            out.push_str(&diff);
            Ok((format!("diff: {name}"), out.into_bytes()))
        }
        FetchWhat::Log => {
            let path = snap.jsonl_path.as_deref().ok_or("no transcript")?;
            Ok((format!("log: {name}"), tail_lines(path, LOG_TAIL).join("\n").into_bytes()))
        }
        FetchWhat::Subagents => {
            let path = snap.jsonl_path.as_deref().ok_or("no transcript")?;
            Ok((format!("subagents: {name}"), subagent_report(path).into_bytes()))
        }
        FetchWhat::Reply => {
            let path = snap.jsonl_path.as_deref().ok_or("no transcript")?;
            let last = jsonl::read_tail_entries(path, LOG_TAIL).into_iter().rev().find_map(|e| match e {
                jsonl::LogEntry::AssistantText(t) => Some(t.as_str().to_string()),
                _ => None,
            });
            Ok((format!("reply: {name}"), last.ok_or("no reply yet")?.into_bytes()))
        }
        FetchWhat::Transcript => {
            let path = snap.jsonl_path.as_deref().ok_or("no transcript")?;
            Ok((format!("transcript: {name}"), transcript_json(path)))
        }
    }
}

/// The newest transcript lines, each whole up to `ENTRY_CHARS`, as many
/// as fit in `TRANSCRIPT_BYTES` (a few messages), as a JSON list.
/// Successful tool results and finished turns stay out: no one shows them.
fn transcript_json(path: &Path) -> Vec<u8> {
    let entries = jsonl::read_tail_entries(path, TRANSCRIPT_ENTRIES);
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0;
    for e in entries.iter().rev() {
        if matches!(
            e,
            jsonl::LogEntry::ToolResult { is_error: false, .. } | jsonl::LogEntry::Result { is_error: false }
        ) {
            continue;
        }
        let line: String = e.to_string().chars().take(ENTRY_CHARS).collect();
        used += line.len();
        if used > TRANSCRIPT_BYTES && !lines.is_empty() {
            break;
        }
        lines.push(line);
    }
    lines.reverse();
    serde_json::to_vec(&lines).unwrap_or_default()
}

fn fleet_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(Fleet::file(dir)).and_then(|m| m.modified()).ok()
}

fn subagent_report(transcript: &Path) -> String {
    let mut out = String::new();
    for f in jsonl::subagent_transcripts(transcript) {
        let desc = jsonl::subagent_description(&f);
        out.push_str(&format!("## {} {desc}\n", f.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default()));
        for l in tail_lines(&f, SUBAGENT_TAIL) {
            out.push_str(&l);
            out.push('\n');
        }
        out.push('\n');
    }
    if out.is_empty() {
        "no subagents".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_agent_holds_the_lock_until_it_goes() {
        let dir = tempfile::tempdir().unwrap();
        let sync = dir.path().join("sync");
        let held = take_lock(&sync).expect("the first agent takes it");
        match take_lock(&sync) {
            Err(LockError::AlreadyHeld { pid }) => assert_eq!(pid, Some(std::process::id())),
            other => panic!("a second agent must not take it: {other:?}"),
        }
        drop(held);
        take_lock(&sync).expect("free once the first is gone");
    }

    #[test]
    fn reconnects_back_off_doubling_up_to_a_cap() {
        let secs: Vec<u64> = (0..8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 64, 120, 120]);
        assert_eq!(backoff(u32::MAX), RETRY_MAX, "no overflow however long it fails");
    }

    #[test]
    fn followers_keep_up_after_the_log_is_full() {
        let mut v = SyncView::default();
        for i in 0..LOG_LINES + 50 {
            v.note(format!("line {i}"));
        }
        let mut seen = 0;
        assert_eq!(v.lines_since(seen).count(), LOG_LINES, "a late follower gets what is held");
        seen = v.seq;
        for i in 0..3 {
            v.note(format!("new {i}"));
        }
        let got: Vec<&String> = v.lines_since(seen).collect();
        assert_eq!(got.len(), 3, "the log's length no longer changes, its seq does");
        assert!(got[2].ends_with("new 2"));
        assert_eq!(v.lines_since(v.seq).count(), 0);
    }

    #[test]
    fn peer_text_cannot_escape_or_forge_a_log_line() {
        let mut v = SyncView::default();
        v.note("pi refused ab: \x1b]52;c;Zm9v\x07no\n10:00:00Z pi \u{2713} x1: forged");
        let line = &v.log[0];
        assert!(!line.contains('\x1b') && !line.contains('\x07') && !line.contains('\n'), "{line:?}");
    }
}
