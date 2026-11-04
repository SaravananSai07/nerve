use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;

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
}

/// Where a session runs, which decides how nerve can reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionKind {
    /// CLI in a terminal tab / pane: focus via the terminal bridge.
    #[default]
    Terminal,
    /// Embedded in the Claude desktop app: no tty, focus the app.
    Desktop,
    /// `claude --bg` session managed by the daemon: reach it through
    /// `claude attach|stop <job id>`. May have no live process while parked.
    Background,
}

impl SessionKind {
    /// Where to find the session: its tty, or the kind's name for
    /// sessions without one.
    pub(crate) fn location(self, tty: Option<&Sanitised>) -> &str {
        tty.map(Sanitised::as_str).unwrap_or(match self {
            Self::Terminal => "?",
            Self::Desktop => "desktop",
            Self::Background => "background",
        })
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

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, o: Self) {
        self.input_tokens = self.input_tokens.saturating_add(o.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(o.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(o.cache_read_tokens);
        self.cache_creation_tokens = self.cache_creation_tokens.saturating_add(o.cache_creation_tokens);
        self.cost_usd += o.cost_usd;
    }
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

/// Figures Claude Code reports about itself through the statusline hook
/// (see `detect::statusline`). Unlike `TokenUsage` these aren't estimates:
/// the cost reflects Claude Code's own pricing, including org overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct OfficialUsage {
    pub(crate) cost_usd: Option<f64>,
    pub(crate) context_pct: Option<f64>,
    pub(crate) model: Option<Sanitised>,
}

/// Claude's own cost when known, else nerve's estimate. Shared by the
/// detail panel, sync, and `--list` so they can't disagree.
pub(crate) fn effective_cost(official: Option<&OfficialUsage>, usage: &TokenUsage) -> f64 {
    official.and_then(|o| o.cost_usd).unwrap_or(usage.cost_usd)
}

/// Plain snapshot of everything the discovery worker can know about a
/// session at a point in time. Decoupling this from `Session` means
/// the worker never constructs registry-owned state (Debounce,
/// state_changed_at, last_notified_state) — those
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
    /// Seconds since the session last wrote anything: its transcript, or
    /// for a parked background job its state file.
    pub(crate) activity_age_secs: Option<f64>,
    pub(crate) kind: SessionKind,
    /// Claude's own reason for `WaitingForInput`, e.g. "permission prompt".
    pub(crate) waiting_for: Option<Sanitised>,
    pub(crate) official: Option<OfficialUsage>,
    /// Claude config dir the session belongs to (`~/.claude` or
    /// `$CLAUDE_CONFIG_DIR`). The `claude` CLI must run against it.
    pub(crate) claude_root: PathBuf,
    /// True when `detected_state` came from Claude's own status field
    /// rather than transcript heuristics, so it needs no debouncing.
    #[serde(skip)]
    pub(crate) state_authoritative: bool,
}

#[cfg(test)]
impl DiscoverySnapshot {
    pub(crate) fn for_test(id: &str, state: SessionState) -> Self {
        Self {
            id: SessionId::new(id),
            cwd: PathBuf::from("/tmp/x"),
            name: Sanitised::new("x"),
            tty: None,
            branch: None,
            cpu_percent: 0.0,
            pid: None,
            detected_state: state,
            current_tool: None,
            usage: TokenUsage::default(),
            jsonl_path: None,
            activity_age_secs: None,
            kind: SessionKind::Terminal,
            waiting_for: None,
            official: None,
            claude_root: PathBuf::new(),
            state_authoritative: false,
        }
    }
}

// Clone is test-only. A Session carries registry-owned mutable state
// — `debounce`'s confirmation counter, `last_notified_state` for
// notification dedup — so a production clone inserted
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
    /// Last tool the session was running. Sticks past the
    /// `ToolRunning` state into `Idle` so users can see what the
    /// session was last doing when it goes quiet — only `merge_snapshot`
    /// updates it (and only when the snapshot has a tool to give).
    pub(crate) current_tool: Option<Sanitised>,
    pub(crate) jsonl_path: Option<PathBuf>,
    /// User-supplied custom name via the rename dialog. Privately
    /// owned because the legal write path is `rename_to`, which
    /// updates `name` in lockstep — letting external code flip this
    /// without setting the name (or vice versa) breaks the
    /// disambiguation logic in `registry::re_disambiguate_names`.
    renamed: bool,
    pub(crate) usage: TokenUsage,
    pub(crate) pid: Option<u32>,
    pub(crate) activity_age_secs: Option<f64>,
    pub(crate) kind: SessionKind,
    pub(crate) waiting_for: Option<Sanitised>,
    pub(crate) official: Option<OfficialUsage>,
    pub(crate) claude_root: PathBuf,
    last_notified_state: Option<SessionState>,
    debounce: Debounce<SessionState>,
}

