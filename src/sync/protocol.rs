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
//!   answers, blobs, run traffic. Stored, so a device that was asleep
//!   catches up on reconnect (store-and-forward).
//! - Acks are ordinary messages: a relay's `OK` is not delivery (public
//!   relays were measured accepting and then silently dropping events).
//!
//! Strings arriving here are untrusted; the receiver wraps anything it
//! renders in `Sanitised`.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// 2: chunked hand-off, invite pairing, mod-backed sessions. Devices on
/// different versions ignore each other rather than misread.
const PROTO_VERSION: u32 = 2;
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
    Send {
        session: String,
        text: String,
    },
    /// Resolve a pending permission prompt or question.
    Answer {
        request: String,
        answer: Answer,
    },
    /// Ask the device to republish its state now.
    SyncNow,
    /// Stop a running session (SIGTERM, or `claude stop` for a `--bg` one).
    Stop {
        session: String,
    },
    /// Who to notify when `session` (running on the recipient) finishes a
    /// turn. Empty turns it off; the devices ticked in Settings follow
    /// Settings again.
    SetNotify {
        session: String,
        devices: Vec<String>,
    },
    /// Ask for a larger artefact; it comes back as `Blob` chunks.
    Fetch {
        session: String,
        what: FetchWhat,
    },
    Blob(Blob),
    /// Continue `session` on the recipient. Starts a short exchange:
    /// offer → `HandoffHave` (what the target's checkout already holds)
    /// → `HandoffManifest` (the files, as chunk ids) → `ChunkWant` (only
    /// the ids the target lacks) → `ChunkData` → `Ack`.
    HandoffOffer(HandoffOffer),
    HandoffHave(HandoffHave),
    HandoffManifest(HandoffManifest),
    /// Ask the sender of `re` for these chunks.
    ChunkWant {
        re: String,
        ids: Vec<String>,
    },
    /// Chunks for `re`: (id, base64 bytes), at most `CHUNK_BYTES` raw.
    ChunkData {
        re: String,
        chunks: Vec<(String, String)>,
        last: bool,
    },
    /// Join with an invite code (`nerve connect <code>`): proves knowledge
    /// of the inviter's one-time secret. The only message accepted from
    /// a key that is not yet a peer. `want`: which of the offered devices
    /// to join besides the inviter (None: all of them).
    PairRequest {
        secret: String,
        name: String,
        relays: Vec<String>,
        want: Option<Vec<String>>,
    },
    /// The join of `re` was allowed: these devices (name, key hex, and
    /// whether devices joining later meet it) now trust the joiner.
    Welcome {
        re: String,
        devices: Vec<Met>,
    },
    /// Trust this device too: one that both of us trust says so.
    Introduce(Met),
    /// Stop trusting this device (key hex): it was forgotten elsewhere.
    Forget {
        key: String,
    },
    /// A person's answer to an approval this device holds.
    Decide(Decision),
    /// Start a new interactive session on the recipient (`nerve sync start`).
    Start(StartSpec),
    /// Progress of a run. Named as before runs had their own name, so
    /// devices on either side of that change still read it.
    #[serde(rename = "job_update")]
    RunUpdate(RunStatus),
    /// Run one allowlisted command (`nerve sync run`).
    RunSubmit(RunSpec),
    /// Send files or git refs into a named place on the recipient. The
    /// exchange is the hand-off's: `HandoffHave` → `HandoffManifest` →
    /// `ChunkWant`/`ChunkData` → `Ack`.
    XferOffer(XferOffer),
    /// Ask the recipient to send this back as an `XferOffer` (pull).
    XferGet(XferOffer),
    /// Share a synced folder with the recipient, or change who shares it.
    /// A recipient missing from `members` stops syncing it.
    FolderSet(FolderSet),
    /// Send me your changes to this folder now.
    FolderNow {
        name: String,
    },
    Ack {
        re: String,
        ok: bool,
        msg: String,
        /// It failed because the relay could not carry it.
        #[serde(default)]
        relay: bool,
    },
    /// A body this build doesn't know: from a newer nerve, or one that is
    /// gone (`job_submit`, the headless jobs `start` replaced). The
    /// recipient refuses it with an ack instead of dropping the envelope.
    #[serde(other)]
    Unknown,
}

