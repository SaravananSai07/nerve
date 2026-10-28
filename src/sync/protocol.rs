//! Wire format for device-to-device sync. Pure data + encoding rules, no
//! I/O, so it's unit-testable and a web client can mirror it.
//!
//! Every payload is JSON, NIP-44 encrypted to exactly one recipient
//! device. Three Nostr event classes carry it:
//!
//! - `KIND_STATE` (addressable, NIP-01 30000-39999): a device's latest
//!   state, one per recipient (`d` = "nerve/1/state/<recipient>"). The
//!   relay keeps only the newest, so a closed laptop's last state stays
//!   readable: "what was it doing when it went away".
//! - `KIND_MSG` (regular, stored) with a NIP-40 expiration: commands,
//!   answers, blobs, job traffic. Stored, so a device that was asleep
//!   catches up on reconnect (store-and-forward).
//! - Acks are ordinary messages: a relay's `OK` is not delivery (public
//!   relays were measured accepting and then silently dropping events).
//!
//! Strings arriving here are untrusted; the receiver wraps anything it
//! renders in `Sanitised`.

use serde::{Deserialize, Serialize};

/// 2: chunked hand-off, invite pairing, mod-backed sessions. Devices on
/// different versions ignore each other rather than misread.
pub(crate) const PROTO_VERSION: u32 = 2;
/// NIP-78 application data: addressable, so relays keep the latest only.
pub(crate) const KIND_STATE: u16 = 30078;
/// Regular (stored) kind in the unassigned 4xxx range.
pub(crate) const KIND_MSG: u16 = 4590;
/// Messages expire after a day; state after a week.
pub(crate) const MSG_TTL_SECS: u64 = 24 * 3600;
pub(crate) const STATE_TTL_SECS: u64 = 7 * 24 * 3600;
/// Raw bytes carried per message. NIP-44 v2 caps plaintext at 64 KiB;
/// 32 KiB of data is ~44 KB once base64'd into the JSON envelope.
pub(crate) const CHUNK_BYTES: usize = 32 * 1024;
/// Lines of transcript preview carried per session in a state.
pub(crate) const TAIL_LINES: usize = 8;

