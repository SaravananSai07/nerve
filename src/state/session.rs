use std::path::PathBuf;
use std::time::Instant;

use serde::Serialize;

use crate::state::state_machine::StateMachine;
use crate::util::sanitize::Sanitised;

/// Consecutive `propose_state` calls required before the visible
/// state actually changes. Defends against flicker in upstream
/// signals (CPU spikes, transient file rewrites).
const CONFIRM_TICKS: u8 = 3;

/// Canonical session identity. Wrapping `String` here gives the
/// rest of the codebase a single type to think about, and lets us
/// encode the `--resume` precedence rule in one place (the
/// constructor) instead of scattering it through call sites.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct SessionId(String);

impl SessionId {
    pub(crate) fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::borrow::Borrow<str> for SessionId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) cost_usd: f64,
}

impl TokenUsage {
    pub(crate) fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    pub(crate) fn compact_display(&self) -> String {
        let total_k = self.total_tokens() as f64 / 1000.0;
        if self.cost_usd >= 0.01 {
            format!("{:.0}k/${:.2}", total_k, self.cost_usd)
        } else {
            format!("{:.0}k", total_k)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionState {
    Processing,
    ToolRunning(String),
    WaitingForInput,
    Idle,
    Error,
    /// The per-pid session file has vanished from discovery — the
    /// process is almost certainly gone. Evicted after a short grace
    /// window so the user notices the session ended.
    Vanished,
    /// Was `WaitingForInput` long enough (≥ 48 h) that we no longer
    /// assume the user is coming back to it imminently, but the
    /// process is alive. Kept indefinitely.
    Dormant,
}

impl SessionState {
    pub(crate) fn sort_priority(&self) -> u8 {
        match self {
            Self::Processing => 0,
            Self::ToolRunning(_) => 1,
            Self::WaitingForInput => 2,
            Self::Idle => 3,
            Self::Error => 5,
            Self::Dormant => 6,
            Self::Vanished => 7,
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Self::Processing => "Processing".into(),
            Self::ToolRunning(tool) => format!("Tool: {tool}"),
            Self::WaitingForInput => "Waiting".into(),
            Self::Idle => "Idle".into(),
            Self::Error => "Error".into(),
            Self::Dormant => "Dormant".into(),
            Self::Vanished => "Gone".into(),
        }
    }

    /// True for states where the session is no longer actively
    /// producing work — `start_kill` uses this to refuse a redundant
    /// SIGTERM on a dead/dormant session.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Vanished | Self::Dormant)
    }
}

/// Plain snapshot of everything the discovery worker can know about a
/// session at a point in time. Decoupling this from `Session` means
/// the worker never constructs registry-owned state (StateMachine,
/// state_changed_at, ActivityHistory, last_notified_state) — those
/// belong to whichever `Session` the registry owns, and the worker
/// has no business creating them just so they get overwritten.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DiscoverySnapshot {
    pub(crate) id: SessionId,
    pub(crate) cwd: PathBuf,
    pub(crate) name: Sanitised,
    pub(crate) tty: Option<Sanitised>,
    pub(crate) branch: Option<Sanitised>,
    pub(crate) cpu_percent: f32,
    pub(crate) pid: Option<u32>,
    pub(crate) detected_state: SessionState,
    pub(crate) current_tool: Option<Sanitised>,
    pub(crate) usage: TokenUsage,
    pub(crate) jsonl_path: Option<PathBuf>,
    pub(crate) jsonl_age_secs: Option<f64>,
}

// Clone is test-only. A Session carries registry-owned mutable state
// — `state_machine`'s confirmation counter, `last_notified_state` for
// notification dedup, `activity` ring — so a production clone inserted
// alongside the original would silently double-count. Tests need Clone
// to set up fixtures; production paths only ever hand around &Session
// references.
#[cfg_attr(test, derive(Clone))]
pub(crate) struct Session {
    pub(crate) id: SessionId,
    pub(crate) cwd: PathBuf,
    name: Sanitised,
    state_changed_at: Instant,
    pub(crate) tty: Option<Sanitised>,
    pub(crate) branch: Option<Sanitised>,
    pub(crate) cpu_percent: f32,
    /// Last tool the session was running. Sticks past the
    /// `ToolRunning` state into `Idle` so users can see what the
    /// session was last doing when it goes quiet — only `merge_snapshot`
    /// updates it (and only when the snapshot has a tool to give).
    pub(crate) current_tool: Option<Sanitised>,
    activity: ActivityHistory,
    pub(crate) jsonl_path: Option<PathBuf>,
    /// User-supplied custom name via the rename overlay. Privately
    /// owned because the legal write path is `rename_to`, which
    /// updates `name` in lockstep — letting external code flip this
    /// without setting the name (or vice versa) breaks the
    /// disambiguation logic in `registry::re_disambiguate_names`.
    renamed: bool,
    pub(crate) usage: TokenUsage,
    pub(crate) pid: Option<u32>,
    pub(crate) jsonl_age_secs: Option<f64>,
    last_notified_state: Option<SessionState>,
    state_machine: StateMachine<SessionState>,
}

// Debug is hand-written rather than derived — the registry-side
// invariants (state_machine confirmation counter, last_notified_state)
// are noise in debug output. We surface only the load-bearing
// identity + current state.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("state", self.state())
            .finish()
    }
}

