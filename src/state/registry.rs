use std::collections::HashMap;
use std::time::Instant;

use super::session::{Session, SessionId, SessionState};

#[derive(Clone, Copy, PartialEq)]
pub enum SortMode {
    Stable,
    State,
    Name,
    Age,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Stable => Self::State,
            Self::State => Self::Name,
            Self::Name => Self::Age,
            Self::Age => Self::Stable,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::State => "state",
            Self::Name => "name",
            Self::Age => "age",
        }
    }
}

pub struct SessionRegistry {
    sessions: HashMap<SessionId, Session>,
    order: Vec<SessionId>,
    sort_mode: SortMode,
    /// Monotonic counter incremented whenever a mutation could affect the
    /// rendered/filtered view (insertion, removal, state change, rename,
    /// activity tick, sort change). Consumers like `FilteredView` snapshot
    /// the value and rebuild only when it changes — eliminating the
    /// per-frame filter-rebuild cost when nothing has actually changed
    /// (closes A1).
    version: u64,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            order: Vec::new(),
            sort_mode: SortMode::Stable,
            version: 0,
        }
    }

    #[allow(dead_code)]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Bump the version counter. Public so callers that mutate sessions
    /// via `get_mut` can announce that the rendered view should rebuild.
    pub fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn upsert(&mut self, session: Session) {
        let id = session.id.clone();
        if !self.sessions.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.sessions.insert(id, session);
        self.bump_version();
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Session> {
        // SessionId: Borrow<str>, so HashMap lookups with &str work.
        // Callers should call `bump_version()` if the mutation affects
        // the rendered view (state, name, etc.).
        self.sessions.get_mut(id)
    }

    pub fn sorted_sessions(&self) -> Vec<&Session> {
        let mut sessions: Vec<&Session> = self
            .order
            .iter()
            .filter_map(|id| self.sessions.get(id))
            .collect();

        match self.sort_mode {
            SortMode::Stable => {}
            SortMode::State => {
                sessions.sort_by(|a, b| {
                    a.state
                        .sort_priority()
                        .cmp(&b.state.sort_priority())
                        .then(a.name.cmp(&b.name))
                });
            }
            SortMode::Name => {
                sessions.sort_by(|a, b| a.name.cmp(&b.name));
            }
            SortMode::Age => {
                // Hoist one Instant::now() outside the sort comparator so
                // every comparison sees the same `now` snapshot (closes
                // part of L13/L30). Without this each cmp call sampled
                // an independently-drifting `Instant`.
                let now = Instant::now();
                sessions.sort_by_key(|s| std::cmp::Reverse(s.state_duration_at(now)));
            }
        }

        sessions
    }

    pub fn cycle_sort(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.bump_version();
    }

    pub fn sort_mode(&self) -> SortMode {
        self.sort_mode
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn total_cost_usd(&self) -> f64 {
        self.sessions.values().map(|s| s.usage.cost_usd).sum()
    }

    pub fn count_by_state(&self) -> StateCount {
        let mut count = StateCount::default();
        for session in self.sessions.values() {
            match session.state {
                SessionState::Processing | SessionState::ToolRunning(_) => count.active += 1,
                SessionState::WaitingForInput => count.waiting += 1,
                SessionState::Idle => count.idle += 1,
                SessionState::Error => count.error += 1,
                SessionState::Dormant => count.dormant += 1,
                SessionState::Vanished => count.vanished += 1,
            }
        }
        count
    }

    pub fn mark_stale(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.set_state(SessionState::Vanished);
            self.version = self.version.wrapping_add(1);
        }
    }

    /// Evict sessions that have passed their state-specific retention
    /// window. `Vanished` (process gone) ages out after `vanished_grace_secs`
    /// so the user gets a brief notice that the session ended. `Dormant`
    /// (idle ≥ 48 h but process alive) is kept indefinitely; the user may
    /// still come back to it (A22 — replaces the fused 60-s timeout
    /// that previously treated both cases identically).
    pub fn remove_stale(&mut self, vanished_grace_secs: u64) {
        let active_cwds: std::collections::HashSet<std::path::PathBuf> = self
            .sessions
            .values()
            .filter(|s| !matches!(s.state, SessionState::Vanished))
            .map(|s| s.cwd.clone())
            .collect();

        let before = self.sessions.len();
        self.sessions.retain(|_, s| {
            match s.state {
                SessionState::Vanished => {
                    // Evict immediately if a live session has taken over
                    // the same cwd; otherwise wait out the grace window.
                    if active_cwds.contains(&s.cwd) {
                        return false;
                    }
                    s.state_duration().as_secs() <= vanished_grace_secs
                }
                // Dormant sessions stay around forever — the user might
                // resume them, and the process is still alive so the
                // tab/pane info is valid.
                SessionState::Dormant => true,
                _ => true,
            }
        });
        if self.sessions.len() != before {
            self.order.retain(|id| self.sessions.contains_key(id));
            self.bump_version();
        }
    }

    pub fn shift_all_activity(&mut self) {
        for session in self.sessions.values_mut() {
            session.activity.shift_if_needed();
        }
        // No version bump: the cached `FilteredView` stores only session
        // IDs, not their content. Renderers iterate the cache and
        // resolve `&Session` references on demand, which means activity
        // sparkline changes flow through automatically without needing
        // the filter cache to invalidate. Bumping here would defeat the
        // steady-state win.
    }

    pub fn re_disambiguate_names(&mut self) {
        let reserved: std::collections::HashSet<String> = self
            .sessions
            .values()
            .filter(|s| s.renamed)
            .map(|s| s.name.clone())
            .collect();

        let mut base_to_ids: HashMap<String, Vec<SessionId>> = HashMap::new();
        for (id, session) in &self.sessions {
            if session.renamed {
                continue;
            }
            let base = strip_disambiguation_suffix(&session.name);
            base_to_ids.entry(base).or_default().push(id.clone());
        }

        let mut any_renamed = false;
        for (base, mut ids) in base_to_ids {
            if ids.len() <= 1 && !reserved.contains(&base) {
                if let Some(session) = self.sessions.get_mut(&ids[0]) {
                    if session.name != base {
                        session.name = base;
                        any_renamed = true;
                    }
                }
                continue;
            }
            ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            let mut n = 1;
            for id in &ids {
                let candidate = loop {
                    let name = format!("{} ({})", base, n);
                    n += 1;
                    if !reserved.contains(&name) {
                        break name;
                    }
                };
                if let Some(session) = self.sessions.get_mut(id) {
                    if session.name != candidate {
                        session.name = candidate;
                        any_renamed = true;
                    }
                }
            }
        }
        if any_renamed {
            self.bump_version();
        }
    }

    pub fn name_taken(&self, name: &str, exclude_id: &str) -> bool {
        self.sessions
            .iter()
            .any(|(id, s)| s.name == name && id.as_str() != exclude_id)
    }

    pub fn ids(&self) -> &[SessionId] {
        &self.order
    }
}

fn strip_disambiguation_suffix(name: &str) -> String {
    if let Some(idx) = name.rfind(" (") {
        if name.ends_with(')') {
            let inner = &name[idx + 2..name.len() - 1];
            if inner.chars().all(|c| c.is_ascii_digit()) {
                return name[..idx].to_string();
            }
        }
    }
    name.to_string()
}

#[derive(Default)]
pub struct StateCount {
    pub active: usize,
    pub waiting: usize,
    pub idle: usize,
    pub error: usize,
    pub dormant: usize,
    pub vanished: usize,
}
