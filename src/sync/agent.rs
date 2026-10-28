//! The sync agent: one per device. Publishes this device's state to every
//! peer, executes commands peers send (type into a session, answer a
//! prompt, hand a session over, run a job) and keeps a view of the
//! peers for the TUI and CLI.
//!
//! Runs on its own thread with a single-threaded tokio runtime. It is a
//! worker in nerve's sense: all I/O happens here, the UI only reads
//! `SyncView` and sends `Request`s.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::PublicKey;
use nostr_sdk::prelude::{ClientNotification, StreamExt};

use super::chunks::{self, FileRef, Store};
use super::fleet::{self, Fleet};
use super::handoff::{self, Pack};
use super::inject;
use super::jobs;
use super::mailbox::Mailbox;
use super::modhub::{HubEvent, ModHub};
use super::pairing;
use super::bulk::{self, End};
use super::places::{self, XferPack};
use super::protocol::{
    self, Blob, Body, DeviceState, Envelope, FetchWhat, HandoffHave, HandoffManifest, HandoffOffer, JobSpec,
    JobStatus, Reassembly, RemoteSession, RunSpec, TransferStats, XferOffer, XferWhat,
    BulkOffer,
};
use super::runs;
use super::transport::{Inbound, RelayHealth, Seen, Transport};
use crate::detect::claude::{discover_sessions_with, ScanCaches};
use crate::detect::process::ProcessTable;
use crate::state::session::DiscoverySnapshot;
use crate::util::sanitize::Sanitised;

/// What a peer looks like from here. Strings in `state` came off the
/// network: sanitise before rendering.
#[derive(Debug, Clone, Default)]
pub(crate) struct PeerView {
    pub(crate) name: String,
    pub(crate) npub: String,
    pub(crate) state: Option<DeviceState>,
    /// When the peer generated the state we hold (its clock).
    pub(crate) state_generated_at: Option<u64>,
    /// When we last heard anything from it (our clock).
    pub(crate) last_heard: Option<u64>,
    pub(crate) jobs: BTreeMap<String, JobStatus>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SyncView {
    pub(crate) me: String,
    pub(crate) npub: String,
    pub(crate) running: bool,
    pub(crate) error: Option<String>,
    pub(crate) relays: Vec<RelayHealth>,
    pub(crate) last_publish: Option<u64>,
    /// Peers the last publish reached (relay accepted).
    pub(crate) last_publish_peers: usize,
    pub(crate) peers: Vec<PeerView>,
    /// Recent activity, newest last.
    pub(crate) log: VecDeque<String>,
    /// Chunked transfers in flight: (title, parts in, parts total).
    pub(crate) incoming: Vec<(String, usize, usize)>,
    /// Fetched artefacts (title, text), newest last.
    pub(crate) blobs: VecDeque<(String, String)>,
    pub(crate) local_jobs: Vec<JobStatus>,
    pub(crate) away: bool,
    /// Hand-offs in either direction: (title, chunks done, chunks total).
    pub(crate) transfers: Vec<(String, usize, usize)>,
    /// Sessions on this device whose nerve mod is connected.
    pub(crate) mod_sessions: usize,
    pub(crate) activity: Option<PathBuf>,
}

/// Activity of the long-lived agent, one line per event, so CLI commands
/// that hand their request to it can follow the outcome.
pub(crate) fn activity_file(sync_dir: &Path) -> PathBuf {
    sync_dir.join("activity.log")
}

impl SyncView {
    fn note(&mut self, msg: impl Into<String>) {
        let line = format!("{} {}", clock(protocol::now_secs()), msg.into());
        crate::log_info!("sync: {line}");
        if let Some(f) = &self.activity {
            use std::io::Write;
            if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
                let _ = writeln!(out, "{line}");
            }
        }
        self.log.push_back(line);
        while self.log.len() > 200 {
            self.log.pop_front();
        }
    }
}

