//! Hand-offs and transfers into places, both ends: offer → have →
//! manifest → chunks → ack, and the rsync copy for what the relay can't
//! carry.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::PublicKey;

use super::{Agent, Done, SyncView};
use crate::sync::chunks::{self, FileRef};
use crate::sync::fleet::Fleet;
use crate::sync::folders;
use crate::sync::handoff::{self, Pack};
use crate::sync::inject;
use crate::sync::ipc::{self, Outcome};
use crate::sync::places;
use crate::sync::places::{End, XferPack};
use crate::sync::protocol::{
    self, Body, BulkOffer, Envelope, HandoffHave, HandoffManifest, HandoffOffer, XferOffer, XferWhat,
};
use crate::util::spawn_named;
use crate::util::text::format_bytes;

/// Chunk ids honoured in one `ChunkWant`.
const WANT_MAX: usize = 4096;

/// Times a receiver asks again for missing chunks before it gives up.
const MAX_ASKS: u32 = 5;

/// A transfer into this device that finished unpacking (or failed to).
pub(super) struct Landed {
    /// The sender, to ack.
    pub(super) to: PublicKey,
    /// The offer id.
    pub(super) re: String,
    pub(super) ok: bool,
    pub(super) msg: String,
    /// The pull this answers, when this device asked for it.
    pub(super) for_req: Option<String>,
}

#[derive(Clone)]
pub(super) enum OutJob {
    Handoff { session_id: String, cwd: PathBuf },
    Xfer { offer: XferOffer, base: PathBuf },
}

/// What one hand-off carried, for the log.
#[derive(Clone, Copy, Default)]
pub(super) struct TransferStats {
    pub(super) total_bytes: u64,
    pub(super) sent_bytes: u64,
    pub(super) chunks: usize,
    pub(super) sent_chunks: usize,
}

pub(super) struct Outgoing {
    pub(super) to: PublicKey,
    pub(super) device: String,
    pub(super) label: String,
    pub(super) job: OutJob,
    pub(super) started: Instant,
    /// Every chunk id the pack (and its index) uses.
    pub(super) ids: Vec<String>,
    pub(super) stats: TransferStats,
    /// Relay events this transfer published, deleted once it is over.
    pub(super) events: Vec<nostr_sdk::prelude::EventId>,
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

#[derive(Clone)]
pub(super) enum InJob {
    Handoff(HandoffOffer),
    Xfer(XferOffer),
}

impl InJob {
    fn label(&self) -> String {
        match self {
            Self::Handoff(o) => o.repo_name.clone(),
            Self::Xfer(o) => format!("place {}", o.place),
        }
    }

    pub(super) fn for_req(&self) -> Option<String> {
        match self {
            Self::Handoff(_) => None,
            Self::Xfer(o) => o.for_req.clone(),
        }
    }
}

#[derive(Clone)]
pub(super) enum Parsed {
    Handoff(Pack),
    Xfer(XferPack),
}

impl Parsed {
    pub(super) fn files(&self) -> Vec<FileRef> {
        match self {
            Self::Handoff(p) => p.files.clone(),
            Self::Xfer(p) => p.file_refs(),
        }
    }
}

pub(super) struct Incoming {
    pub(super) from: PublicKey,
    pub(super) job: InJob,
    pub(super) dest: PathBuf,
    pub(super) index: Option<FileRef>,
    pub(super) pack: Option<Parsed>,
    /// The sender said it has pushed everything it thinks we lack.
    pub(super) pushed: bool,
    pub(super) asks: u32,
    pub(super) started: Instant,
    pub(super) unpacking: bool,
    /// Chunks (and their bytes) that came over the wire for this transfer.
    pub(super) got_chunks: usize,
    pub(super) got_bytes: u64,
}

impl Incoming {
    fn new(from: PublicKey, job: InJob, dest: PathBuf) -> Self {
        Self {
            from,
            job,
            dest,
            index: None,
            pack: None,
            pushed: false,
            asks: 0,
            started: Instant::now(),
            unpacking: false,
            got_chunks: 0,
            got_bytes: 0,
        }
    }
}

impl Agent {
    /// Source, step 1: tell the target what's coming and which repo it is.
    /// `asks` carries what the user chose: `continue_with`, `place`, `force`.
    pub(super) async fn start_handoff(
        &mut self,
        id: &str,
        pk: PublicKey,
        device: String,
        session_id: String,
        cwd: PathBuf,
        asks: HandoffOffer,
    ) {
        let m = match tokio::task::spawn_blocking({
            let cwd = cwd.clone();
            move || handoff::describe(&cwd)
        })
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => return self.outcome(id, Err(format!("handoff: {e}"))),
            Err(e) => return self.outcome(id, Err(format!("handoff: {e}"))),
        };
        let offer = HandoffOffer {
            session: session_id.clone(),
            src_cwd: m.src_cwd.clone(),
            repo_name: m.repo_name,
            repo_id: m.repo_id,
            branch: m.branch,
            ..asks
        };
        let env = Envelope::with_id(&self.ctx.fleet.device, id, Body::HandoffOffer(offer));
        if let Err(e) = self.t.send(&pk, &env).await {
            return self.outcome(id, Err(format!("handoff offer failed: {e}")));
        }
        self.note(format!("offered '{session_id:.8}' to {device}; waiting for it to say what it has"));
        let label = format!("handoff '{session_id:.8}'");
        self.outgoing.insert(
            id.to_string(),
            Outgoing {
                to: pk,
                device,
                label,
                job: OutJob::Handoff { session_id, cwd },
                started: Instant::now(),
                ids: Vec::new(),
                stats: TransferStats::default(),
                events: Vec::new(),
            },
        );
    }