impl Session {
    /// Construct a registry-side `Session` from a freshly observed
    /// snapshot. The state machine is seeded directly with the
    /// detected state — `propose_state` would otherwise keep the
    /// session at `Processing` for the first CONFIRM_TICKS while it
    /// confirmed its very first proposal.
    pub(crate) fn from_snapshot(snap: DiscoverySnapshot) -> Self {
        // Snapshot fields are already `Sanitised` — the discovery worker
        // is the ingestion boundary, so the type guarantees the invariant
        // by the time this code runs.
        Self {
            id: snap.id,
            cwd: snap.cwd,
            name: snap.name,
            state_changed_at: Instant::now(),
            tty: snap.tty,
            branch: snap.branch,
            cpu_percent: snap.cpu_percent,
            current_tool: snap.current_tool,
            activity: ActivityHistory::new(),
            jsonl_path: snap.jsonl_path,
            renamed: false,
            usage: snap.usage,
            pid: snap.pid,
            jsonl_age_secs: snap.jsonl_age_secs,
            last_notified_state: None,
            state_machine: StateMachine::new(snap.detected_state, CONFIRM_TICKS),
        }
    }

    /// Test-only constructor — production code always goes through
    /// `from_snapshot`. The default initial state is `Processing` so
    /// that propose_state semantics match production behavior.
    #[cfg(test)]
    pub(crate) fn new(id: SessionId, cwd: PathBuf) -> Self {
        let name = Sanitised::new(
            cwd.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".into()),
        );
        Self {
            id,
            cwd,
            name,
            state_changed_at: Instant::now(),
            tty: None,
            branch: None,
            cpu_percent: 0.0,
            current_tool: None,
            activity: ActivityHistory::new(),
            jsonl_path: None,
            renamed: false,
            usage: TokenUsage::default(),
            pid: None,
            jsonl_age_secs: None,
            last_notified_state: None,
            state_machine: StateMachine::new(SessionState::Processing, CONFIRM_TICKS),
        }
    }

    pub(crate) fn state(&self) -> &SessionState {
        self.state_machine.current()
    }

    /// Propose a transition. Returns true once the proposal has
    /// been confirmed `CONFIRM_TICKS` times in a row.
    pub(crate) fn propose_state(&mut self, new_state: SessionState) -> bool {
        if self.state_machine.propose(new_state) {
            self.state_changed_at = Instant::now();
            true
        } else {
            false
        }
    }

