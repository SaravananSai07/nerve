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

    pub fn remove(&mut self, id: &str) -> Option<Session> {
        self.order.retain(|i| i != id);
        self.sessions.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
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

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn count_by_state(&self) -> StateCount {
        let mut count = StateCount::default();
        for session in self.sessions.values() {
            match session.state {
                SessionState::Processing | SessionState::ToolRunning(_) => count.active += 1,
                SessionState::WaitingForInput | SessionState::WaitingForPermission => {
                    count.waiting += 1
                }
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
        let to_remove: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.state == SessionState::Stale && s.state_duration().as_secs() > max_age_secs
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in to_remove {
            self.order.retain(|i| i != &id);
            self.sessions.remove(&id);
        }
    }

    pub fn ids(&self) -> Vec<String> {
        self.order.clone()
    }
}

#[derive(Default)]
pub struct StateCount {
    pub active: usize,
    pub waiting: usize,
    pub idle: usize,
    pub error: usize,
    pub stale: usize,
}
