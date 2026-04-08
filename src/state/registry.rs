use std::collections::HashMap;
use super::session::{Session, SessionState};

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
    sessions: HashMap<String, Session>,
    order: Vec<String>,
    sort_mode: SortMode,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            order: Vec::new(),
            sort_mode: SortMode::Stable,
        }
    }

    pub fn upsert(&mut self, session: Session) {
        let id = session.id.clone();
        if !self.sessions.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.sessions.insert(id, session);
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Session> {
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
                sessions.sort_by(|a, b| b.state_duration().cmp(&a.state_duration()));
            }
        }

        sessions
    }

    pub fn cycle_sort(&mut self) {
        self.sort_mode = self.sort_mode.next();
    }

    pub fn sort_mode(&self) -> SortMode {
        self.sort_mode
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn count_by_state(&self) -> StateCount {
        let mut count = StateCount::default();
        for session in self.sessions.values() {
            match session.state {
                SessionState::Processing | SessionState::ToolRunning(_) => count.active += 1,
                SessionState::WaitingForInput => count.waiting += 1,
                SessionState::Idle => count.idle += 1,
                SessionState::Error => count.error += 1,
                SessionState::Stale => count.stale += 1,
            }
        }
        count
    }

    pub fn mark_stale(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.set_state(SessionState::Stale);
        }
    }

    pub fn remove_stale(&mut self, max_age_secs: u64) {
        let active_cwds: std::collections::HashSet<std::path::PathBuf> = self
            .sessions
            .values()
            .filter(|s| s.state != SessionState::Stale)
            .map(|s| s.cwd.clone())
            .collect();

        self.sessions.retain(|_, s| {
            if s.state != SessionState::Stale {
                return true;
            }
            // Evict immediately if an active session covers the same CWD
            if active_cwds.contains(&s.cwd) {
                return false;
            }
            s.state_duration().as_secs() <= max_age_secs
        });
        self.order.retain(|id| self.sessions.contains_key(id));
    }

    pub fn shift_all_activity(&mut self) {
        for session in self.sessions.values_mut() {
            session.activity.shift_if_needed();
        }
    }

    pub fn re_disambiguate_names(&mut self) {
        let reserved: std::collections::HashSet<String> = self
            .sessions
            .values()
            .filter(|s| s.renamed)
            .map(|s| s.name.clone())
            .collect();

        let mut base_to_ids: HashMap<String, Vec<String>> = HashMap::new();
        for (id, session) in &self.sessions {
            if session.renamed {
                continue;
            }
            let base = strip_disambiguation_suffix(&session.name);
            base_to_ids.entry(base).or_default().push(id.clone());
        }

        for (base, mut ids) in base_to_ids {
            if ids.len() <= 1 && !reserved.contains(&base) {
                if let Some(session) = self.sessions.get_mut(&ids[0]) {
                    session.name = base;
                }
                continue;
            }
            ids.sort();
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
                    session.name = candidate;
                }
            }
        }
    }

    pub fn name_taken(&self, name: &str, exclude_id: &str) -> bool {
        self.sessions
            .iter()
            .any(|(id, s)| s.name == name && id != exclude_id)
    }

    pub fn ids(&self) -> &[String] {
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
    pub stale: usize,
}