    /// Force a transition without confirmations. For when authority
    /// comes from a side channel — e.g. discovery says the session's
    /// file vanished, so we mark it `Vanished` right away.
    pub(crate) fn set_state(&mut self, new_state: SessionState) {
        if self.state_machine.current() != &new_state {
            self.state_machine.set(new_state);
            self.state_changed_at = Instant::now();
        }
    }

    /// Returns the current state as a fresh notification target if it
    /// differs from the last state we notified for, otherwise None.
    /// Callers no longer need to clone the current state speculatively
    /// before checking. One clone on the positive path: the value is
    /// cloned once, then split between the stored last-notified slot
    /// and the returned outgoing-notification value.
    pub(crate) fn take_pending_notification(&mut self) -> Option<SessionState> {
        let current = self.state_machine.current();
        if self.last_notified_state.as_ref() == Some(current) {
            return None;
        }
        let cloned = current.clone();
        self.last_notified_state = Some(cloned.clone());
        Some(cloned)
    }

    /// Apply a fresh observation from the discovery worker. The state
    /// machine, event-history, and notification-dedup fields are not
    /// touched — they belong to the registry, not the snapshot. The
    /// caller passes in any config-supplied name override; the
    /// renamed-by-user case wins over both.
    pub(crate) fn merge_snapshot(&mut self, snap: DiscoverySnapshot, name_override: Option<&str>) {
        self.cpu_percent = snap.cpu_percent;
        self.tty = snap.tty;
        self.branch = snap.branch;
        self.pid = snap.pid;
        self.usage = snap.usage;
        if !self.renamed {
            self.name = name_override
                .map(Sanitised::new)
                .unwrap_or(snap.name);
        }
        if let Some(tool) = snap.current_tool {
            // current_tool sticks past the ToolRunning state so users
            // can see what the session was last doing when it goes
            // idle. Only updated when the snapshot has one to give.
            self.current_tool = Some(tool);
        }
        if self.jsonl_path.is_none() {
            self.jsonl_path = snap.jsonl_path;
        }
        if let Some(age) = snap.jsonl_age_secs {
            self.jsonl_age_secs = Some(age);
        }
    }

    /// Record an event-driven activity tick if the session is doing
    /// work. The "record only when busy" invariant lives here so
    /// callers can't accidentally light up the sparkline during idle
    /// transitions.
    pub(crate) fn record_activity_if_busy(&mut self, state: &SessionState) {
        if matches!(state, SessionState::Processing | SessionState::ToolRunning(_)) {
            self.activity.record_activity();
        }
    }

    /// Time-driven shift of the activity ring. Called once per scan
    /// by the registry so the sparkline keeps decaying when no events
    /// arrive.
    pub(crate) fn shift_activity(&mut self) {
        self.activity.shift_if_needed();
    }

    pub(crate) fn activity_sparkline(&self) -> String {
        self.activity.sparkline()
    }

    pub(crate) fn name(&self) -> &Sanitised {
        &self.name
    }

    /// User-driven rename. Sets the custom name and flags the session
    /// so future discovery snapshots / disambiguation don't overwrite
    /// it. `Sanitised::new` runs the same strip_ansi + control-char
    /// filter, so this ingestion point is covered by the type boundary.
    pub(crate) fn rename_to(&mut self, raw: String) {
        self.name = Sanitised::new(raw);
        self.renamed = true;
    }

    /// Automatic display name. The registry's disambiguation pass
    /// calls this to apply `(N)` suffixes when multiple sessions
    /// share a base; user renames are protected.
    pub(crate) fn set_auto_name(&mut self, name: String) {
        if self.renamed {
            return;
        }
        self.name = Sanitised::new(name);
    }

    pub(crate) fn is_renamed(&self) -> bool {
        self.renamed
    }

    /// One-shot wrapper used by CLI paths and tests; samples its
    /// own `Instant::now()`. Render and sort paths use
    /// `state_duration_at` with a frame-level `now` instead, so all
    /// cards in one frame share a single timestamp.
    pub(crate) fn state_duration(&self) -> std::time::Duration {
        self.state_duration_at(Instant::now())
    }