impl Body {
    /// Does it make this device do something? A one-shot CLI agent leaves
    /// that to the long-lived one. Exhaustive, so a new variant has to
    /// be classified here.
    pub(crate) fn is_work(&self) -> bool {
        match self {
            Self::Send { .. }
            | Self::Answer { .. }
            | Self::Stop { .. }
            | Self::SetNotify { .. }
            | Self::Fetch { .. }
            | Self::HandoffOffer(_)
            | Self::HandoffManifest(_)
            | Self::ChunkData { .. }
            | Self::Start(_)
            | Self::RunSubmit(_)
            | Self::XferOffer(_)
            | Self::XferGet(_)
            | Self::FolderSet(_)
            | Self::FolderNow { .. }
            | Self::Introduce(_)
            | Self::Welcome { .. }
            | Self::Forget { .. }
            | Self::Decide(_)
            | Self::PairRequest { .. }
            | Self::Unknown => true,
            Self::State(_)
            | Self::SyncNow
            | Self::Blob(_)
            | Self::HandoffHave(_)
            | Self::ChunkWant { .. }
            | Self::RunUpdate(_)
            | Self::Ack { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct FolderSet {
    pub(crate) name: String,
    /// Every device sharing it, by key (hex): names differ per device.
    pub(crate) members: Vec<String>,
    pub(crate) when: super::folders::When,
    /// The sender's path, for the recipient's `path_map`.
    pub(crate) src_path: String,
    /// The same path relative to the sender's home, when it is inside it.
    pub(crate) home_rel: Option<String>,
    /// What the members may sync next without asking.
    #[serde(default)]
    pub(crate) others: super::folders::Others,
    /// A person started it (in the TUI): no approval needed.
    #[serde(default)]
    pub(crate) yes: bool,
    /// What the sender holds, for whoever approves it.
    #[serde(default)]
    pub(crate) files: u32,
    #[serde(default)]
    pub(crate) bytes: u64,
}

/// A device met through another: its name there and key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Met {
    pub(crate) name: String,
    pub(crate) key: String,
    /// Devices that join later are connected to it automatically.
    pub(crate) later: bool,
}

/// Something that waits for a person's yes on the device holding it.
/// Any connected device may answer it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Approval {
    pub(crate) id: String,
    pub(crate) at: u64,
    /// Who asked, by key hex.
    pub(crate) from: String,
    pub(crate) what: Asked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Asked {
    /// A device asks to connect with a code this device showed.
    Join {
        name: String,
        /// Three words both screens show.
        words: String,
        /// The devices it would join, by this device's names.
        join: Vec<String>,
        later: bool,
        relays: Vec<String>,
        /// The request's envelope id, for the answer.
        re: String,
    },
    /// A device shares a folder new to this one: it would land at `lands`.
    Folder { set: FolderSet, lands: String, re: String },
}

impl Approval {
    /// What it asks, and the details to decide on: `asker` is the device
    /// that asked and `holder` the one it waits on, as the reader names them.
    pub(crate) fn describe(&self, asker: &str, holder: &str) -> (String, String) {
        match &self.what {
            Asked::Join { name, words, join, .. } => {
                let with: Vec<String> = std::iter::once(holder.to_string()).chain(join.iter().cloned()).collect();
                (
                    format!("{name} wants to connect"),
                    format!("Joins {}. Connect only if it shows: {words}.", crate::util::text::list_join(&with)),
                )
            }
            Asked::Folder { set, lands, .. } => {
                let size = match set.files {
                    0 => String::new(),
                    1 => format!(" 1 file, {}.", crate::util::text::format_bytes(set.bytes)),
                    n => format!(" {n} files, {}.", crate::util::text::format_bytes(set.bytes)),
                };
                (format!("{asker} wants to sync {}", set.name), format!("Lands at {lands} on {holder}.{size}"))
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Decision {
    pub(crate) id: String,
    pub(crate) yes: bool,
    /// A folder: land it here instead.
    #[serde(default)]
    pub(crate) at: Option<String>,
    /// A folder: what its members may sync next without asking.
    #[serde(default)]
    pub(crate) others: Option<super::folders::Others>,
}

/// The files of one synced-folder transfer, as (path, sha256) pairs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct FolderPush {
    pub(crate) files: Vec<(String, String)>,
    /// Deleted on the sender: (path, the version the recipient held).
    pub(crate) gone: Vec<(String, String)>,
    pub(crate) bytes: u64,
}

/// One synced folder in a device's state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct FolderStatus {
    pub(crate) name: String,
    pub(crate) path: String,
    /// Files this device has that each member (by key hex) lacks.
    pub(crate) pending: std::collections::BTreeMap<String, u32>,
    /// The rest, for this device's own TUI; peers don't get it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mine: Option<super::folders::View>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Answer {
    Allow,
    Deny {
        reason: String,
    },
    /// One label per question, in question order.
    Choose {
        labels: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FetchWhat {
    Diff,
    Log,
    Subagents,
    /// The session's last assistant message, in full.
    Reply,
    /// Its recent transcript, whole, for a device that has it open: a
    /// JSON list of lines as `tail` has them, without the length cap.
    Transcript,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Blob {
    /// The `Fetch` this answers (its message id).
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
    #[serde(default, rename = "jobs")]
    pub(crate) runs: Vec<RunStatus>,
    /// Fleet-wide notification settings; the newest copy wins.
    #[serde(default)]
    pub(crate) notify: crate::notify::NotifyPrefs,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) folders: Vec<FolderStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) approvals: Vec<Approval>,
    /// Keys (hex) of the devices this one is connected to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) linked: Vec<String>,
    /// The code this device shows for a new device, for its own TUI;
    /// peers never get it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) invite: Option<String>,
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
    /// Devices told when this session finishes a turn: its own choice, or
    /// the devices ticked in Settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) notify: Vec<String>,
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
    /// A synced folder's changes (`place` names the folder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) folder: Option<FolderPush>,
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
    /// For a synced folder: files the target already has as offered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skip: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct HandoffManifest {
    pub(crate) re: String,
    /// The manifest JSON itself, chunked: large repos list thousands of
    /// chunk ids, which don't fit one message.
    pub(crate) index: super::chunks::FileRef,
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

/// A new session: `claude <prompt>` in `dir` on the recipient, in a
/// detached tmux session, as a hand-off resumes one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct StartSpec {
    /// The session id the sender chose (a UUID), passed as
    /// `--session-id`, so it knows the id before the session reports.
    pub(crate) session: String,
    /// On the recipient: an absolute path, `~` or `~/…`, or a place name.
    pub(crate) dir: String,
    pub(crate) prompt: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct RunStatus {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) device: String,
    /// queued | running | done | failed
    pub(crate) phase: String,
    pub(crate) tail: Vec<String>,
    pub(crate) result: Option<String>,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    /// Output lines seen so far, so a viewer can tell how many of `tail`
    /// are new since the last update.
    #[serde(default)]
    pub(crate) lines: u64,
}

impl Envelope {
    pub(crate) fn new(from: &str, body: Body) -> Self {
        Self { v: PROTO_VERSION, id: new_id(), from: from.to_string(), sent_at: crate::util::now_secs(), body }
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

/// A random (v4) UUID, the form Claude takes for `--session-id`.
pub(crate) fn new_session_id() -> String {
    let hex = format!("{}{}", new_id(), new_id());
    let variant = ["8", "9", "a", "b"][usize::from_str_radix(&hex[16..17], 16).unwrap_or(0) % 4];
    format!("{}-{}-4{}-{variant}{}-{}", &hex[..8], &hex[8..12], &hex[13..16], &hex[17..20], &hex[20..32])
}

/// Is `id` a UUID (lowercase or not), as `--session-id` needs?
pub(crate) fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// 16 hex chars from the OS RNG (via the nanosecond clock + pid as a
/// fallback seed — ids only need to be unique, not secret).
pub(crate) fn new_id() -> String {
    let mut buf = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf)).is_err() {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
            as u64
            ^ ((std::process::id() as u64) << 32);
        buf = n.to_le_bytes();
    }
    buf.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The first `n` characters of an id, for display. Ids from the network
/// may be short or not ASCII, so this never slices inside a character.
pub(crate) fn short_id(id: &str, n: usize) -> &str {
    id.char_indices().nth(n).map_or(id, |(i, _)| &id[..i])
}

/// Split bytes into base64 chunks of at most `CHUNK_BYTES` raw bytes.
pub(crate) fn chunk(re: &str, title: &str, bytes: &[u8]) -> Vec<Blob> {
    let pieces: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(CHUNK_BYTES).collect() };
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
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = ((c[0] as u32) << 16) | ((*c.get(1).unwrap_or(&0) as u32) << 8) | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
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
        acc = (acc << 6) | v;
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
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
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
    fn short_ids_survive_short_and_multibyte_input() {
        assert_eq!(short_id("0123456789", 6), "012345");
        assert_eq!(short_id("abc", 6), "abc");
        assert_eq!(short_id("", 6), "");
        assert_eq!(short_id("ééééééé", 6), "éééééé");
    }

    #[test]
    fn answer_wire_shape_is_stable() {
        let a = Answer::Choose { labels: vec!["Bash".into()] };
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"kind":"choose","labels":["Bash"]}"#);
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
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
    fn a_start_round_trips_with_its_prompt_intact() {
        let prompt = "say \"hi\"; $(rm -rf ~) `id`\nline two\t'quoted'";
        let spec = StartSpec { session: new_session_id(), dir: "~/code/app".into(), prompt: prompt.into() };
        let e = Envelope::new("laptop", Body::Start(spec.clone()));
        let back = Envelope::decode(&e.encode()).unwrap();
        assert_eq!(back.body, Body::Start(spec));
        assert!(back.body.is_work());
    }

    #[test]
    fn session_ids_are_uuids() {
        for _ in 0..50 {
            let id = new_session_id();
            assert!(is_uuid(&id), "{id}");
            assert_eq!(&id[14..15], "4", "{id}");
            assert!("89ab".contains(&id[19..20]), "{id}");
        }
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("../../../../etc/passwd/xxxxxxxxxxxxxxxxx"));
    }

    // A device on an older build may still send a job: it must decode,
    // as something to refuse, not fail the whole envelope.
    #[test]
    fn a_job_from_an_older_device_decodes_as_unknown() {
        let old = r#"{"v":2,"id":"ab","from":"old","sent_at":1,"body":{"t":"job_submit","id":"x","title":"t","prompt":"p","cwd":null,"repo":null,"requires":[],"verify":"rm -rf /","model":null,"permission_mode":"bypassPermissions","submitted_by":"old"}}"#;
        let e = Envelope::decode(old).unwrap();
        assert_eq!(e.body, Body::Unknown);
        assert!(e.body.is_work(), "the long-lived agent answers it");
    }

    #[test]
    fn runs_keep_their_wire_names() {
        let st = RunStatus { id: "r".into(), phase: "done".into(), exit_code: Some(0), ..Default::default() };
        let json = serde_json::to_string(&Body::RunUpdate(st)).unwrap();
        assert!(json.starts_with(r#"{"t":"job_update""#), "{json}");
        let state = serde_json::to_value(DeviceState::default()).unwrap();
        assert!(state.get("jobs").is_some(), "{state}");
    }

    #[test]
    fn approvals_say_what_they_ask() {
        let set = FolderSet { name: "app-runs".into(), files: 12, bytes: 3 << 20, ..Default::default() };
        let a = Approval {
            id: "q".into(),
            at: 1,
            from: "ab".into(),
            what: Asked::Folder { set, lands: "~/code/app-runs".into(), re: "e".into() },
        };
        assert_eq!(
            a.describe("mini", "laptop"),
            ("mini wants to sync app-runs".into(), "Lands at ~/code/app-runs on laptop. 12 files, 3.0 MiB.".into())
        );
        let j = Approval {
            what: Asked::Join {
                name: "air".into(),
                words: "amber fox river".into(),
                join: vec!["studio".into()],
                later: true,
                relays: vec![],
                re: "e".into(),
            },
            ..a
        };
        assert_eq!(
            j.describe("air", "laptop").1,
            "Joins laptop and studio. Connect only if it shows: amber fox river."
        );
    }
}