pub(crate) fn clock(secs: u64) -> String {
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

/// Things the UI / CLI ask the agent to do.
#[derive(Debug, Clone)]
pub(crate) enum Request {
    To { device: String, body: Body },
    SyncNow,
    Handoff { session_id: String, cwd: PathBuf, device: String, continue_with: Option<String>, place: Option<String>, force: bool },
    Job { spec: JobSpec, device: Option<String> },
    /// `spec.id` names the run in the activity log.
    Run { spec: RunSpec, device: String },
    /// Files or refs into a place on `device` (`pull`: from it into ours).
    /// `id` names the transfer in the activity log.
    Xfer { id: String, device: String, offer: XferOffer, pull: bool },
    SetAway(bool),
}

/// An outcome line in the activity log: `<clock> <device> ✓|✗ <id>: <msg>`.
/// Parsed by position, so a ✓ inside command output never counts.
pub(crate) fn outcome(line: &str) -> Option<(&str, bool, &str)> {
    let mut it = line.splitn(4, ' ');
    let (_clock, _from, mark, rest) = (it.next()?, it.next()?, it.next()?, it.next()?);
    let ok = match mark {
        "\u{2713}" => true,
        "\u{2717}" => false,
        _ => return None,
    };
    let (id, msg) = rest.split_once(": ").unwrap_or((rest.trim_end_matches(':'), ""));
    Some((id, ok, msg))
}

pub(crate) struct AgentHandle {
    pub(crate) view: Arc<Mutex<SyncView>>,
    tx: Sender<Request>,
}

impl AgentHandle {
    pub(crate) fn request(&self, r: Request) {
        let _ = self.tx.send(r);
    }

    pub(crate) fn snapshot(&self) -> SyncView {
        self.view.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

pub(crate) struct Ctx {
    pub(crate) sync_dir: PathBuf,
    pub(crate) fleet: Fleet,
    pub(crate) statusline_dir: Option<PathBuf>,
    /// The TUI or `nerve sync agent`, as opposed to a one-shot CLI
    /// command. Only these answer hooks and take spooled requests.
    pub(crate) long_lived: bool,
}

pub(crate) fn away_flag(sync_dir: &Path) -> PathBuf {
    sync_dir.join("away")
}

pub(crate) fn heartbeat_file(sync_dir: &Path) -> PathBuf {
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
    let view = Arc::new(Mutex::new(SyncView { me: ctx.fleet.device.clone(), ..Default::default() }));
    let (tx, rx) = channel();
    let v = view.clone();
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
        rt.block_on(run(ctx, v, rx));
    })?;
    Ok(AgentHandle { view, tx })
}

struct Agent {
    ctx: Ctx,
    t: Transport,
    view: Arc<Mutex<SyncView>>,
    peers: Vec<(String, PublicKey)>,
    mailbox: Mailbox,
    seen: Seen,
    caches: ScanCaches,
    table: ProcessTable,
    caps: Vec<String>,
    last_state: Option<String>,
    last_publish: Option<Instant>,
    publish_now: bool,
    snapshots: Vec<DiscoverySnapshot>,
    reassembly: HashMap<String, Reassembly>,
    job_tx: Sender<JobStatus>,
    job_rx: Receiver<JobStatus>,
    jobs: BTreeMap<String, (JobStatus, Option<PublicKey>)>,
    handoff_tx: Sender<String>,
    handoff_rx: Receiver<String>,
    store: Arc<Store>,
    /// Hand-offs this device is sending, by offer id.
    outgoing: HashMap<String, Outgoing>,
    /// Hand-offs this device is receiving, by offer id.
    incoming: HashMap<String, Incoming>,
    pack_tx: Sender<Packed>,
    pack_rx: Receiver<Packed>,
    /// Bulk copies this device ran as the sender: (offer id, outcome).
    bulk_tx: Sender<(String, Result<String, String>)>,
    bulk_rx: Receiver<(String, Result<String, String>)>,
    hub: Option<Arc<ModHub>>,
    /// Prompts handed to a mod, waiting for its ack: command id → sender.
    mod_cmds: HashMap<String, PublicKey>,
    open_sub: Option<nostr_sdk::prelude::SubscriptionId>,
    fleet_mtime: Option<std::time::SystemTime>,
    /// Pulls this device asked for: get id → the peer asked.
    pending_gets: HashMap<String, PublicKey>,
    /// Runs this device submitted: id → output lines already logged.
    my_runs: HashMap<String, u64>,
}

/// A finished pack from the packing thread: (offer id, its files + index).
type Packed = (String, Result<(Vec<FileRef>, FileRef), String>);

#[derive(Clone)]
enum OutJob {
    Handoff { session_id: String, cwd: PathBuf },
    Xfer { offer: XferOffer, base: PathBuf },
}

struct Outgoing {
    to: PublicKey,
    device: String,
    label: String,
    job: OutJob,
    started: Instant,
    /// Every chunk id the pack (and its index) uses.
    ids: Vec<String>,
    stats: TransferStats,
    /// Relay events this transfer published, deleted once it is over.
    events: Vec<nostr_sdk::prelude::EventId>,
}

impl Outgoing {
    fn job_for_req(&self) -> Option<String> {
        match &self.job {
            OutJob::Xfer { offer, .. } => offer.for_req.clone(),
            OutJob::Handoff { .. } => None,
        }
    }
}

/// A running rsync's progress line. It names the pull it serves too,
/// since a follower of the pull watches for that id.
fn beat(view: &Arc<Mutex<SyncView>>, re: &str, for_req: Option<&str>, line: String) {
    if let Ok(mut v) = view.lock() {
        v.note(match for_req {
            Some(g) => format!("[{re} for {g}] {line}"),
            None => format!("[{re}] {line}"),
        });
    }
}

enum InJob {
    Handoff(HandoffOffer),
    Xfer(XferOffer),
}

impl InJob {
    fn label(&self) -> String {
        match self {
            Self::Handoff(o) => o.name.clone(),
            Self::Xfer(o) => format!("place {}", o.place),
        }
    }

    fn for_req(&self) -> Option<String> {
        match self {
            Self::Handoff(_) => None,
            Self::Xfer(o) => o.for_req.clone(),
        }
    }
}

#[derive(Clone)]
enum Parsed {
    Handoff(Pack),
    Xfer(XferPack),
}

impl Parsed {
    fn files(&self) -> Vec<FileRef> {
        match self {
            Self::Handoff(p) => p.files.clone(),
            Self::Xfer(p) => p.file_refs(),
        }
    }
}

struct Incoming {
    from: PublicKey,
    job: InJob,
    dest: PathBuf,
    index: Option<FileRef>,
    pack: Option<Parsed>,
    /// The sender said it has pushed everything it thinks we lack.
    pushed: bool,
    asks: u32,
    started: Instant,
    unpacking: bool,
    /// Chunks (and their bytes) that came over the wire for this transfer.
    got_chunks: usize,
    got_bytes: u64,
}

impl Incoming {
    fn new(from: PublicKey, job: InJob, dest: PathBuf) -> Self {
        Self { from, job, dest, index: None, pack: None, pushed: false, asks: 0, started: Instant::now(), unpacking: false, got_chunks: 0, got_bytes: 0 }
    }
}

const HEARTBEAT: Duration = Duration::from_secs(60);
const MIN_GAP: Duration = Duration::from_secs(2);
/// Instructions older than this are refused rather than typed.
const SEND_MAX_AGE_SECS: u64 = 300;

async fn run(ctx: Ctx, view: Arc<Mutex<SyncView>>, rx: Receiver<Request>) {
    let keys = match fleet::device_keys(&ctx.sync_dir) {
        Ok(k) => k,
        Err(e) => return fail(&view, format!("device key: {e}")),
    };
    let t = match Transport::connect(keys, &ctx.fleet.relays).await {
        Ok(t) => t,
        Err(e) => return fail(&view, e),
    };
    let peers = ctx.fleet.peer_keys();
    let mut authors: Vec<PublicKey> = peers.iter().map(|(_, k)| *k).collect();
    authors.push(t.keys.public_key());
    let since_file = ctx.sync_dir.join("since");
    let since = std::fs::read_to_string(&since_file)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| protocol::now_secs().saturating_sub(protocol::MSG_TTL_SECS))
        .saturating_sub(120);
    if let Err(e) = t.subscribe(&authors, since).await {
        return fail(&view, format!("subscribe: {e}"));
    }
    if let Ok(mut v) = view.lock() {
        v.running = true;
        v.npub = fleet::npub(&t.keys.public_key());
        v.peers = peers
            .iter()
            .map(|(n, k)| PeerView { name: n.clone(), npub: fleet::npub(k), ..Default::default() })
            .collect();
        v.away = away_flag(&ctx.sync_dir).exists();
        if ctx.long_lived {
            let f = activity_file(&ctx.sync_dir);
            // Keep it short: only the tail matters to anyone reading it.
            if std::fs::metadata(&f).is_ok_and(|m| m.len() > 1024 * 1024) {
                let _ = std::fs::remove_file(&f);
            }
            v.activity = Some(f);
        }
        v.note(format!("agent up on {} relay(s), {} peer(s)", ctx.fleet.relays.len(), peers.len()));
    }
    let (job_tx, job_rx) = channel();
    let (handoff_tx, handoff_rx) = channel();
    let (pack_tx, pack_rx) = channel();
    let (bulk_tx, bulk_rx) = channel();
    // Sessions running the nerve mod connect here (long-lived agents only:
    // a one-shot CLI must not take the socket from the real agent).
    let hub = if ctx.long_lived {
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
    let mut a = Agent {
        caps: fleet::detect_caps(&ctx.fleet.caps),
        mailbox: Mailbox::new(&ctx.sync_dir),
        seen: Seen::load(ctx.sync_dir.join("seen")),
        caches: ScanCaches::new(ctx.statusline_dir.clone()),
        table: ProcessTable::refreshed(),
        ctx,
        t,
        view,
        peers,
        last_state: None,
        last_publish: None,
        publish_now: true,
        snapshots: Vec::new(),
        reassembly: HashMap::new(),
        job_tx,
        job_rx,
        jobs: BTreeMap::new(),
        handoff_tx,
        handoff_rx,
        store: Arc::new(Store::new(&ctx_sync_dir)),
        outgoing: HashMap::new(),
        incoming: HashMap::new(),
        pack_tx,
        pack_rx,
        bulk_tx,
        bulk_rx,
        hub,
        mod_cmds: HashMap::new(),
        open_sub: None,
        fleet_mtime: None,
        pending_gets: HashMap::new(),
        my_runs: HashMap::new(),
    };
    let mut notifications = a.t.client.notifications();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut last_health = Instant::now() - Duration::from_secs(60);
    let mut last_fleet = Instant::now();
    let mut last_prune = Instant::now();
    a.fleet_mtime = fleet_mtime(&a.ctx.sync_dir);
    loop {
        tokio::select! {
            n = notifications.next() => {
                let Some(ClientNotification::Event { event, .. }) = n else {
                    if n.is_none() { break; }
                    continue;
                };
                if let Some(inb) = a.t.open(&event) {
                    a.handle(inb).await;
                    let _ = std::fs::write(&since_file, protocol::now_secs().to_string());
                }
            }
            _ = tick.tick() => {
                if a.ctx.long_lived {
                    let _ = std::fs::write(heartbeat_file(&a.ctx.sync_dir), std::process::id().to_string());
                    for r in super::spool::drain(&a.ctx.sync_dir) {
                        a.request(r).await;
                    }
                }
                loop {
                    match rx.try_recv() {
                        Ok(r) => a.request(r).await,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            a.t.disconnect().await;
                            return;
                        }
                    }
                }
                a.drain_jobs().await;
                a.drain_handoffs().await;
                a.drain_packs().await;
                a.drain_bulk().await;
                a.drain_hub().await;
                if a.ctx.long_lived && last_prune.elapsed() >= Duration::from_secs(3600) {
                    last_prune = Instant::now();
                    let store = a.store.clone();
                    tokio::task::spawn_blocking(move || store.prune());
                }
                if last_fleet.elapsed() >= Duration::from_secs(5) {
                    last_fleet = Instant::now();
                    a.reload_fleet().await;
                    a.sync_invite_subscription().await;
                }
                if last_scan.elapsed() >= Duration::from_secs(3) {
                    last_scan = Instant::now();
                    a.scan();
                }
                if last_health.elapsed() >= Duration::from_secs(10) {
                    last_health = Instant::now();
                    let h = a.t.health().await;
                    if let Ok(mut v) = a.view.lock() { v.relays = h; }
                }
                a.maybe_publish().await;
            }
        }
    }
}

fn fail(view: &Arc<Mutex<SyncView>>, e: String) {
    crate::log_warn!("sync: {e}");
    if let Ok(mut v) = view.lock() {
        v.running = false;
        v.error = Some(e);
    }
}

impl Agent {
    fn note(&self, msg: impl Into<String>) {
        if let Ok(mut v) = self.view.lock() {
            v.note(msg);
        }
    }

    fn peer_name(&self, pk: &PublicKey) -> Option<String> {
        if *pk == self.t.keys.public_key() {
            return Some(self.ctx.fleet.device.clone());
        }
        self.peers.iter().find(|(_, k)| k == pk).map(|(n, _)| n.clone())
    }

    fn peer_key(&self, name: &str) -> Option<PublicKey> {
        if name == self.ctx.fleet.device {
            return Some(self.t.keys.public_key());
        }
        self.peers.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }

    fn scan(&mut self) {
        self.table.refresh_if_stale(Duration::from_secs(5));
        self.snapshots = discover_sessions_with(&self.table, &mut self.caches).sessions;
        let away = away_flag(&self.ctx.sync_dir).exists();
        if let Ok(mut v) = self.view.lock() {
            v.away = away;
            v.local_jobs = self.jobs.values().map(|(j, _)| j.clone()).collect();
        }
    }