    /// Target, step 2: report the commits our copy of the repo has.
    pub(super) async fn on_offer(&mut self, from: &PublicKey, name: &str, offer_id: String, offer: HandoffOffer) {
        let dest = match handoff::destination(&offer, &self.ctx.fleet, &Fleet::file(&self.ctx.sync_dir)) {
            Ok(d) => d,
            Err(e) => return self.ack(from, &offer_id, false, e).await,
        };
        let (d, rid) = (dest.clone(), offer.repo_id.clone());
        let tips = tokio::task::spawn_blocking(move || handoff::local_tips(&d, &rid)).await.unwrap_or_default();
        self.note(format!(
            "{name} is handing over '{}' → {} ({})",
            offer.repo_name,
            dest.display(),
            if tips.is_empty() { "new checkout".to_string() } else { format!("have {} commit tip(s)", tips.len()) }
        ));
        self.incoming.insert(offer_id.clone(), Incoming::new(*from, InJob::Handoff(offer), dest));
        self.reply(from, Body::HandoffHave(HandoffHave { re: offer_id, tips, dest: None, skip: Vec::new() })).await;
    }

    /// Source, step 3: pack against what the target has, off the loop.
    pub(super) fn on_have(&mut self, from: &PublicKey, have: HandoffHave) {
        let Some(o) = self.outgoing.get(&have.re).filter(|o| o.to == *from) else { return };
        if let OutJob::Xfer {
            offer: XferOffer { bulk: Some(scan), what: XferWhat::Files { roots, delete }, place, .. },
            base,
        } = &o.job
        {
            // The recipient checked the place and gave its path; we copy.
            let res = match (&have.dest, self.ctx.fleet.ssh.get(&o.device)) {
                (Some(dest), Some(host)) => Ok((dest.clone(), host.clone())),
                (None, _) => Err(format!("{} gave no path for a bulk copy", o.device)),
                (_, None) => Err(format!("no [ssh] route to {}", o.device)),
            };
            let (done, view, re) = (self.done.clone(), self.view.clone(), have.re.clone());
            let (scan, roots, delete, place, base) =
                (scan.clone(), roots.clone(), *delete, place.clone(), base.clone());
            let for_req = o.job_for_req();
            let (fail_done, fail_re) = (done.clone(), re.clone());
            if let Err(e) = spawn_named("bulk-copy", move || {
                let res = res.and_then(|(dest, host)| {
                    let dst = End::Remote { host, path: dest.into() };
                    places::copy(&End::Local(base), &dst, &roots, &scan, delete, &|l| {
                        beat(&view, &re, for_req.as_deref(), l)
                    })
                });
                let _ = done.send(Done::Bulk(re, res.map(|m| format!("{place}: {m}"))));
            }) {
                let _ = fail_done.send(Done::Bulk(fail_re, Err(e)));
            }
            return;
        }
        let mut job = o.job.clone();
        // A synced folder's target already has these as offered.
        if let OutJob::Xfer { offer: XferOffer { what: XferWhat::Files { roots, .. }, folder: Some(_), .. }, .. } =
            &mut job
        {
            roots.retain(|r| !have.skip.contains(r));
            let n = have.skip.len();
            self.folder_skipped(&have.re, n);
        }
        let (store, done, re) = (self.store.clone(), self.done.clone(), have.re.clone());
        let (fail_done, fail_re) = (done.clone(), re.clone());
        if let Err(e) = spawn_named("pack", move || {
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
            let _ = done.send(Done::Packed(re, res));
        }) {
            let _ = fail_done.send(Done::Packed(fail_re, Err(e)));
        }
    }