pub(crate) fn state_d_tag(recipient_hex: &str) -> String {
    format!("nerve/{PROTO_VERSION}/state/{}", &recipient_hex[..16.min(recipient_hex.len())])
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Envelope {
    pub(crate) v: u32,
    /// Random id; acks and replies point at it.
    pub(crate) id: String,
    /// Sender's device name (informational; identity is the pubkey).
    pub(crate) from: String,
    pub(crate) sent_at: u64,
    pub(crate) body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum Body {
    State(Box<DeviceState>),
    /// Type `text` into a running session's prompt, then Enter.
    Send { session: String, text: String },
    /// Resolve a pending permission prompt or question.
    Answer { request: String, answer: Answer },
    /// Ask the device to republish its state now.
    SyncNow,
    /// Ask for a larger artefact; it comes back as `Blob` chunks.
    Fetch { session: String, what: FetchWhat },
    Blob(Blob),
    /// Continue `session` on the recipient. Starts a short exchange:
    /// offer → `HandoffHave` (what the target's checkout already holds)
    /// → `HandoffManifest` (the files, as chunk ids) → `ChunkWant` (only
    /// the ids the target lacks) → `ChunkData` → `Ack`.
    HandoffOffer(HandoffOffer),
    HandoffHave(HandoffHave),
    HandoffManifest(HandoffManifest),
    /// Ask the sender of `re` for these chunks.
    ChunkWant { re: String, ids: Vec<String> },
    /// Chunks for `re`: (id, base64 bytes), at most `CHUNK_BYTES` raw.
    ChunkData { re: String, chunks: Vec<(String, String)>, last: bool },
    /// Join with an invite code (`nerve setup <code>`): proves knowledge
    /// of the inviter's one-time secret. The only message accepted from
    /// a key that is not yet a peer.
    PairRequest { secret: String, name: String, relays: Vec<String> },
    JobSubmit(JobSpec),
    /// Progress of a job or a run; runs carry `kind = "run"`.
    JobUpdate(JobStatus),
    /// Run one allowlisted command (`nerve sync run`).
    RunSubmit(RunSpec),
    /// Send files or git refs into a named place on the recipient. The
    /// exchange is the hand-off's: `HandoffHave` → `HandoffManifest` →
    /// `ChunkWant`/`ChunkData` → `Ack`.
    XferOffer(XferOffer),
    /// Ask the recipient to send this back as an `XferOffer` (pull).
    XferGet(XferOffer),
    Ack { re: String, ok: bool, msg: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Answer {
    Allow,
    Deny { reason: String },
    /// One label per question, in question order.
    Choose { labels: Vec<String> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FetchWhat {
    Diff,
    Log,
    Subagents,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Blob {
    /// The message this answers (a Fetch id, or `handoff:<id>`).
    pub(crate) re: String,
    pub(crate) title: String,
    pub(crate) part: u32,
    pub(crate) parts: u32,
    /// base64 of this chunk's bytes.
    pub(crate) data: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct DeviceState {
    pub(crate) device: String,
    pub(crate) host: String,
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) caps: Vec<String>,
    pub(crate) nerve_version: String,
    pub(crate) generated_at: u64,
    /// The user is away from this device: questions route remotely.
    pub(crate) away: bool,
    pub(crate) sessions: Vec<RemoteSession>,
    pub(crate) requests: Vec<PendingRequest>,
    pub(crate) jobs: Vec<JobStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct RemoteSession {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) branch: Option<String>,
    pub(crate) state: String,
    pub(crate) waiting_for: Option<String>,
    pub(crate) kind: String,
    pub(crate) tool: Option<String>,
    pub(crate) cost_usd: f64,
    pub(crate) active_age_secs: Option<f64>,
    /// True when a remote `Send` can reach it (mod, tmux pane or Ghostty tab).
    pub(crate) injectable: bool,
    pub(crate) subagents: u32,
    pub(crate) tail: Vec<String>,
    /// The nerve mod is loaded in this session: prompts, answers and
    /// turn events go through it rather than the terminal.
    #[serde(default)]
    pub(crate) via_mod: bool,
    #[serde(default)]
    pub(crate) last_turn: Option<TurnInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct PendingRequest {
    pub(crate) id: String,
    pub(crate) session: String,
    /// "permission" or "question".
    pub(crate) kind: String,
    pub(crate) tool: String,
    pub(crate) summary: String,
    pub(crate) questions: Vec<Question>,
    pub(crate) created_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Question {
    pub(crate) question: String,
    pub(crate) header: String,
    pub(crate) options: Vec<String>,
    pub(crate) multi: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct HandoffOffer {
    pub(crate) session: String,
    pub(crate) name: String,
    pub(crate) src_cwd: String,
    pub(crate) repo_name: String,
    /// The repo's first commit: names "the same project" on every device.
    pub(crate) repo_id: String,
    pub(crate) branch: String,
    /// Typed into the resumed session once it is ready.
    pub(crate) continue_with: Option<String>,
    /// A place on the target to land in (`--to`), instead of `path_map`.
    #[serde(default)]
    pub(crate) place: Option<String>,
    /// Switch an existing checkout that is on another branch.
    #[serde(default)]
    pub(crate) force: bool,
}

/// One allowlisted command, run with argv as given (no shell unless the
/// matching `[run] allow` entry says so).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct RunSpec {
    pub(crate) id: String,
    pub(crate) argv: Vec<String>,
    /// Run inside this place (its path on the executing device).
    pub(crate) place: Option<String>,
    /// Or inside this absolute directory on the executing device.
    pub(crate) cwd: Option<String>,
    /// Kill the command after this long; it may otherwise run past the
    /// 5-minute window, which only bounds *starting* it.
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) submitted_by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct XferOffer {
    /// Names a directory in each device's `[places]`: senders never need
    /// to know the remote path.
    pub(crate) place: String,
    pub(crate) what: XferWhat,
    /// The recipient's `XferGet` this answers.
    #[serde(default)]
    pub(crate) for_req: Option<String>,
    /// Too big for the relay (`[xfer] bulk_over`): the files go over
    /// rsync and the relay only carries the coordination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bulk: Option<BulkOffer>,
}

/// What a bulk transfer holds, scanned on the sender.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct BulkOffer {
    /// The roots that are directories: the scope of `delete`, and rsync
    /// copies them with a trailing slash.
    pub(crate) dirs: Vec<String>,
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    /// Set when the sender has no ssh route to the recipient: its own
    /// path of the place, for the recipient to pull from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) src: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum XferWhat {
    /// Paths relative to the place. `delete` removes files under the
    /// directories among them that the sender no longer has.
    Files { roots: Vec<String>, delete: bool },
    /// Git refs of the place's repo: sender ref → receiver ref.
    Refs { map: Vec<RefMap>, repo_id: String },
}

impl Default for XferWhat {
    fn default() -> Self {
        Self::Files { roots: Vec::new(), delete: false }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct RefMap {
    pub(crate) src: String,
    pub(crate) dst: String,
}

/// The target's answer to an offer: commits its copy of the repo
/// already has (branch tips), so the sender's git bundle starts there.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct HandoffHave {
    pub(crate) re: String,
    pub(crate) tips: Vec<String>,
    /// For a bulk transfer the sender runs: the recipient's own path of
    /// the place, checked and with its parent directories made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dest: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct HandoffManifest {
    pub(crate) re: String,
    /// The manifest JSON itself, chunked: large repos list thousands of
    /// chunk ids, which don't fit one message.
    pub(crate) index: super::chunks::FileRef,
}

/// What one hand-off carried, for the log and the experiments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TransferStats {
    pub(crate) files: usize,
    pub(crate) total_bytes: u64,
    pub(crate) sent_bytes: u64,
    pub(crate) chunks: usize,
    pub(crate) sent_chunks: usize,
}

/// The last finished turn of a session, as its mod reported it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct TurnInfo {
    pub(crate) ended_at: u64,
    pub(crate) ms: u64,
    pub(crate) reason: String,
    pub(crate) model: String,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct JobSpec {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) prompt: String,
    /// Directory on the executing device; `{repo}` is cloned there if missing.
    pub(crate) cwd: Option<String>,
    pub(crate) repo: Option<String>,
    /// Capabilities the executing device must advertise.
    pub(crate) requires: Vec<String>,
    /// Shell command run after the agent finishes; exit 0 = verified.
    pub(crate) verify: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) permission_mode: String,
    /// `--allowedTools` rules, e.g. "Bash(python3 *)". Headless jobs deny
    /// anything that would prompt, so shell steps must be allowed here.
    #[serde(default)]
    pub(crate) allowed_tools: Vec<String>,
    pub(crate) submitted_by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct JobStatus {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) device: String,
    /// queued | running | verifying | done | failed
    pub(crate) phase: String,
    pub(crate) session_id: Option<String>,
    pub(crate) turns: u32,
    pub(crate) cost_usd: f64,
    pub(crate) verified: Option<bool>,
    pub(crate) tail: Vec<String>,
    pub(crate) result: Option<String>,
    pub(crate) updated_at: u64,
    /// "" for a Claude job, "run" for a command run.
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    /// Output lines seen so far, so a viewer can tell how many of `tail`
    /// are new since the last update.
    #[serde(default)]
    pub(crate) lines: u64,
}

impl Envelope {
    pub(crate) fn new(from: &str, body: Body) -> Self {
        Self {
            v: PROTO_VERSION,
            id: new_id(),
            from: from.to_string(),
            sent_at: now_secs(),
            body,
        }
    }

    /// With an id the caller chose, so a CLI can follow its outcome.
    pub(crate) fn with_id(from: &str, id: &str, body: Body) -> Self {
        Self { id: id.to_string(), ..Self::new(from, body) }
    }

    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).expect("envelope serialises")
    }

    /// Rejects other protocol versions rather than misreading them.
    pub(crate) fn decode(s: &str) -> Result<Self, String> {
        let e: Self = serde_json::from_str(s).map_err(|e| format!("bad envelope: {e}"))?;
        if e.v != PROTO_VERSION {
            return Err(format!("unsupported protocol v{}", e.v));
        }
        Ok(e)
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 16 hex chars from the OS RNG (via the nanosecond clock + pid as a
/// fallback seed — ids only need to be unique, not secret).
pub(crate) fn new_id() -> String {
    let mut buf = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_err()
    {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0) as u64
            ^ (std::process::id() as u64) << 32;
        buf = n.to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Split bytes into base64 chunks of at most `CHUNK_BYTES` raw bytes.
pub(crate) fn chunk(re: &str, title: &str, bytes: &[u8]) -> Vec<Blob> {
    let pieces: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[][..]]
    } else {
        bytes.chunks(CHUNK_BYTES).collect()
    };
    let parts = pieces.len() as u32;
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, p)| Blob {
            re: re.to_string(),
            title: title.to_string(),
            part: i as u32,
            parts,
            data: base64_encode(p),
        })
        .collect()
}

/// Reassembles chunks that may arrive out of order or twice.
#[derive(Debug, Default)]
pub(crate) struct Reassembly {
    pub(crate) title: String,
    parts: Vec<Option<Vec<u8>>>,
}

impl Reassembly {
    /// Feed one chunk; returns the whole payload once every part is in.
    pub(crate) fn add(&mut self, b: &Blob) -> Result<Option<Vec<u8>>, String> {
        if b.parts == 0 || b.part >= b.parts || b.parts > 100_000 {
            return Err("bad chunk numbering".into());
        }
        if self.parts.is_empty() {
            self.parts = vec![None; b.parts as usize];
            self.title = b.title.clone();
        } else if self.parts.len() != b.parts as usize {
            return Err("chunk count changed mid-stream".into());
        }
        self.parts[b.part as usize] = Some(base64_decode(&b.data)?);
        if self.parts.iter().all(Option::is_some) {
            Ok(Some(self.parts.iter().flatten().flatten().copied().collect()))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn progress(&self) -> (usize, usize) {
        (self.parts.iter().filter(|p| p.is_some()).count(), self.parts.len())
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub(crate) fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for ch in s.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err("invalid base64".into()),
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Claude's project-dir slug: every non-alphanumeric char becomes `-`.
/// The handoff must install the transcript under the *target's* slug.
pub(crate) fn project_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Devices that can run a job: advertise every required capability.
/// Least busy wins; ties go to the name for determinism, so every device
/// computing this agrees.
pub(crate) fn pick_device<'a>(
    candidates: impl IntoIterator<Item = (&'a str, &'a [String], usize)>,
    requires: &[String],
) -> Option<&'a str> {
    candidates
        .into_iter()
        .filter(|(_, caps, _)| requires.iter().all(|r| caps.iter().any(|c| c == r)))
        .min_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(b.0)))
        .map(|(name, _, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_and_rejects_other_versions() {
        let e = Envelope::new("laptop", Body::Send { session: "s1".into(), text: "hi\nthere".into() });
        let back = Envelope::decode(&e.encode()).unwrap();
        assert_eq!(back, e);
        let future = e.encode().replacen("\"v\":2", "\"v\":9", 1);
        assert!(Envelope::decode(&future).unwrap_err().contains("v9"));
    }

    #[test]
    fn answer_wire_shape_is_stable() {
        let a = Answer::Choose { labels: vec!["Bash".into()] };
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"kind":"choose","labels":["Bash"]}"#);
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (raw, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64_encode(raw.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), raw.as_bytes());
        }
        assert!(base64_decode("a$b").is_err());
    }

    #[test]
    fn chunks_reassemble_out_of_order_and_tolerate_duplicates() {
        let data: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17)).map(|i| (i * 7 % 251) as u8).collect();
        let mut blobs = chunk("r1", "pack", &data);
        assert_eq!(blobs.len(), 3);
        blobs.reverse();
        let mut r = Reassembly::default();
        assert_eq!(r.add(&blobs[0]).unwrap(), None);
        assert_eq!(r.add(&blobs[0]).unwrap(), None, "duplicate is harmless");
        assert_eq!(r.add(&blobs[1]).unwrap(), None);
        assert_eq!(r.add(&blobs[2]).unwrap().unwrap(), data);
        assert_eq!(r.progress(), (3, 3));
    }

    #[test]
    fn empty_payload_is_one_empty_chunk() {
        let blobs = chunk("r", "t", b"");
        let mut r = Reassembly::default();
        assert_eq!(r.add(&blobs[0]).unwrap().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn slug_matches_claude_project_dirs() {
        assert_eq!(project_slug("/home/user/lab/ho-plain"), "-home-user-lab-ho-plain");
        assert_eq!(project_slug("/Users/a/SS/x.y"), "-Users-a-SS-x-y");
    }

    #[test]
    fn job_goes_to_least_busy_capable_device() {
        let laptop = ["xcode".to_string(), "ios-sim".to_string()];
        let server = ["linux".to_string(), "arm64".to_string()];
        let devs = [("laptop", &laptop[..], 3), ("server", &server[..], 0)];
        assert_eq!(pick_device(devs, &["ios-sim".into()]), Some("laptop"));
        assert_eq!(pick_device(devs, &[]), Some("server"));
        assert_eq!(pick_device(devs, &["gpu".into()]), None);
    }
}
