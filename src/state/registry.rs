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
    /// Monotonic counter bumped whenever a mutation could change the
    /// rendered/filtered view. `FilteredView` snapshots this and
    /// rebuilds only on mismatch, so a steady-state idle tick pays
    /// zero filter cost.
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

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Announce that the rendered view should rebuild. Callers
    /// mutating through `get_mut` use this to flag changes the
    /// registry can't otherwise detect.
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
        // Callers must call `bump_version()` after a mutation that
        // affects the rendered view (state, name, etc.); the registry
        // can't detect that on its own.
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
                    a.state()
                        .sort_priority()
                        .cmp(&b.state().sort_priority())
                        .then(a.name.cmp(&b.name))
                });
            }
            SortMode::Name => {
                sessions.sort_by(|a, b| a.name.cmp(&b.name));
            }
            SortMode::Age => {
                // One `Instant::now()` outside the comparator so all
                // pairwise compares see the same `now` snapshot —
                // otherwise the sort could observe inconsistent
                // orderings under heavy load.
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
            match session.state() {
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

    /// State-specific retention. `Vanished` (process gone) ages out
    /// after `vanished_grace_secs` so the user gets a brief notice
    /// that the session ended. `Dormant` (process alive, idle ≥ 48 h)
    /// is kept indefinitely — the user may resume it and the tab/
    /// pane info is still valid.
    pub fn remove_stale(&mut self, vanished_grace_secs: u64) {
        let active_cwds: std::collections::HashSet<std::path::PathBuf> = self
            .sessions
            .values()
            .filter(|s| !matches!(s.state(), SessionState::Vanished))
            .map(|s| s.cwd.clone())
            .collect();

        let before = self.sessions.len();
        self.sessions.retain(|_, s| {
            match s.state() {
                SessionState::Vanished => {
                    // Evict early when a live session has reclaimed
                    // the cwd; otherwise wait out the grace window.
                    if active_cwds.contains(&s.cwd) {
                        return false;
                    }
                    s.state_duration().as_secs() <= vanished_grace_secs
                }
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
        // No version bump: `FilteredView` stores only ids, not
        // content. Renderers resolve `&Session` on demand, so
        // sparkline changes flow through without invalidating the
        // filter — bumping would defeat the steady-state win.
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