// Debug is hand-written rather than derived — the registry-side
// invariants (debounce confirmation counter, last_notified_state)
// are noise in debug output. We surface only the load-bearing
// identity + current state.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("id", &self.id).field("name", &self.name).field("state", self.state()).finish()
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
            current_tool: snap.current_tool,
            jsonl_path: snap.jsonl_path,
            renamed: false,
            usage: snap.usage,
            pid: snap.pid,
            activity_age_secs: snap.activity_age_secs,
            kind: snap.kind,
            waiting_for: snap.waiting_for,
            official: snap.official,
            claude_root: snap.claude_root,
            last_notified_state: None,
            debounce: Debounce::new(snap.detected_state, CONFIRM_TICKS),
        }
    }

    /// Test-only constructor — production code always goes through
    /// `from_snapshot`. The default initial state is `Processing` so
    /// that propose_state semantics match production behavior.
    #[cfg(test)]
    pub(crate) fn new(id: SessionId, cwd: PathBuf) -> Self {
        let name = Sanitised::new(
            cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "unknown".into()),
        );
        Self {
            id,
            cwd,
            name,
            state_changed_at: Instant::now(),
            tty: None,
            branch: None,
            current_tool: None,
            jsonl_path: None,
            renamed: false,
            usage: TokenUsage::default(),
            pid: None,
            activity_age_secs: None,
            kind: SessionKind::Terminal,
            waiting_for: None,
            official: None,
            claude_root: PathBuf::new(),
            last_notified_state: None,
            debounce: Debounce::new(SessionState::Processing, CONFIRM_TICKS),
        }
    }

    pub(crate) fn state(&self) -> &SessionState {
        self.debounce.current()
    }

    /// Propose a transition. Returns true once the proposal has
    /// been confirmed `CONFIRM_TICKS` times in a row.
    fn propose_state(&mut self, new_state: SessionState) -> bool {
        if self.debounce.propose(new_state) {
            self.state_changed_at = Instant::now();
            true
        } else {
            false
        }
    }

    /// Feed one discovery observation. Claude's own status is already
    /// debounced at the source, so an authoritative observation commits
    /// immediately; heuristic ones go through `propose_state`. Returns true
    /// when the visible state changed.
    pub(crate) fn observe_state(&mut self, new_state: SessionState, authoritative: bool) -> bool {
        if !authoritative {
            return self.propose_state(new_state);
        }
        if self.state() == &new_state {
            // Same state: just drop any half-confirmed heuristic proposal.
            self.debounce.set(new_state);
            return false;
        }
        self.set_state(new_state);
        true
    }

    /// Force a transition without confirmations. For when authority
    /// comes from a side channel — e.g. discovery says the session's
    /// file vanished, so we mark it `Vanished` right away.
    pub(crate) fn set_state(&mut self, new_state: SessionState) {
        if self.debounce.current() != &new_state {
            self.debounce.set(new_state);
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
        let current = self.debounce.current();
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
        self.tty = snap.tty;
        self.branch = snap.branch;
        self.pid = snap.pid;
        self.usage = snap.usage;
        self.kind = snap.kind;
        self.claude_root = snap.claude_root;
        self.waiting_for = snap.waiting_for;
        if snap.official.is_some() {
            self.official = snap.official;
        }
        if !self.renamed {
            self.name = name_override.map(Sanitised::new).unwrap_or(snap.name);
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
        if let Some(age) = snap.activity_age_secs {
            self.activity_age_secs = Some(age);
        }
    }

    pub(crate) fn name(&self) -> &Sanitised {
        &self.name
    }

    /// The repo the session works in: see `project_dir`.
    pub(crate) fn project_dir(&self) -> Sanitised {
        project_dir(&self.cwd)
    }

    /// Claude's reported cost when the statusline hook supplies it, else
    /// nerve's transcript-based estimate.
    pub(crate) fn cost_usd(&self) -> f64 {
        effective_cost(self.official.as_ref(), &self.usage)
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

    /// How long the session has been in its current state.
    pub(crate) fn state_duration(&self) -> std::time::Duration {
        // `saturating_duration_since` so a clock that briefly walks
        // backwards (suspend/resume) returns zero rather than an
        // absurd Duration.
        Instant::now().saturating_duration_since(self.state_changed_at)
    }
}

/// A value that only changes after `threshold` consecutive matching
/// proposals, against flicker in heuristic signals (CPU spikes, transient
/// file rewrites).
#[derive(Debug, Clone)]
struct Debounce<T: PartialEq + Clone> {
    current: T,
    pending: Option<T>,
    count: u8,
    threshold: u8,
}

impl<T: PartialEq + Clone> Debounce<T> {
    fn new(initial: T, threshold: u8) -> Self {
        debug_assert!(threshold > 0, "threshold must be at least 1");
        Self { current: initial, pending: None, count: 0, threshold }
    }

    fn current(&self) -> &T {
        &self.current
    }

    /// Propose a transition to `next`. Returns true when the proposal has
    /// been confirmed `threshold` times in a row and the current value has
    /// been updated. A different proposal resets the counter.
    fn propose(&mut self, next: T) -> bool {
        if next == self.current {
            self.pending = None;
            self.count = 0;
            return false;
        }
        if self.pending.as_ref() == Some(&next) {
            self.count += 1;
        } else {
            self.pending = Some(next);
            self.count = 1;
        }
        if self.count >= self.threshold {
            self.current = self.pending.take().expect("pending is set when count > 0");
            self.count = 0;
            return true;
        }
        false
    }

    /// Change at once, dropping any half-confirmed proposal.
    fn set(&mut self, next: T) {
        if self.current != next {
            self.current = next;
        }
        self.pending = None;
        self.count = 0;
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

/// The folder a session's row and notifications name: the repo for
/// Claude's worktrees (`<repo>/.claude/worktrees/<name>`), since the branch
/// already names the worktree, else the cwd's last component.
pub(crate) fn project_dir(cwd: &Path) -> Sanitised {
    let parts: Vec<_> = cwd.components().map(|c| c.as_os_str()).collect();
    let dir = match parts.as_slice() {
        [.., repo, dot_claude, worktrees, _] if *dot_claude == ".claude" && *worktrees == "worktrees" => repo,
        [.., last] => last,
        [] => cwd.as_os_str(),
    };
    Sanitised::new(dir.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn requires_threshold_confirmations() {
        let mut m = Debounce::new("a", 3);
        assert!(!m.propose("b"));
        assert!(!m.propose("b"));
        assert!(m.propose("b"));
        assert_eq!(m.current(), &"b");
    }

    #[test]
    fn different_proposal_resets_counter() {
        let mut m = Debounce::new("a", 3);
        assert!(!m.propose("b"));
        assert!(!m.propose("b"));
        assert!(!m.propose("c"));
        assert!(!m.propose("c"));
        assert!(m.propose("c"));
        assert_eq!(m.current(), &"c");
    }

    #[test]
    fn proposing_current_value_clears_pending() {
        let mut m = Debounce::new("a", 3);
        m.propose("b");
        m.propose("b");
        assert!(!m.propose("a"));
        assert!(!m.propose("b"));
    }

    #[test]
    fn set_forces_immediate_transition() {
        let mut m = Debounce::new("a", 3);
        m.propose("b");
        m.set("c");
        assert_eq!(m.current(), &"c");
        assert!(!m.propose("b"));
    }

    #[test]
    fn threshold_of_one_transitions_immediately() {
        let mut m = Debounce::new("a", 1);
        assert!(m.propose("b"));
        assert_eq!(m.current(), &"b");
    }

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
    fn from_snapshot_seeds_debounce_with_detected_state() {
        // A session first observed as Idle should report Idle
        // immediately — not Processing for the first 3 ticks while the
        // confirmation counter climbs from the Debounce default.
        let snap = DiscoverySnapshot::for_test("id1", SessionState::Idle);
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
    fn name_override_is_sanitised() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        s.merge_snapshot(DiscoverySnapshot::for_test("id1", SessionState::Idle), Some("clean\u{1b}[2Jname"));
        assert_eq!(s.name().as_str(), "cleanname");
    }

    #[test]
    fn merge_snapshot_skips_name_when_renamed() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        s.rename_to("custom".into());
        s.merge_snapshot(DiscoverySnapshot::for_test("id1", SessionState::Idle), None);
        // Rename wins over snapshot name.
        assert_eq!(s.name, "custom");
    }

    #[test]
    fn authoritative_observation_commits_immediately() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        assert!(s.observe_state(SessionState::WaitingForInput, true));
        assert_eq!(s.state(), &SessionState::WaitingForInput);
        assert!(!s.observe_state(SessionState::WaitingForInput, true));
    }

    #[test]
    fn heuristic_observation_is_debounced() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        assert!(!s.observe_state(SessionState::Idle, false));
        assert_eq!(s.state(), &SessionState::Processing);
    }

    // Worktrees name the repo, not the worktree, even when Claude's name
    // starts with the repo's.
    #[test]
    fn project_dir_names_the_repo() {
        let mut s = Session::new("id1".into(), PathBuf::from("/home/u/app"));
        assert_eq!(s.project_dir(), "app");
        s.cwd = PathBuf::from("/home/u/web/.claude/worktrees/design-files-09cbde");
        s.rename_to("web: design files".into());
        assert_eq!(s.project_dir(), "web");
        assert_eq!(project_dir(Path::new("/")), "/");
    }

    #[test]
    fn cost_prefers_official_figure() {
        let mut s = Session::new("id1".into(), PathBuf::from("/tmp/x"));
        s.usage.cost_usd = 3.0;
        assert_eq!(s.cost_usd(), 3.0);
        s.official = Some(OfficialUsage { cost_usd: Some(2.5), ..Default::default() });
        assert_eq!(s.cost_usd(), 2.5);
    }
}