    /// Source, step 4: send the index and push the chunks the target is
    /// not known to have.
    pub(super) async fn on_packed(&mut self, re: String, res: Result<(Vec<FileRef>, FileRef), String>) {
        let Some(o) = self.outgoing.get_mut(&re) else { return };
        let (files, index) = match res {
            Ok(x) => x,
            Err(e) => {
                let to = o.to;
                self.outgoing.remove(&re);
                self.outcome(&re, Err(format!("could not pack: {e}")));
                let msg = format!("sender could not pack: {e}");
                self.reply(&to, Body::Ack { re, ok: false, msg, relay: false }).await;
                return;
            }
        };
        let mut all: Vec<FileRef> = files;
        all.push(index.clone());
        o.ids = chunks::needed(&all, &Default::default());
        // The index travels too, so it counts towards the total.
        o.stats.total_bytes = all.iter().map(|f| f.size).sum();
        o.stats.chunks = o.ids.len();
        let to = o.to;
        let known = self.store.known(&to.to_hex());
        let push = chunks::needed(&all, &known);
        let env =
            Envelope::new(&self.ctx.fleet.device, Body::HandoffManifest(HandoffManifest { re: re.clone(), index }));
        match self.t.send_id(&to, &env).await {
            Ok((_, ev)) => {
                self.track(&re, ev);
                self.send_chunks(&re, push, true).await;
            }
            Err(e) => self.relay_failed(&re, e).await,
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
                let env = Envelope::new(
                    &self.ctx.fleet.device,
                    Body::ChunkData { re: re.to_string(), chunks: std::mem::take(&mut batch), last: false },
                );
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
            let env =
                Envelope::new(&self.ctx.fleet.device, Body::ChunkData { re: re.to_string(), chunks: batch, last });
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
                format_bytes(st.sent_bytes),
                format_bytes(st.total_bytes),
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

    pub(super) async fn on_chunk_want(&mut self, from: &PublicKey, re: &str, ids: Vec<String>) {
        let Some(o) = self.outgoing.get(re).filter(|o| o.to == *from) else { return };
        // Only chunks of this hand-off: a peer can't read the store at large.
        let allowed: std::collections::HashSet<&String> = o.ids.iter().collect();
        let ids: Vec<String> = ids.into_iter().filter(|i| allowed.contains(i)).take(WANT_MAX).collect();
        let _ = self.store.forget_known(&from.to_hex(), &ids);
        self.send_chunks(re, ids, true).await;
    }

    /// Target: the index of what's coming.
    pub(super) async fn on_manifest(&mut self, from: &PublicKey, m: HandoffManifest) {
        let Some(inc) = self.incoming.get_mut(&m.re).filter(|i| i.from == *from) else { return };
        inc.index = Some(m.index);
        self.progress_incoming(&m.re).await;
    }

    pub(super) async fn on_chunk_data(&mut self, from: &PublicKey, re: &str, data: Vec<(String, String)>, last: bool) {
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
                            let _ = self.folder_landed(re, false);
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
        let mut ids = chunks::needed(&files, &Default::default());
        let missing = ids.iter().filter(|c| !store.has(c)).cloned().collect::<Vec<_>>();
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
        ids.extend(index.chunks.iter().cloned());
        let total = ids.iter().collect::<HashSet<_>>().len();
        let _ = store.mark_known(&from.to_hex(), ids);
        let moved = format!(
            "received {} in {} of {} chunks ({} reused)",
            format_bytes(inc.got_bytes),
            inc.got_chunks.min(total),
            total,
            total.saturating_sub(inc.got_chunks)
        );
        let for_req = inc.job.for_req();
        let job = inc.job.clone();
        let fleet = self.ctx.fleet.clone();
        let sync_dir = self.ctx.sync_dir.clone();
        let done = self.done.clone();
        let me = self.ctx.fleet.device.clone();
        let re = re.to_string();
        let (fail_done, fail_re, fail_for_req) = (done.clone(), re.clone(), for_req.clone());
        if let Err(e) = spawn_named("unpack", move || {
            let msg = match (job, pack) {
                (InJob::Handoff(offer), Parsed::Handoff(pack)) => (|| -> Result<String, String> {
                    let claude_root = handoff::claude_root()?;
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
                    Ok(format!(
                        "{me} resumed '{}' in tmux session {tmux} at {} ({:.1}s)",
                        offer.repo_name,
                        inst.cwd.display(),
                        started.elapsed().as_secs_f64()
                    ))
                })()
                .map_err(|e| format!("handoff failed: {e}")),
                (InJob::Xfer(offer), Parsed::Xfer(pack)) => {
                    let done = match (&offer.what, &pack) {
                        (XferWhat::Files { .. }, XferPack::Files { .. }) => places::apply_files(&pack, &dest, &store)
                            .map(|a| {
                                format!(
                                    "{} file(s) written, {} unchanged, {} deleted",
                                    a.written, a.unchanged, a.deleted
                                )
                            }),
                        (XferWhat::Refs { .. }, XferPack::Refs { .. }) => {
                            places::apply_refs(&pack, &dest, &store).map(|l| l.join("; "))
                        }
                        _ => Err("transfer index does not match its offer".into()),
                    };
                    done.map(|d| {
                        format!("{me} place {}: {d}; {moved} in {:.1}s", offer.place, started.elapsed().as_secs_f64())
                    })
                    .map_err(|e| format!("{me} place {}: {e}", offer.place))
                }
                _ => Err("transfer index does not match its offer".into()),
            };
            let (ok, text) = match msg {
                Ok(t) => (true, t),
                Err(e) => (false, e),
            };
            let _ = done.send(Done::Landed(Landed { to: from, re, ok, msg: text, for_req }));
        }) {
            let msg = format!("handoff failed: {e}");
            let _ =
                fail_done.send(Done::Landed(Landed { to: from, re: fail_re, ok: false, msg, for_req: fail_for_req }));
        }
        self.refresh_transfers();
    }

    async fn want(&mut self, re: &str, missing: Vec<String>) {
        let Some(inc) = self.incoming.get_mut(re) else { return };
        inc.pushed = false;
        inc.asks += 1;
        if inc.asks > MAX_ASKS {
            let from = inc.from;
            self.incoming.remove(re);
            let _ = self.folder_landed(re, false);
            return self.ack(&from, re, false, "handoff gave up: chunks kept going missing").await;
        }
        let from = inc.from;
        self.note(format!("asking for {} missing chunk(s)", missing.len()));
        self.reply(&from, Body::ChunkWant { re: re.to_string(), ids: missing }).await;
    }

    pub(super) fn refresh_transfers(&self) {
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

    /// Target, last step: tell the sender how the transfer landed.
    pub(super) async fn on_landed(&mut self, Landed { to, re, mut ok, mut msg, for_req }: Landed) {
        self.incoming.remove(&re);
        match self.folder_landed(&re, ok) {
            Some(Ok(more)) if !more.is_empty() => msg = format!("{msg}; {more}"),
            Some(Err(e)) if ok => (ok, msg) = (false, e),
            _ => {}
        }
        self.refresh_transfers();
        self.publish_now = true;
        self.ack(&to, &re, ok, msg.clone()).await;
        // A pull this device asked for: its CLI follows the get's id.
        if let Some(get) = for_req {
            self.pending_gets.remove(&get);
            let from = self.peer_name(&to).unwrap_or_default();
            self.note(ipc::outcome_line(&from, ok.into(), &get, &msg));
        }
    }

    /// Source: offer files or refs from one of our places to `pk`.
    pub(super) async fn start_xfer(
        &mut self,
        pk: PublicKey,
        device: &str,
        id: &str,
        mut offer: XferOffer,
    ) -> Result<(), String> {
        let base = self.ctx.fleet.place(&offer.place, &Fleet::file(&self.ctx.sync_dir))?;
        let folder = offer.folder.as_ref().map(|p| format!("{} change(s)", p.files.len() + p.gone.len()));
        let label = match &mut offer.what {
            XferWhat::Files { roots, delete } => {
                let (b, r) = (base.clone(), roots.clone());
                let scan =
                    tokio::task::spawn_blocking(move || places::scan(&b, &r)).await.map_err(|e| e.to_string())??;
                let over = scan.bytes >= self.ctx.fleet.xfer.bulk_over;
                if over {
                    // With no route to it, the recipient pulls from us.
                    let src = (!self.ctx.fleet.ssh.contains_key(device)).then(|| base.display().to_string());
                    offer.bulk = Some(BulkOffer { src, ..scan.clone() });
                }
                format!(
                    "files {} in {}{}{}",
                    folder.unwrap_or_else(|| roots.join(" ")),
                    offer.place,
                    if *delete { " (--delete)" } else { "" },
                    if over { format!(" ({} over rsync)", format_bytes(scan.bytes)) } else { String::new() }
                )
            }
            XferWhat::Refs { map, repo_id } => {
                let b = base.clone();
                let repo =
                    tokio::task::spawn_blocking(move || places::place_repo(&b).map(|r| (places::this_repo_id(&r), r)))
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
                self.track(id, ev);
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
            let (to, for_req) = (o.to, o.job_for_req());
            if let Some(o) = self.outgoing.remove(re) {
                self.forget_events(o.events).await;
            }
            self.refresh_transfers();
            self.outcome_as(re, Outcome::RelayFailed, &format!("relay failed: {e}"));
            // Best effort: the relay may still carry a short message.
            if for_req.is_some() {
                let msg = format!("relay failed: {e}");
                let env = Envelope::new(&me, Body::Ack { re: re.to_string(), ok: false, msg, relay: true });
                let _ = self.t.send(&to, &env).await;
            }
            return;
        };
        let XferWhat::Files { roots, delete } = offer.what.clone() else {
            if let Some(o) = self.outgoing.remove(re) {
                self.forget_events(o.events).await;
            }
            return self.outcome_as(re, Outcome::RelayFailed, &format!("relay failed: {e}"));
        };
        self.note(format!("{me}: relay failed for {re} ({e}); copying over rsync to {host}"));
        let (done, view, re, place) = (self.done.clone(), self.view.clone(), re.to_string(), offer.place.clone());
        let (fail_done, fail_re) = (done.clone(), re.clone());
        if let Err(e) = spawn_named("rsync-copy", move || {
            let res = (|| {
                let scan = places::scan(&base, &roots)?;
                let far = places::remote_place(&host, &place, false, &roots)?;
                let dst = End::Remote { host: host.clone(), path: far.path.into() };
                places::copy(&End::Local(base), &dst, &roots, &scan, delete, &|l| beat(&view, &re, None, l))
            })();
            let _ = done.send(Done::Bulk(re, res.map(|m| format!("{place}: {m}"))));
        }) {
            let _ = fail_done.send(Done::Bulk(fail_re, Err(e)));
        }
    }

    /// Sender of a bulk copy it ran: report it here, tell the recipient
    /// (whose pull, if it asked, ends on that), and free the relay.
    pub(super) async fn on_bulk(&mut self, re: String, res: Result<String, String>) {
        let Some(o) = self.outgoing.remove(&re) else { return };
        let (ok, msg) = match res {
            Ok(m) => (true, format!("{} place {m}", o.device)),
            Err(e) => (false, e),
        };
        self.folder_sent(&re, ok, &msg);
        self.outcome(&re, if ok { Ok(msg.clone()) } else { Err(msg.clone()) });
        self.reply(&o.to, Body::Ack { re: re.clone(), ok, msg, relay: false }).await;
        self.forget_events(o.events).await;
        self.refresh_transfers();
    }

    pub(super) async fn forget_events(&self, ids: Vec<nostr_sdk::prelude::EventId>) {
        if ids.is_empty() {
            return;
        }
        if let Err(e) = self.t.delete(&ids).await {
            self.note(format!("could not free {} relay event(s): {e}", ids.len()));
        }
    }

    /// Target: check the place and what it may receive, then report what
    /// it already has (commit tips, for refs) so the sender packs less.
    /// `sender` is the name the sender gives itself (conflict copies are
    /// named after it, alike on every device).
    pub(super) async fn on_xfer_offer(
        &mut self,
        from: &PublicKey,
        name: &str,
        sender_self: &str,
        offer_id: String,
        offer: XferOffer,
    ) {
        let folder = match &offer.folder {
            None => None,
            Some(push) => match self.folder_had(&offer.place, from) {
                Ok((had, me_first)) => Some((push.clone(), had, me_first)),
                Err(e) => return self.ack(from, &offer_id, false, e).await,
            },
        };
        let (me, them) = (self.ctx.fleet.device.clone(), sender_self.to_string());
        let fleet = self.ctx.fleet.clone();
        let file = Fleet::file(&self.ctx.sync_dir);
        let o = offer.clone();
        let sender = name.to_string();
        let versions = self.versions();
        let checked = tokio::task::spawn_blocking(move || -> Result<(PathBuf, Vec<String>, Option<folders::Plan>), String> {
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
                    // Sort out conflicts before anything is written.
                    let plan = match &folder {
                        Some((push, had, me_first)) => Some(folders::prepare(&dest, push, had, &me, &them, *me_first)?),
                        None => None,
                    };
                    // Keep what the incoming versions replace, while it is still here.
                    if let Some(p) = &plan {
                        for rel in &p.replaced {
                            let _ = versions.keep(&o.place, &dest, rel, &format!("Replaced by {sender}"));
                        }
                    }
                    Ok((dest, Vec::new(), plan))
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
                    Ok((dest, places::repo_tips(&repo), None))
                }
            }
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let (dest, tips, plan) = match checked {
            Ok(x) => x,
            Err(e) => {
                if let Some(get) = &offer.for_req {
                    self.pending_gets.remove(get);
                    self.outcome(get, Err(e.clone()));
                }
                return self.ack(from, &offer_id, false, e).await;
            }
        };
        self.note(format!("{name} is sending into place {} → {}", offer.place, dest.display()));
        let mut skip = Vec::new();
        if let Some(plan) = plan {
            skip = plan.skip.clone();
            self.folder_arriving(&offer_id, *from, name.to_string(), &offer, plan);
        }
        let bulk = offer.bulk.clone();
        let (place, what, for_req) = (offer.place.clone(), offer.what.clone(), offer.for_req.clone());
        self.incoming.insert(offer_id.clone(), Incoming::new(*from, InJob::Xfer(offer), dest.clone()));
        self.refresh_transfers();
        match (bulk, what) {
            // The sender can't reach us: we pull from the path it gave.
            (Some(scan @ BulkOffer { src: Some(_), .. }), XferWhat::Files { roots, delete }) => {
                let host = self.ctx.fleet.ssh.get(name).cloned().unwrap_or_default();
                let src = End::Remote { host, path: scan.src.clone().unwrap_or_default().into() };
                let (done, view, me, re, to) =
                    (self.done.clone(), self.view.clone(), self.ctx.fleet.device.clone(), offer_id, *from);
                let (fail_done, fail_re, fail_for_req) = (done.clone(), re.clone(), for_req.clone());
                let fail_prefix = format!("{me} place {place}");
                if let Err(e) = spawn_named("pull-copy", move || {
                    let res = places::copy(&src, &End::Local(dest), &roots, &scan, delete, &|l| {
                        beat(&view, &re, for_req.as_deref(), l)
                    });
                    let (ok, msg) = match res {
                        Ok(m) => (true, format!("{me} place {place}: {m}")),
                        Err(e) => (false, format!("{me} place {place}: {e}")),
                    };
                    let _ = done.send(Done::Landed(Landed { to, re, ok, msg, for_req }));
                }) {
                    let msg = format!("{fail_prefix}: {e}");
                    let _ =
                        fail_done.send(Done::Landed(Landed { to, re: fail_re, ok: false, msg, for_req: fail_for_req }));
                }
            }
            (Some(_), _) => {
                let dest = Some(dest.display().to_string());
                self.reply(from, Body::HandoffHave(HandoffHave { re: offer_id, tips, dest, skip })).await;
            }
            (None, _) => {
                self.reply(from, Body::HandoffHave(HandoffHave { re: offer_id, tips, dest: None, skip })).await
            }
        }
    }

    /// Source of a pull: send it back as an ordinary offer.
    pub(super) async fn on_xfer_get(&mut self, from: PublicKey, name: &str, get_id: &str, offer: XferOffer) {
        let offer = XferOffer { for_req: Some(get_id.to_string()), ..offer };
        let id = protocol::new_id();
        if let Err(e) = self.start_xfer(from, name, &id, offer).await {
            self.ack(&from, get_id, false, e).await;
        }
    }
}