    fn build_state(&self) -> DeviceState {
        let panes = inject::tmux_panes();
        let mods = self.hub.as_ref().map(|h| h.sessions()).unwrap_or_default();
        let mut ordered: Vec<&DiscoverySnapshot> = self.snapshots.iter().collect();
        // Stable order across publishes, so remote views don't reshuffle.
        ordered.sort_by(|a, b| a.cwd.cmp(&b.cwd).then_with(|| a.id.as_str().cmp(b.id.as_str())));
        let sessions = ordered
            .into_iter()
            .map(|s| RemoteSession {
                id: s.id.as_str().to_string(),
                name: s.name.as_str().to_string(),
                cwd: s.cwd.to_string_lossy().into_owned(),
                branch: s.branch.as_ref().map(|b| b.as_str().to_string()),
                state: s.detected_state.label(),
                waiting_for: s.waiting_for.as_ref().map(|w| w.as_str().to_string()),
                kind: format!("{:?}", s.kind).to_lowercase(),
                tool: s.current_tool.as_ref().map(|t| t.as_str().to_string()),
                cost_usd: crate::state::session::effective_cost(s.official.as_ref(), &s.usage),
                active_age_secs: s.activity_age_secs,
                injectable: mods.contains_key(s.id.as_str())
                    || (s.kind == crate::state::session::SessionKind::Terminal
                        && (s.tty.as_ref().is_some_and(|t| inject::pane_for_tty(&panes, t.as_str()).is_some())
                            || cfg!(target_os = "macos"))),
                via_mod: mods.contains_key(s.id.as_str()),
                last_turn: mods.get(s.id.as_str()).and_then(|(_, t)| t.clone()),
                subagents: s.jsonl_path.as_deref().map(count_subagents).unwrap_or(0),
                tail: s.jsonl_path.as_deref().map(|p| tail_lines(p, protocol::TAIL_LINES)).unwrap_or_default(),
            })
            .collect();
        DeviceState {
            device: self.ctx.fleet.device.clone(),
            host: fleet::hostname(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            caps: self.caps.clone(),
            nerve_version: env!("CARGO_PKG_VERSION").into(),
            generated_at: protocol::now_secs(),
            away: away_flag(&self.ctx.sync_dir).exists(),
            sessions,
            requests: {
                let mut r = self.mailbox.pending();
                if let Some(h) = &self.hub {
                    r.extend(h.pending());
                }
                r
            },
            jobs: self.jobs.values().map(|(j, _)| j.clone()).collect(),
        }
    }

    async fn maybe_publish(&mut self) {
        if self.peers.is_empty() {
            return;
        }
        if self.last_publish.is_some_and(|t| t.elapsed() < MIN_GAP) && !self.publish_now {
            return;
        }
        let state = self.build_state();
        let mut fingerprint = state.clone();
        fingerprint.generated_at = 0;
        let fp = serde_json::to_string(&fingerprint).unwrap_or_default();
        let changed = self.last_state.as_deref() != Some(fp.as_str());
        let due = self.last_publish.is_none_or(|t| t.elapsed() >= HEARTBEAT);
        if !(changed || due || self.publish_now) {
            return;
        }
        self.publish_now = false;
        self.last_publish = Some(Instant::now());
        let env = Envelope::new(&self.ctx.fleet.device, Body::State(Box::new(state)));
        let mut reached = 0;
        for (name, pk) in self.peers.clone() {
            match self.t.put_state(&pk, &env).await {
                Ok(_) => reached += 1,
                Err(e) => self.note(format!("publish to {name} failed: {e}")),
            }
        }
        self.last_state = Some(fp);
        if let Ok(mut v) = self.view.lock() {
            v.last_publish = Some(protocol::now_secs());
            v.last_publish_peers = reached;
        }
    }

    async fn reply(&self, to: &PublicKey, body: Body) {
        let env = Envelope::new(&self.ctx.fleet.device, body);
        if let Err(e) = self.t.send(to, &env).await {
            self.note(format!("send failed: {e}"));
        }
    }

    async fn ack(&self, to: &PublicKey, re: &str, ok: bool, msg: impl Into<String>) {
        let msg = msg.into();
        self.note(format!("{} {re}: {msg}", if ok { "ok" } else { "refused" }));
        self.reply(to, Body::Ack { re: re.to_string(), ok, msg }).await;
    }

    async fn handle(&mut self, inb: Inbound) {
        let Some(from) = self.peer_name(&inb.sender) else {
            // Not a fleet member. The one thing a stranger may say is
            // "here is the invite secret you handed out".
            if let Body::PairRequest { secret, name, relays } = &inb.envelope.body {
                if self.ctx.long_lived && self.seen.first(&inb.envelope.id) {
                    self.on_pair_request(&inb, secret, name, relays).await;
                }
            }
            return;
        };
        let id = inb.envelope.id.clone();
        // A one-shot CLI agent only reads; work addressed to this device
        // waits (stored) for the long-lived agent, unmarked as seen.
        let is_work = matches!(
            inb.envelope.body,
            Body::Send { .. }
                | Body::Answer { .. }
                | Body::Fetch { .. }
                | Body::HandoffOffer(_)
                | Body::HandoffManifest(_)
                | Body::ChunkData { .. }
                | Body::JobSubmit(_)
                | Body::RunSubmit(_)
                | Body::XferOffer(_)
                | Body::XferGet(_)
                | Body::PairRequest { .. }
        );
        // Except the replies to a pull this very agent asked for.
        let asked_for = match &inb.envelope.body {
            Body::XferOffer(o) => o.for_req.as_ref().is_some_and(|r| self.pending_gets.get(r) == Some(&inb.sender)),
            Body::HandoffManifest(m) => self.incoming.contains_key(&m.re),
            Body::ChunkData { re, .. } => self.incoming.contains_key(re),
            _ => false,
        };
        if is_work && !asked_for && !self.ctx.long_lived {
            return;
        }
        if !self.seen.first(&id) {
            return;
        }
        let now = protocol::now_secs();
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
                            p.state = Some(*state);
                        }
                    }
                }
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
                // The mod submits it inside Claude (queued if a turn is
                // running); its ack comes back through the hub.
                if let Some(hub) = self.hub.as_ref().filter(|h| h.connected(&session)) {
                    if hub.submit(&session, &id, &text) {
                        self.mod_cmds.insert(id.clone(), inb.sender);
                        return;
                    }
                }
                let res = self.inject(&session, &text);
                match res {
                    Ok(how) => self.ack(&inb.sender, &id, true, format!("typed into {how}")).await,
                    Err(e) => self.ack(&inb.sender, &id, false, e).await,
                }
                self.publish_now = true;
            }
            Body::Answer { request, answer } => {
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
                let res = self.fetch(&session, what);
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
            Body::Blob(b) => self.on_blob(&inb.sender, b).await,
            Body::HandoffOffer(offer) => self.on_offer(&inb.sender, &from, id.clone(), offer).await,
            Body::HandoffHave(have) => self.on_have(&inb.sender, have),
            Body::HandoffManifest(m) => self.on_manifest(&inb.sender, m).await,
            Body::ChunkWant { re, ids } => self.on_chunk_want(&inb.sender, &re, ids).await,
            Body::ChunkData { re, chunks, last } => self.on_chunk_data(&inb.sender, &re, chunks, last).await,
            // Already paired (the joiner re-ran setup): just confirm.
            Body::PairRequest { .. } => self.ack(&inb.sender, &id, true, format!("{} already trusts '{from}'", self.ctx.fleet.device)).await,
            Body::JobSubmit(spec) => self.accept_job(&inb.sender, &id, spec).await,
            Body::RunSubmit(spec) => {
                let age = now.saturating_sub(inb.envelope.sent_at.min(inb.created_at));
                self.accept_run(&inb.sender, &id, age, spec).await;
            }
            Body::XferOffer(offer) => self.on_xfer_offer(&inb.sender, &from, id.clone(), offer).await,
            Body::XferGet(offer) => self.on_xfer_get(inb.sender, &from, &id, offer).await,
            Body::JobUpdate(st) => {
                if let Ok(mut v) = self.view.lock() {
                    if let Some(p) = v.peers.iter_mut().find(|p| p.name == from) {
                        p.jobs.insert(st.id.clone(), st.clone());
                    }
                    if st.kind != "run" && (st.phase == "done" || st.phase == "failed") {
                        v.note(format!("job '{}' on {from}: {}", st.title, st.phase));
                    }
                }
                self.on_run_update(&from, &st);
            }
            Body::Ack { re, ok, msg } => {
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
                }
                self.note(format!("{from} {} {re}: {msg}", if ok { "✓" } else { "✗" }));
                // The sender ended a pull we asked for (a bulk copy it ran,
                // or giving up): report it under the pull's id, which is
                // what the CLI follows.
                if let Some(get) = self.incoming.remove(&re).and_then(|i| i.job.for_req()) {
                    self.pending_gets.remove(&get);
                    self.note(format!("{from} {} {get}: {msg}", if ok { "✓" } else { "✗" }));
                    self.refresh_transfers();
                }
                self.pending_gets.remove(&re);
            }
        }
    }

    fn local_session(&self, id: &str) -> Option<&DiscoverySnapshot> {
        self.snapshots.iter().find(|s| s.id.as_str() == id || s.id.as_str().starts_with(id))
    }

    fn inject(&self, session: &str, text: &str) -> Result<String, String> {
        let snap = self.local_session(session).ok_or("no such session on this device")?;
        let target = inject::resolve(snap, &inject::tmux_panes()).ok_or("session has no pane/tab to type into")?;
        // Re-validate right before typing: the pane must still belong to
        // a live claude process (a recycled tty would type into a shell).
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

    fn fetch(&self, session: &str, what: FetchWhat) -> Result<(String, Vec<u8>), String> {
        let snap = self.local_session(session).ok_or("no such session on this device")?;
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
                if diff.len() > 512 * 1024 {
                    diff.truncate(512 * 1024);
                    diff.push_str("\n… diff truncated at 512 KiB\n");
                }
                out.push_str(&diff);
                Ok((format!("diff: {name}"), out.into_bytes()))
            }
            FetchWhat::Log => {
                let path = snap.jsonl_path.as_deref().ok_or("no transcript")?;
                Ok((format!("log: {name}"), tail_lines(path, 200).join("\n").into_bytes()))
            }
            FetchWhat::Subagents => {
                let path = snap.jsonl_path.as_deref().ok_or("no transcript")?;
                Ok((format!("subagents: {name}"), subagent_report(path).into_bytes()))
            }
        }
    }

    async fn on_blob(&mut self, from: &PublicKey, b: Blob) {
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
        let Some(bytes) = done else {
            let inflight: Vec<(String, usize, usize)> =
                self.reassembly.values().map(|r| { let (a, b) = r.progress(); (r.title.clone(), a, b) }).collect();
            if let Ok(mut v) = self.view.lock() {
                v.incoming = inflight;
            }
            return;
        };
        if let Ok(mut v) = self.view.lock() {
            v.incoming.retain(|(t, _, _)| *t != b.title);
        }
        let title = self.reassembly.remove(&key).map(|r| r.title).unwrap_or_default();
        let _ = from;
        let text = Sanitised::new(String::from_utf8_lossy(&bytes).into_owned()).as_str().to_string();
        self.note(format!("received {title} ({} bytes)", bytes.len()));
        if let Ok(mut v) = self.view.lock() {
            v.blobs.push_back((title, text));
            while v.blobs.len() > 10 {
                v.blobs.pop_front();
            }
        }
    }

    // ---- hand-off: offer → have → manifest → want → data → ack ----

    /// Source, step 1: tell the target what's coming and which repo it is.
    /// `asks` carries what the user chose: `continue_with`, `place`, `force`.
    async fn start_handoff(&mut self, pk: PublicKey, device: String, session_id: String, cwd: PathBuf, asks: HandoffOffer) {
        let m = match tokio::task::spawn_blocking({
            let cwd = cwd.clone();
            move || handoff::describe(&cwd)
        })
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => return self.note(format!("handoff: {e}")),
            Err(e) => return self.note(format!("handoff: {e}")),
        };
        let offer = HandoffOffer {
            session: session_id.clone(),
            name: m.repo_name.clone(),
            src_cwd: m.src_cwd.clone(),
            repo_name: m.repo_name,
            repo_id: m.repo_id,
            branch: m.branch,
            ..asks
        };
        let env = Envelope::new(&self.ctx.fleet.device, Body::HandoffOffer(offer));
        let offer_id = env.id.clone();
        if let Err(e) = self.t.send(&pk, &env).await {
            return self.note(format!("handoff offer failed: {e}"));
        }
        self.note(format!("offered '{session_id:.8}' to {device}; waiting for it to say what it has"));
        let label = format!("handoff '{session_id:.8}'");
        self.outgoing.insert(
            offer_id,
            Outgoing { to: pk, device, label, job: OutJob::Handoff { session_id, cwd }, started: Instant::now(), ids: Vec::new(), stats: TransferStats::default(), events: Vec::new() },
        );
    }

    /// Target, step 2: report the commits our copy of the repo has.
    async fn on_offer(&mut self, from: &PublicKey, name: &str, offer_id: String, offer: HandoffOffer) {
        let dest = match handoff::destination(&offer, &self.ctx.fleet, &Fleet::file(&self.ctx.sync_dir)) {
            Ok(d) => d,
            Err(e) => return self.ack(from, &offer_id, false, e).await,
        };
        let (d, rid) = (dest.clone(), offer.repo_id.clone());
        let tips = tokio::task::spawn_blocking(move || handoff::local_tips(&d, &rid)).await.unwrap_or_default();
        self.note(format!(
            "{name} is handing over '{}' → {} ({})",
            offer.name,
            dest.display(),
            if tips.is_empty() { "new checkout".to_string() } else { format!("have {} commit tip(s)", tips.len()) }
        ));
        self.incoming.insert(offer_id.clone(), Incoming::new(*from, InJob::Handoff(offer), dest));
        self.reply(from, Body::HandoffHave(HandoffHave { re: offer_id, tips, dest: None })).await;
    }

    /// Source, step 3: pack against what the target has, off the loop.
    fn on_have(&mut self, from: &PublicKey, have: HandoffHave) {
        let Some(o) = self.outgoing.get(&have.re).filter(|o| o.to == *from) else { return };
        if let OutJob::Xfer { offer: XferOffer { bulk: Some(scan), what: XferWhat::Files { roots, delete }, place, .. }, base } = &o.job {
            // The recipient checked the place and gave its path; we copy.
            let res = match (&have.dest, self.ctx.fleet.ssh.get(&o.device)) {
                (Some(dest), Some(host)) => Ok((dest.clone(), host.clone())),
                (None, _) => Err(format!("{} gave no path for a bulk copy", o.device)),
                (_, None) => Err(format!("no [ssh] route to {}", o.device)),
            };
            let (tx, view, re) = (self.bulk_tx.clone(), self.view.clone(), have.re.clone());
            let (scan, roots, delete, place, base) = (scan.clone(), roots.clone(), *delete, place.clone(), base.clone());
            let for_req = o.job_for_req();
            std::thread::spawn(move || {
                let res = res.and_then(|(dest, host)| {
                    let dst = End::Remote { host, path: dest.into() };
                    bulk::run(&End::Local(base), &dst, &roots, &scan, delete, &|l| beat(&view, &re, for_req.as_deref(), l))
                });
                let _ = tx.send((re, res.map(|m| format!("{place}: {m}"))));
            });
            return;
        }
        let (job, store, tx, re) = (o.job.clone(), self.store.clone(), self.pack_tx.clone(), have.re.clone());
        std::thread::spawn(move || {
            let index = |json: Vec<u8>| store.add_file("index", &json).map_err(|e| e.to_string());
            let res = match job {
                OutJob::Handoff { session_id, cwd } => {
                    let roots = crate::detect::claude_roots();
                    handoff::pack(&cwd, &session_id, &roots, &have.tips, &store)
                        .and_then(|p| Ok((p.files.clone(), index(serde_json::to_vec(&p).map_err(|e| e.to_string())?)?)))
                }
                OutJob::Xfer { offer, base } => {
                    let pack = match &offer.what {
                        XferWhat::Files { roots, delete } => places::pack_files(&base, roots, *delete, &store),
                        XferWhat::Refs { map, .. } => places::pack_refs(&base, map, &have.tips, &store),
                    };
                    pack.and_then(|p| Ok((p.file_refs(), index(serde_json::to_vec(&p).map_err(|e| e.to_string())?)?)))
                }
            };
            let _ = tx.send((re, res));
        });
    }

    /// Source, step 4: send the index and push the chunks the target is
    /// not known to have.
    async fn drain_packs(&mut self) {
        while let Ok((re, res)) = self.pack_rx.try_recv() {
            let Some(o) = self.outgoing.get_mut(&re) else { continue };
            let (files, index) = match res {
                Ok(x) => x,
                Err(e) => {
                    let to = o.to;
                    self.outgoing.remove(&re);
                    self.note(format!("{} \u{2717} {re}: could not pack: {e}", self.ctx.fleet.device));
                    self.reply(&to, Body::Ack { re, ok: false, msg: format!("sender could not pack: {e}") }).await;
                    continue;
                }
            };
            o.stats.files = files.len();
            let mut all: Vec<FileRef> = files;
            all.push(index.clone());
            o.ids = chunks::needed(&all, &Default::default());
            // The index travels too, so it counts towards the total.
            o.stats.total_bytes = all.iter().map(|f| f.size).sum();
            o.stats.chunks = o.ids.len();
            let to = o.to;
            let known = self.store.known(&to.to_hex());
            let push = chunks::needed(&all, &known);
            let env = Envelope::new(&self.ctx.fleet.device, Body::HandoffManifest(HandoffManifest { re: re.clone(), index }));
            match self.t.send_id(&to, &env).await {
                Ok((_, ev)) => {
                    if let Some(o) = self.outgoing.get_mut(&re) {
                        o.events.push(ev);
                    }
                    self.send_chunks(&re, push, true).await;
                }
                Err(e) => self.relay_failed(&re, e).await,
            }
        }
    }

    fn track(&mut self, re: &str, ev: nostr_sdk::prelude::EventId) {
        if let Some(o) = self.outgoing.get_mut(re) {
            o.events.push(ev);
        }
    }

    /// Chunks for `re`, packed up to `CHUNK_BYTES` raw per message, over
    /// the bulk lane. `last` marks the end of a push.
    async fn send_chunks(&mut self, re: &str, ids: Vec<String>, last: bool) {
        let Some(to) = self.outgoing.get(re).map(|o| o.to) else { return };
        let mut batch: Vec<(String, String)> = Vec::new();
        let mut size = 0usize;
        let (mut sent_bytes, mut sent_chunks) = (0u64, 0usize);
        let mut missing = Vec::new();
        for id in &ids {
            let Some(bytes) = self.store.get(id) else {
                missing.push(id.clone());
                continue;
            };
            if size + bytes.len() > protocol::CHUNK_BYTES && !batch.is_empty() {
                let env = Envelope::new(&self.ctx.fleet.device, Body::ChunkData { re: re.to_string(), chunks: std::mem::take(&mut batch), last: false });
                match self.t.send_bulk(&to, &env).await {
                    Ok((_, ev)) => self.track(re, ev),
                    Err(e) => return self.relay_failed(re, format!("handoff chunks failed: {e}")).await,
                }
                size = 0;
            }
            size += bytes.len();
            sent_bytes += bytes.len() as u64;
            sent_chunks += 1;
            batch.push((id.clone(), protocol::base64_encode(&bytes)));
        }
        if !missing.is_empty() {
            self.note(format!("{} chunk(s) were pruned here; re-packing is needed", missing.len()));
        }
        if !batch.is_empty() || last {
            let env = Envelope::new(&self.ctx.fleet.device, Body::ChunkData { re: re.to_string(), chunks: batch, last });
            match self.t.send_bulk(&to, &env).await {
                Ok((_, ev)) => self.track(re, ev),
                Err(e) => return self.relay_failed(re, format!("handoff chunks failed: {e}")).await,
            }
        }
        let line = self.outgoing.get_mut(re).map(|o| {
            o.stats.sent_bytes += sent_bytes;
            o.stats.sent_chunks += sent_chunks;
            let st = o.stats;
            format!(
                "{} → {}: sent {} of {} ({} of {} chunks) in {:.1}s",
                o.label,
                o.device,
                human(st.sent_bytes),
                human(st.total_bytes),
                st.sent_chunks,
                st.chunks,
                o.started.elapsed().as_secs_f64()
            )
        });
        if let (Some(l), true) = (line, last) {
            self.note(l);
        }
        self.refresh_transfers();
    }

    async fn on_chunk_want(&mut self, from: &PublicKey, re: &str, ids: Vec<String>) {
        let Some(o) = self.outgoing.get(re).filter(|o| o.to == *from) else { return };
        // Only chunks of this hand-off: a peer can't read the store at large.
        let allowed: std::collections::HashSet<&String> = o.ids.iter().collect();
        let ids: Vec<String> = ids.into_iter().filter(|i| allowed.contains(i)).take(4096).collect();
        let _ = self.store.forget_known(&from.to_hex(), &ids);
        self.send_chunks(re, ids, true).await;
    }

    /// Target: the index of what's coming.
    async fn on_manifest(&mut self, from: &PublicKey, m: HandoffManifest) {
        let Some(inc) = self.incoming.get_mut(&m.re).filter(|i| i.from == *from) else { return };
        inc.index = Some(m.index);
        self.progress_incoming(&m.re).await;
    }

    async fn on_chunk_data(&mut self, from: &PublicKey, re: &str, data: Vec<(String, String)>, last: bool) {
        let Some(inc) = self.incoming.get_mut(re).filter(|i| i.from == *from) else { return };
        if last {
            inc.pushed = true;
        }
        inc.got_chunks += data.len();
        for (id, b64) in data {
            match protocol::base64_decode(&b64) {
                Ok(bytes) => {
                    if let Some(i) = self.incoming.get_mut(re) {
                        i.got_bytes += bytes.len() as u64;
                    }
                    if let Err(e) = self.store.put_verified(&id, &bytes) {
                        self.note(format!("dropped a chunk: {e}"));
                    }
                }
                Err(_) => self.note("dropped a chunk: bad base64"),
            }
        }
        self.progress_incoming(re).await;
    }

    /// Target: once the index and then every chunk are here, unpack.
    /// After the sender's push ends, ask for whatever is still missing.
    async fn progress_incoming(&mut self, re: &str) {
        let store = self.store.clone();
        let Some(inc) = self.incoming.get_mut(re) else { return };
        if inc.unpacking {
            return;
        }
        let Some(index) = inc.index.clone() else { return };
        if inc.pack.is_none() {
            match store.assemble(&index) {
                Ok(bytes) => {
                    let parsed = match &inc.job {
                        InJob::Handoff(_) => serde_json::from_slice::<Pack>(&bytes).map(Parsed::Handoff),
                        InJob::Xfer(_) => serde_json::from_slice::<XferPack>(&bytes).map(Parsed::Xfer),
                    };
                    match parsed {
                        Ok(p) => inc.pack = Some(p),
                        Err(e) => {
                            let (from, why) = (inc.from, format!("bad transfer index: {e}"));
                            self.incoming.remove(re);
                            return self.ack(&from, re, false, why).await;
                        }
                    }
                }
                Err(missing) => {
                    if inc.pushed {
                        self.want(re, missing).await;
                    }
                    return;
                }
            }
        }
        let Some(pack) = inc.pack.clone() else { return };
        let files = pack.files();
        let missing = chunks::needed(&files, &Default::default()).into_iter().filter(|c| !store.has(c)).collect::<Vec<_>>();
        if !missing.is_empty() {
            if inc.pushed {
                self.want(re, missing).await;
            }
            self.refresh_transfers();
            return;
        }
        inc.unpacking = true;
        let (from, dest, started) = (inc.from, inc.dest.clone(), inc.started);
        // Everything is here: the sender may assume we hold these from now on.
        let mut ids = chunks::needed(&files, &Default::default());
        ids.extend(index.chunks.iter().cloned());
        let _ = store.mark_known(&from.to_hex(), ids.clone());
        let total = {
            let mut u: std::collections::HashSet<&String> = ids.iter().collect();
            u.extend(index.chunks.iter());
            u.len()
        };
        let moved = format!(
            "received {} in {} of {} chunks ({} reused)",
            human(inc.got_bytes),
            inc.got_chunks.min(total),
            total,
            total.saturating_sub(inc.got_chunks)
        );
        let for_req = inc.job.for_req();
        let job = match &inc.job {
            InJob::Handoff(o) => InJob::Handoff(o.clone()),
            InJob::Xfer(o) => InJob::Xfer(o.clone()),
        };
        let fleet = self.ctx.fleet.clone();
        let sync_dir = self.ctx.sync_dir.clone();
        let tx = self.handoff_tx.clone();
        let claude_root = crate::detect::claude_roots().into_iter().next().unwrap_or_else(|| PathBuf::from(".claude"));
        let me = self.ctx.fleet.device.clone();
        let re = re.to_string();
        std::thread::spawn(move || {
            let msg = match (job, pack) {
                (InJob::Handoff(offer), Parsed::Handoff(pack)) => (|| -> Result<String, String> {
                    let inst = handoff::unpack(&pack, &dest, &store, &claude_root, offer.force)?;
                    let settings = sync_dir.join("claude-settings.json");
                    let tmux = handoff::resume_in_tmux(&inst, &claude_root, &settings)?;
                    if fleet.auto_trust_handoffs {
                        handoff::accept_trust_dialog(&tmux);
                    }
                    if let Some(text) = offer.continue_with.as_deref().filter(|t| !t.trim().is_empty()) {
                        // Give the TUI time to load the conversation first.
                        std::thread::sleep(Duration::from_secs(4));
                        let pane = inject::InputTarget::TmuxPane(format!("{tmux}:"));
                        inject::type_into(&pane, text)?;
                    }
                    Ok(format!("{me} resumed '{}' in tmux session {tmux} at {} ({:.1}s)", offer.name, inst.cwd.display(), started.elapsed().as_secs_f64()))
                })()
                .map_err(|e| format!("handoff failed: {e}")),
                (InJob::Xfer(offer), Parsed::Xfer(pack)) => {
                    let done = match (&offer.what, &pack) {
                        (XferWhat::Files { .. }, XferPack::Files { .. }) => places::apply_files(&pack, &dest, &store)
                            .map(|a| format!("{} file(s) written, {} unchanged, {} deleted", a.written, a.unchanged, a.deleted)),
                        (XferWhat::Refs { .. }, XferPack::Refs { .. }) => places::apply_refs(&pack, &dest, &store).map(|l| l.join("; ")),
                        _ => Err("transfer index does not match its offer".into()),
                    };
                    done.map(|d| format!("{me} place {}: {d}; {moved} in {:.1}s", offer.place, started.elapsed().as_secs_f64()))
                        .map_err(|e| format!("{me} place {}: {e}", offer.place))
                }
                _ => Err("transfer index does not match its offer".into()),
            };
            let (ok, text) = match msg {
                Ok(t) => (true, t),
                Err(e) => (false, e),
            };
            let _ = tx.send(serde_json::json!({"to": from.to_hex(), "re": re, "ok": ok, "msg": text, "for_req": for_req}).to_string());
        });
        self.refresh_transfers();
    }

    async fn want(&mut self, re: &str, missing: Vec<String>) {
        let Some(inc) = self.incoming.get_mut(re) else { return };
        inc.pushed = false;
        inc.asks += 1;
        if inc.asks > 5 {
            let from = inc.from;
            self.incoming.remove(re);
            return self.ack(&from, re, false, "handoff gave up: chunks kept going missing").await;
        }
        let from = inc.from;
        self.note(format!("asking for {} missing chunk(s)", missing.len()));
        self.reply(&from, Body::ChunkWant { re: re.to_string(), ids: missing }).await;
    }

    fn refresh_transfers(&self) {
        let mut t: Vec<(String, usize, usize)> = Vec::new();
        for o in self.outgoing.values() {
            t.push((format!("→ {} {}", o.device, o.label), o.stats.sent_chunks, o.stats.chunks));
        }
        for i in self.incoming.values() {
            let total = i.pack.as_ref().map(|p| chunks::needed(&p.files(), &Default::default())).unwrap_or_default();
            let have = total.iter().filter(|c| self.store.has(c)).count();
            t.push((format!("← {}", i.job.label()), have, total.len()));
        }
        if let Ok(mut v) = self.view.lock() {
            v.transfers = t;
        }
    }

    // ---- pairing ----

    async fn on_pair_request(&mut self, inb: &Inbound, secret: &str, name: &str, relays: &[String]) {
        if !pairing::consume(&self.ctx.sync_dir, secret) {
            self.note("refused a pairing request: invite unknown, used or expired");
            // Tell the joiner rather than leave it waiting. Nothing about
            // this device is revealed beyond what the invite already held.
            let id = inb.envelope.id.clone();
            self.ack(&inb.sender, &id, false, "that invite was already used or has expired; ask for a new one with `nerve sync invite`").await;
            return;
        }
        let mut fleet = Fleet::load(&self.ctx.sync_dir).unwrap_or_else(|| self.ctx.fleet.clone());
        let existing = fleet.peer_keys().into_iter().find(|(_, k)| *k == inb.sender).map(|(n, _)| n);
        let me = fleet.device.clone();
        let name = existing.unwrap_or_else(|| {
            let f = fleet.clone();
            pairing::unique_name(name, |n| f.peers.contains_key(n) || n == me)
        });
        fleet.peers.insert(name.clone(), fleet::npub(&inb.sender));
        // A private relay the joiner knows and we don't is worth sharing.
        for r in relays.iter().filter(|r| r.starts_with("ws://") || r.starts_with("wss://")).take(8) {
            if !fleet.relays.contains(r) && r.starts_with("ws://") {
                fleet.relays.push(r.clone());
            }
        }
        if let Err(e) = fleet.save(&self.ctx.sync_dir) {
            return self.note(format!("pairing: could not save fleet.toml: {e}"));
        }
        self.note(format!("paired with {name} (invite accepted)"));
        let id = inb.envelope.id.clone();
        self.reload_fleet().await;
        self.ack(&inb.sender, &id, true, format!("{me} now trusts '{name}'")).await;
    }

    /// Pick up fleet.toml edits (pairing, `nerve sync relay`) without a restart.
    async fn reload_fleet(&mut self) {
        let m = fleet_mtime(&self.ctx.sync_dir);
        if m == self.fleet_mtime {
            return;
        }
        self.fleet_mtime = m;
        let Some(f) = Fleet::load(&self.ctx.sync_dir) else { return };
        let peers = f.peer_keys();
        let added: Vec<String> = peers.iter().filter(|(_, k)| !self.peers.iter().any(|(_, o)| o == k)).map(|(n, _)| n.clone()).collect();
        for r in f.relays.iter().filter(|r| !self.ctx.fleet.relays.contains(r)) {
            let _ = self.t.client.add_relay(r.as_str()).await;
            let _ = self.t.client.connect_relay(r.as_str()).await;
        }
        self.ctx.fleet = f;
        if !added.is_empty() || peers.len() != self.peers.len() {
            self.peers = peers;
            let mut authors: Vec<PublicKey> = self.peers.iter().map(|(_, k)| *k).collect();
            authors.push(self.t.keys.public_key());
            let since = protocol::now_secs().saturating_sub(protocol::MSG_TTL_SECS);
            if let Err(e) = self.t.subscribe(&authors, since).await {
                self.note(format!("resubscribe failed: {e}"));
            }
            if let Ok(mut v) = self.view.lock() {
                for (n, k) in &self.peers {
                    if !v.peers.iter().any(|p| p.name == *n) {
                        v.peers.push(PeerView { name: n.clone(), npub: fleet::npub(k), ..Default::default() });
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
        if !self.ctx.long_lived {
            return;
        }
        let open = pairing::any_open(&self.ctx.sync_dir);
        match (&self.open_sub, open) {
            (None, true) => match self.t.subscribe_open(protocol::now_secs().saturating_sub(60)).await {
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
        let n = hub.sessions().len();
        let pending: usize = hub.pending().len() + self.mailbox.pending().len();
        let peers: Vec<String> = self.view.lock().map(|v| {
            v.peers.iter().map(|p| match p.state_generated_at {
                Some(g) => format!("{} {}", p.name, short_age(protocol::now_secs().saturating_sub(g))),
                None => format!("{} –", p.name),
            }).collect()
        }).unwrap_or_default();
        let mut line = format!("⇄ nerve · {}", if peers.is_empty() { "no peers".to_string() } else { peers.join(" ") });
        if pending > 0 {
            line.push_str(&format!(" · {pending} waiting on another device"));
        }
        hub.set_status(line);
        if let Ok(mut v) = self.view.lock() {
            v.mod_sessions = n;
        }
    }

    async fn drain_handoffs(&mut self) {
        while let Ok(raw) = self.handoff_rx.try_recv() {
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let (Some(to), Some(re)) = (v["to"].as_str(), v["re"].as_str()) else { continue };
            let Ok(pk) = PublicKey::parse(to) else { continue };
            let ok = v["ok"].as_bool().unwrap_or(false);
            let msg = v["msg"].as_str().unwrap_or("").to_string();
            self.incoming.remove(re);
            self.refresh_transfers();
            self.publish_now = true;
            self.ack(&pk, re, ok, msg.clone()).await;
            // A pull this device asked for: its CLI follows the get's id.
            if let Some(get) = v["for_req"].as_str() {
                self.pending_gets.remove(get);
                let from = self.peer_name(&pk).unwrap_or_default();
                self.note(format!("{from} {} {get}: {msg}", if ok { "\u{2713}" } else { "\u{2717}" }));
            }
        }
    }

    // ---- places: files and refs ----

    /// Source: offer files or refs from one of our places to `pk`.
    async fn start_xfer(&mut self, pk: PublicKey, device: &str, id: &str, mut offer: XferOffer) -> Result<(), String> {
        let base = self.ctx.fleet.place(&offer.place, &Fleet::file(&self.ctx.sync_dir))?;
        let label = match &mut offer.what {
            XferWhat::Files { roots, delete } => {
                for r in roots.iter() {
                    let rel = places::norm_root(r)?;
                    let p = if rel.is_empty() { base.clone() } else { base.join(&rel) };
                    if std::fs::symlink_metadata(&p).is_err() {
                        return Err(format!("{r} does not exist in place {} ({})", offer.place, base.display()));
                    }
                }
                let (b, r) = (base.clone(), roots.clone());
                let scan = tokio::task::spawn_blocking(move || bulk::scan(&b, &r)).await.map_err(|e| e.to_string())??;
                let over = scan.bytes >= self.ctx.fleet.xfer.bulk_over;
                if over {
                    let src = match bulk::route(scan.bytes, self.ctx.fleet.xfer.bulk_over, self.ctx.fleet.ssh.contains_key(device)) {
                        bulk::Route::RsyncThere => Some(base.display().to_string()),
                        _ => None,
                    };
                    offer.bulk = Some(BulkOffer { src, ..scan.clone() });
                }
                format!(
                    "files {} in {}{}{}",
                    roots.join(" "),
                    offer.place,
                    if *delete { " (--delete)" } else { "" },
                    if over { format!(" ({} over rsync)", bulk::human(scan.bytes)) } else { String::new() }
                )
            }
            XferWhat::Refs { map, repo_id } => {
                let b = base.clone();
                let repo = tokio::task::spawn_blocking(move || places::place_repo(&b).map(|r| (places::this_repo_id(&r), r)))
                    .await
                    .map_err(|e| e.to_string())??;
                *repo_id = repo.0;
                let pairs: Vec<String> = map.iter().map(|m| format!("{}:{}", m.src, m.dst)).collect();
                format!("refs {} in {}", pairs.join(" "), offer.place)
            }
        };
        // The id in the label puts it on every progress line the CLI follows.
        let label = format!("{label} [{id}]");
        let env = Envelope::with_id(&self.ctx.fleet.device, id, Body::XferOffer(offer.clone()));
        self.outgoing.insert(
            id.to_string(),
            Outgoing {
                to: pk,
                device: device.to_string(),
                label: label.clone(),
                job: OutJob::Xfer { offer, base },
                started: Instant::now(),
                ids: Vec::new(),
                stats: TransferStats::default(),
                events: Vec::new(),
            },
        );
        match self.t.send_id(&pk, &env).await {
            Ok((_, ev)) => {
                self.note(format!("offered {label} to {device}"));
                if let Some(o) = self.outgoing.get_mut(id) {
                    o.events.push(ev);
                }
            }
            Err(e) => self.relay_failed(id, e).await,
        }
        self.refresh_transfers();
        Ok(())
    }

    /// The relay could not carry a transfer of files: copy them over
    /// rsync when this device has a route, asking the far side's nerve
    /// over ssh for its path, since the relay can't. Without a route the
    /// transfer fails, marked so a follower with one can retry it.
    async fn relay_failed(&mut self, re: &str, e: String) {
        let me = self.ctx.fleet.device.clone();
        let Some(o) = self.outgoing.get(re) else { return self.note(format!("relay failed: {e}")) };
        let host = self.ctx.fleet.ssh.get(&o.device).cloned();
        let (OutJob::Xfer { offer, base }, Some(host)) = (o.job.clone(), host) else {
            let (to, for_req) = (o.to, match &o.job {
                OutJob::Xfer { offer, .. } => offer.for_req.clone(),
                OutJob::Handoff { .. } => None,
            });
            self.outgoing.remove(re);
            self.refresh_transfers();
            self.note(format!("{me} \u{2717} {re}: relay failed: {e}"));
            // Best effort: the relay may still carry a short message.
            if for_req.is_some() {
                let env = Envelope::new(&me, Body::Ack { re: re.to_string(), ok: false, msg: format!("relay failed: {e}") });
                let _ = self.t.send(&to, &env).await;
            }
            return;
        };
        let XferWhat::Files { roots, delete } = offer.what.clone() else {
            self.outgoing.remove(re);
            return self.note(format!("{me} \u{2717} {re}: relay failed: {e}"));
        };
        self.note(format!("{me}: relay failed for {re} ({e}); copying over rsync to {host}"));
        let (tx, view, re, place) = (self.bulk_tx.clone(), self.view.clone(), re.to_string(), offer.place.clone());
        std::thread::spawn(move || {
            let res = (|| {
                let scan = bulk::scan(&base, &roots)?;
                let far = bulk::remote_place(&host, &place, false, &roots)?;
                let dst = End::Remote { host: host.clone(), path: far.path.into() };
                bulk::run(&End::Local(base), &dst, &roots, &scan, delete, &|l| beat(&view, &re, None, l))
            })();
            let _ = tx.send((re, res.map(|m| format!("{place}: {m}"))));
        });
    }

    /// Sender of a bulk copy it ran: report it here, tell the recipient
    /// (whose pull, if it asked, ends on that), and free the relay.
    async fn drain_bulk(&mut self) {
        while let Ok((re, res)) = self.bulk_rx.try_recv() {
            let Some(o) = self.outgoing.remove(&re) else { continue };
            let me = self.ctx.fleet.device.clone();
            let (ok, msg) = match res {
                Ok(m) => (true, format!("{} place {m}", o.device)),
                Err(e) => (false, e),
            };
            self.note(format!("{me} {} {re}: {msg}", if ok { "\u{2713}" } else { "\u{2717}" }));
            self.reply(&o.to, Body::Ack { re: re.clone(), ok, msg }).await;
            self.forget_events(o.events).await;
            self.refresh_transfers();
        }
    }

    async fn forget_events(&self, ids: Vec<nostr_sdk::prelude::EventId>) {
        if ids.is_empty() {
            return;
        }
        if let Err(e) = self.t.delete(&ids).await {
            self.note(format!("could not free {} relay event(s): {e}", ids.len()));
        }
    }

    /// Target: check the place and what it may receive, then report what
    /// it already has (commit tips, for refs) so the sender packs less.
    async fn on_xfer_offer(&mut self, from: &PublicKey, name: &str, offer_id: String, offer: XferOffer) {
        let fleet = self.ctx.fleet.clone();
        let file = Fleet::file(&self.ctx.sync_dir);
        let o = offer.clone();
        let sender = name.to_string();
        let checked = tokio::task::spawn_blocking(move || -> Result<(PathBuf, Vec<String>), String> {
            let dest = fleet.place(&o.place, &file)?;
            match &o.what {
                XferWhat::Files { roots, .. } => {
                    for r in roots {
                        let rel = places::norm_root(r)?;
                        // rsync writes as it goes: check every path first.
                        if o.bulk.is_some() {
                            places::ready_for(&dest, &rel)?;
                        }
                    }
                    if o.bulk.as_ref().is_some_and(|b| b.src.is_some()) && !fleet.ssh.contains_key(&sender) {
                        return Err(format!(
                            "{sender} has no ssh route to {me} for files this big, and {me} has none to {sender}: add `{sender} = \"<ssh host>\"` under [ssh] in {} (or `{me} = …` in {sender}'s fleet.toml)",
                            file.display(),
                            me = fleet.device
                        ));
                    }
                    Ok((dest, Vec::new()))
                }
                XferWhat::Refs { map, repo_id } => {
                    let repo = places::place_repo(&dest)?;
                    let mine = places::this_repo_id(&repo);
                    if !repo_id.is_empty() && !mine.is_empty() && *repo_id != mine {
                        return Err(format!("place '{}' on {} holds a different repository", o.place, fleet.device));
                    }
                    for m in map {
                        places::check_dst(&repo, &m.dst)?;
                    }
                    Ok((dest, places::repo_tips(&repo)))
                }
            }
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let (dest, tips) = match checked {
            Ok(x) => x,
            Err(e) => {
                if let Some(get) = &offer.for_req {
                    self.pending_gets.remove(get);
                    self.note(format!("{} \u{2717} {get}: {e}", self.ctx.fleet.device));
                }
                return self.ack(from, &offer_id, false, e).await;
            }
        };
        self.note(format!("{name} is sending into place {} → {}", offer.place, dest.display()));
        let bulk = offer.bulk.clone();
        let (place, what, for_req) = (offer.place.clone(), offer.what.clone(), offer.for_req.clone());
        self.incoming.insert(offer_id.clone(), Incoming::new(*from, InJob::Xfer(offer), dest.clone()));
        self.refresh_transfers();
        match (bulk, what) {
            // The sender can't reach us: we pull from the path it gave.
            (Some(scan @ BulkOffer { src: Some(_), .. }), XferWhat::Files { roots, delete }) => {
                let host = self.ctx.fleet.ssh.get(name).cloned().unwrap_or_default();
                let src = End::Remote { host, path: scan.src.clone().unwrap_or_default().into() };
                let (tx, view, me, re, to) = (self.handoff_tx.clone(), self.view.clone(), self.ctx.fleet.device.clone(), offer_id, from.to_hex());
                std::thread::spawn(move || {
                    let res = bulk::run(&src, &End::Local(dest), &roots, &scan, delete, &|l| beat(&view, &re, for_req.as_deref(), l));
                    let (ok, msg) = match res {
                        Ok(m) => (true, format!("{me} place {place}: {m}")),
                        Err(e) => (false, format!("{me} place {place}: {e}")),
                    };
                    let _ = tx.send(serde_json::json!({"to": to, "re": re, "ok": ok, "msg": msg, "for_req": for_req}).to_string());
                });
            }
            (Some(_), _) => {
                let dest = Some(dest.display().to_string());
                self.reply(from, Body::HandoffHave(HandoffHave { re: offer_id, tips, dest })).await;
            }
            (None, _) => self.reply(from, Body::HandoffHave(HandoffHave { re: offer_id, tips, dest: None })).await,
        }
    }

    /// Source of a pull: send it back as an ordinary offer.
    async fn on_xfer_get(&mut self, from: PublicKey, name: &str, get_id: &str, offer: XferOffer) {
        let offer = XferOffer { for_req: Some(get_id.to_string()), ..offer };
        let id = protocol::new_id();
        if let Err(e) = self.start_xfer(from, name, &id, offer).await {
            self.ack(&from, get_id, false, e).await;
        }
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
        let tx = self.job_tx.clone();
        let device = self.ctx.fleet.device.clone();
        self.jobs.insert(spec.id.clone(), (runs::queued(&spec, &device), Some(*from)));
        std::thread::spawn(move || runs::run(&spec, &plan, &device, &tx));
        self.publish_now = true;
    }

    /// A run this device submitted: log its new output lines and, once,
    /// how it ended.
    fn on_run_update(&mut self, from: &str, st: &JobStatus) {
        let Some(seen) = self.my_runs.get_mut(&st.id) else { return };
        let new = st.lines.saturating_sub(*seen) as usize;
        *seen = (*seen).max(st.lines);
        let shown = new.min(st.tail.len());
        let mut lines = Vec::new();
        if new > shown {
            lines.push(format!("{from} \u{2502} {} … {} line(s) not shown", st.id, new - shown));
        }
        for l in &st.tail[st.tail.len() - shown..] {
            lines.push(format!("{from} \u{2502} {} {}", st.id, Sanitised::new(l.clone()).as_str()));
        }
        let finished = st.phase == "done" || st.phase == "failed";
        if finished {
            self.my_runs.remove(&st.id);
            let code = st.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into());
            let why = Sanitised::new(st.result.clone().unwrap_or_default()).as_str().to_string();
            let mark = if st.exit_code == Some(0) { "\u{2713}" } else { "\u{2717}" };
            lines.push(format!("{from} {mark} {}: run finished: exit {code} ({why})", st.id));
        }
        for l in lines {
            self.note(l);
        }
    }

    async fn accept_job(&mut self, from: &PublicKey, id: &str, spec: JobSpec) {
        let missing: Vec<&String> = spec.requires.iter().filter(|r| !self.caps.contains(r)).collect();
        if !missing.is_empty() {
            let m: Vec<String> = missing.into_iter().cloned().collect();
            self.ack(from, id, false, format!("missing capabilities: {}", m.join(", "))).await;
            return;
        }
        self.ack(from, id, true, format!("job '{}' accepted", spec.title)).await;
        self.start_job(spec, Some(*from));
    }

    fn start_job(&mut self, spec: JobSpec, reply_to: Option<PublicKey>) {
        let tx = self.job_tx.clone();
        let device = self.ctx.fleet.device.clone();
        // A job without --cwd gets its own directory: under handoff_root
        // when one is set, else inside nerve's own config.
        let root = self.ctx.fleet.handoff_root.clone().unwrap_or_else(|| self.ctx.sync_dir.join("jobs"));
        let claude_root = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
        let queued = JobStatus {
            id: spec.id.clone(),
            title: spec.title.clone(),
            device: device.clone(),
            phase: "queued".into(),
            updated_at: protocol::now_secs(),
            ..Default::default()
        };
        self.jobs.insert(spec.id.clone(), (queued, reply_to));
        std::thread::spawn(move || jobs::run(spec, &device, &root, claude_root.as_deref(), &tx));
    }

    async fn drain_jobs(&mut self) {
        let mut updates = Vec::new();
        while let Ok(st) = self.job_rx.try_recv() {
            updates.push(st);
        }
        for st in updates {
            let (reply_to, phase_changed) = match self.jobs.get(&st.id) {
                Some((j, r)) => (*r, j.phase != st.phase),
                None => (None, true),
            };
            if st.phase == "done" || st.phase == "failed" {
                let how = st.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default();
                self.note(format!("{} '{}' {}{how}", if st.kind == "run" { "run" } else { "job" }, st.title, st.phase));
            }
            self.jobs.insert(st.id.clone(), (st.clone(), reply_to));
            if let Some(pk) = reply_to {
                if pk != self.t.keys.public_key() {
                    self.reply(&pk, Body::JobUpdate(st)).await;
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
    }

    async fn request(&mut self, r: Request) {
        match r {
            Request::SyncNow => {
                self.publish_now = true;
                self.last_publish = None;
                for (_, pk) in self.peers.clone() {
                    self.reply(&pk, Body::SyncNow).await;
                }
                self.note("manual sync: republished state and asked peers to");
            }
            Request::SetAway(on) => {
                let f = away_flag(&self.ctx.sync_dir);
                let _ = if on { std::fs::write(&f, b"1") } else { std::fs::remove_file(&f) };
                self.publish_now = true;
                self.note(if on { "away mode on: questions route to peers" } else { "away mode off" });
            }
            Request::To { device, body } => match self.peer_key(&device) {
                Some(pk) => {
                    let env = Envelope::new(&self.ctx.fleet.device, body);
                    match self.t.send(&pk, &env).await {
                        Ok(n) => self.note(format!("sent {} to {device} via {n} relay(s)", env.id)),
                        Err(e) => self.note(format!("send to {device} failed: {e}")),
                    }
                }
                None => self.note(format!("unknown device {device}")),
            },
            Request::Handoff { session_id, cwd, device, continue_with, place, force } => {
                let Some(pk) = self.peer_key(&device) else {
                    return self.note(format!("unknown device {device}"));
                };
                let asks = HandoffOffer { continue_with, place, force, ..Default::default() };
                self.start_handoff(pk, device, session_id, cwd, asks).await;
            }
            Request::Run { mut spec, device } => {
                let me = self.ctx.fleet.device.clone();
                let Some(pk) = self.peer_key(&device) else {
                    return self.note(format!("{me} \u{2717} {}: unknown device {device}", spec.id));
                };
                spec.submitted_by = me.clone();
                let id = spec.id.clone();
                self.my_runs.insert(id.clone(), 0);
                let env = Envelope::with_id(&me, &id, Body::RunSubmit(spec));
                match self.t.send(&pk, &env).await {
                    Ok(_) => self.note(format!("sent run {id} to {device}")),
                    Err(e) => {
                        self.my_runs.remove(&id);
                        self.note(format!("{me} \u{2717} {id}: send to {device} failed: {e}"));
                    }
                }
            }
            Request::Xfer { id, device, offer, pull } => {
                let me = self.ctx.fleet.device.clone();
                let Some(pk) = self.peer_key(&device) else {
                    return self.note(format!("{me} \u{2717} {id}: unknown device {device}"));
                };
                if pull {
                    if let Err(e) = self.ctx.fleet.place(&offer.place, &Fleet::file(&self.ctx.sync_dir)) {
                        return self.note(format!("{me} \u{2717} {id}: {e}"));
                    }
                    self.pending_gets.insert(id.clone(), pk);
                    let env = Envelope::with_id(&me, &id, Body::XferGet(offer));
                    match self.t.send(&pk, &env).await {
                        Ok(_) => self.note(format!("asked {device} to send {id}")),
                        Err(e) => {
                            self.pending_gets.remove(&id);
                            self.note(format!("{me} \u{2717} {id}: relay failed: send to {device} failed: {e}"));
                        }
                    }
                } else if let Err(e) = self.start_xfer(pk, &device, &id, offer).await {
                    self.note(format!("{me} \u{2717} {id}: {e}"));
                }
            }
            Request::Job { mut spec, device } => {
                if spec.id.is_empty() {
                    spec.id = protocol::new_id();
                }
                spec.submitted_by = self.ctx.fleet.device.clone();
                let target = device.or_else(|| self.route_job(&spec));
                match target {
                    None => self.note(format!("no device has {:?} for job '{}'", spec.requires, spec.title)),
                    Some(d) if d == self.ctx.fleet.device => {
                        self.note(format!("running job '{}' here", spec.title));
                        self.start_job(spec, None);
                    }
                    Some(d) => {
                        let Some(pk) = self.peer_key(&d) else { return };
                        self.note(format!("job '{}' → {d}", spec.title));
                        let env = Envelope::new(&self.ctx.fleet.device, Body::JobSubmit(spec));
                        if let Err(e) = self.t.send(&pk, &env).await {
                            self.note(format!("job send failed: {e}"));
                        }
                    }
                }
            }
        }
    }

    /// Capability + load routing over this device and every peer whose
    /// state we hold.
    fn route_job(&self, spec: &JobSpec) -> Option<String> {
        let view = self.view.lock().ok()?;
        let busy = |sessions: usize, jobs: usize| sessions + 2 * jobs;
        let mine_running = self.jobs.values().filter(|(j, _)| j.phase == "running").count();
        let mut cands: Vec<(String, Vec<String>, usize)> =
            vec![(self.ctx.fleet.device.clone(), self.caps.clone(), busy(self.snapshots.len(), mine_running))];
        for p in &view.peers {
            if let Some(s) = &p.state {
                let running = s.jobs.iter().filter(|j| j.phase == "running").count();
                cands.push((p.name.clone(), s.caps.clone(), busy(s.sessions.len(), running)));
            }
        }
        protocol::pick_device(cands.iter().map(|(n, c, l)| (n.as_str(), &c[..], *l)), &spec.requires).map(String::from)
    }
}

fn fleet_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(Fleet::file(dir)).and_then(|m| m.modified()).ok()
}

fn human(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.1} KiB", b as f64 / 1024.0),
        _ => format!("{:.1} MiB", b as f64 / 1_048_576.0),
    }
}

fn short_age(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    }
}

fn count_subagents(transcript: &Path) -> u32 {
    std::fs::read_dir(transcript.with_extension("").join("subagents"))
        .map(|rd| rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "jsonl")).count() as u32)
        .unwrap_or(0)
}

/// Transcript tail as display lines (the same parser the preview uses).
pub(crate) fn tail_lines(transcript: &Path, n: usize) -> Vec<String> {
    use crate::state::log_entry::LogEntry;
    crate::detect::jsonl::read_tail_entries(transcript, n)
        .into_iter()
        .map(|e| match e {
            LogEntry::UserText(t) => format!("› {t}"),
            LogEntry::AssistantText(t) => format!("● {t}"),
            LogEntry::ToolUse { name, detail } => format!("▸ {name} {detail}"),
            LogEntry::ToolResult { status, snippet } => format!("  ⎿ {status} {snippet}"),
            LogEntry::Result { is_error } => if is_error { "✗ turn failed".into() } else { "✓ turn done".into() },
        })
        .map(|l| l.chars().take(240).collect())
        .collect()
}

fn subagent_report(transcript: &Path) -> String {
    let dir = transcript.with_extension("").join("subagents");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return "no subagents".into();
    };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
    files.sort();
    let mut out = String::new();
    for f in files {
        let meta = f.with_extension("meta.json");
        let desc = std::fs::read_to_string(&meta)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("description").or(v.get("agentType")).and_then(|d| d.as_str()).map(String::from))
            .unwrap_or_default();
        out.push_str(&format!("## {} {desc}\n", f.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default()));
        for l in tail_lines(&f, 12) {
            out.push_str(&l);
            out.push('\n');
        }
        out.push('\n');
    }
    if out.is_empty() { "no subagents".into() } else { out }
}