    pub(crate) fn state_duration_at(&self, now: Instant) -> std::time::Duration {
        // `saturating_duration_since` so a clock that briefly walks
        // backwards (suspend/resume) returns zero rather than an
        // absurd Duration.
        now.saturating_duration_since(self.state_changed_at)
    }

    pub(crate) fn format_duration(&self) -> String {
        self.format_duration_at(Instant::now())
    }

    pub(crate) fn format_duration_at(&self, now: Instant) -> String {
        let secs = self.state_duration_at(now).as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {:02}s", secs / 60, secs % 60)
        } else {
            format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

/// Caller must lowercase `query_lower` once per filter pass so we
/// don't re-allocate per target. ASCII-only case folding —
/// sufficient for filenames and paths.
pub(crate) fn fuzzy_match(query_lower: &str, target: &str) -> bool {
    let mut qi = query_lower.chars().peekable();
    for tc in target.chars() {
        if qi.peek().copied() == Some(tc.to_ascii_lowercase()) {
            qi.next();
        }
    }
    qi.peek().is_none()
}

impl Session {
    pub(crate) fn matches_query(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        fuzzy_match(query_lower, self.name.as_str())
            || self
                .cwd
                .file_name()
                .and_then(|n| n.to_str())
                .map(|dir| fuzzy_match(query_lower, dir))
                .unwrap_or(false)
            || fuzzy_match(query_lower, &self.cwd.to_string_lossy())
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActivityHistory {
    buckets: [bool; 10],
    #[serde(skip)]
    last_update: Instant,
}

impl ActivityHistory {
    pub(crate) fn new() -> Self {
        Self {
            buckets: [false; 10],
            last_update: Instant::now(),
        }
    }

    pub(crate) fn record_activity(&mut self) {
        self.shift_if_needed();
        self.buckets[9] = true;
        self.last_update = Instant::now();
    }

    pub(crate) fn shift_if_needed(&mut self) {
        let elapsed = self.last_update.elapsed().as_secs();
        let shifts = (elapsed / 30).min(10) as usize;
        if shifts > 0 {
            self.buckets.rotate_left(shifts);
            for b in &mut self.buckets[(10 - shifts)..] {
                *b = false;
            }
            self.last_update = Instant::now();
        }
    }

    pub(crate) fn sparkline(&self) -> String {
        // U+2588 FULL BLOCK + U+2581 LOWER ONE-EIGHTH BLOCK reads as
        // a binary timeline on every monospace font. The prior ▓░
        // pair rendered with unevenly-spaced cells on Apple Terminal;
        // the middle-dot interim sat mid-line and broke the timeline's
        // baseline. `▁` sits flush at the baseline at the same width.
        self.buckets
            .iter()
            .map(|&active| if active { '█' } else { '▁' })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn fuzzy_match_exact() {
        assert!(fuzzy_match("my-project", "my-project"));
    }

    #[test]
    fn fuzzy_match_substring() {
        assert!(fuzzy_match("proj", "my-project"));
    }

    #[test]
    fn fuzzy_match_case_insensitive() {
        assert!(fuzzy_match("my-project", "MY-PROJECT"));
    }

    #[test]
    fn fuzzy_match_middle() {
        assert!(fuzzy_match("pro", "my-project"));
    }

    #[test]
    fn fuzzy_match_no_match() {
        assert!(!fuzzy_match("xyz", "my-project"));
    }

    #[test]
    fn fuzzy_match_non_contiguous() {
        assert!(fuzzy_match("myp", "my-project"));
    }

    #[test]
    fn fuzzy_match_wrong_order() {
        assert!(!fuzzy_match("pmy", "my-project"));
    }

    #[test]
    fn fuzzy_match_empty_query() {
        assert!(fuzzy_match("", "anything"));
    }

    #[test]
    fn fuzzy_match_empty_target() {
        assert!(!fuzzy_match("a", ""));
    }

    #[test]
    fn matches_query_fuzzy_by_name() {
        let session = Session::new("id1".into(), PathBuf::from("/home/user/my-long-project"));
        assert!(session.matches_query("mlp"));
    }

    #[test]
    fn matches_query_fuzzy_by_dir() {
        let session = Session::new("id1".into(), PathBuf::from("/home/user/my-long-project"));
        assert!(session.matches_query("mylong"));
    }

    #[test]
    fn matches_query_by_name() {
        let session = Session::new("id1".into(), PathBuf::from("/home/user/my-project"));
        assert!(session.matches_query("my-project"));
        assert!(session.matches_query("my"));
    }

    #[test]
    fn matches_query_by_directory_name() {
        let mut session = Session::new("id1".into(), PathBuf::from("/home/user/my-project"));
        session.name = Sanitised::new("Custom Name");
        assert!(session.matches_query("project"));
    }

    #[test]
    fn matches_query_by_path_partial() {
        let session = Session::new("id1".into(), PathBuf::from("/home/user/my-project"));
        assert!(session.matches_query("home"));
        assert!(session.matches_query("user"));
    }

    #[test]
    fn matches_query_fuzzy() {
        let session = Session::new("id1".into(), PathBuf::from("/home/user/my-long-project-name"));
        assert!(session.matches_query("mlpn"));
    }

    #[test]
    fn matches_query_no_match() {
        let session = Session::new("id1".into(), PathBuf::from("/home/user/my-project"));
        assert!(!session.matches_query("nonexistent"));
    }

    #[test]
    fn matches_query_empty() {
        let session = Session::new("id1".into(), PathBuf::from("/home/user/my-project"));
        assert!(session.matches_query(""));
    }

    #[test]
    fn from_snapshot_seeds_state_machine_with_detected_state() {
        // A session first observed as Idle should report Idle
        // immediately — not Processing for the first 3 ticks while the
        // confirmation counter climbs from the StateMachine default.
        let snap = DiscoverySnapshot {
            id: SessionId::new("id1"),
            cwd: PathBuf::from("/tmp/x"),
            name: Sanitised::new("x"),
            tty: None,
            branch: None,
            cpu_percent: 0.0,
            pid: None,
            detected_state: SessionState::Idle,
            current_tool: None,
            usage: TokenUsage::default(),
            jsonl_path: None,
            jsonl_age_secs: None,
        };
        let session = Session::from_snapshot(snap);
        assert_eq!(session.state(), &SessionState::Idle);
    }

    #[test]
    fn take_pending_notification_dedups() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        s.set_state(SessionState::Idle);
        assert_eq!(s.take_pending_notification(), Some(SessionState::Idle));
        // Same state: no fresh notification on the next call.
        assert_eq!(s.take_pending_notification(), None);
        // Different state: fresh again.
        s.set_state(SessionState::Error);
        assert_eq!(s.take_pending_notification(), Some(SessionState::Error));
    }

    #[test]
    fn rename_to_flags_session_as_renamed() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        assert!(!s.is_renamed());
        s.rename_to("custom".into());
        assert_eq!(s.name, "custom");
        assert!(s.is_renamed());
    }

    #[test]
    fn merge_snapshot_skips_name_when_renamed() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        s.rename_to("custom".into());
        let snap = DiscoverySnapshot {
            id: SessionId::new("id1"),
            cwd: PathBuf::from("/tmp/x"),
            name: Sanitised::new("x"),
            tty: None,
            branch: None,
            cpu_percent: 0.0,
            pid: None,
            detected_state: SessionState::Idle,
            current_tool: None,
            usage: TokenUsage::default(),
            jsonl_path: None,
            jsonl_age_secs: None,
        };
        s.merge_snapshot(snap, None);
        // Rename wins over snapshot name.
        assert_eq!(s.name, "custom");
    }
}
