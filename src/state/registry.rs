use std::collections::{HashMap, HashSet};

use super::session::{DiscoverySnapshot, Session, SessionId, SessionState};
use crate::util::sanitize::Sanitised;

/// A committed state change worth a notification.
pub(crate) struct PendingNotification {
    pub(crate) id: String,
    pub(crate) project: Sanitised,
    pub(crate) name: Sanitised,
    pub(crate) state: SessionState,
    pub(crate) waiting_for: Option<Sanitised>,
}

pub(crate) struct SessionRegistry {
    sessions: HashMap<SessionId, Session>,
    /// Insertion order, so the list doesn't reshuffle between scans.
    order: Vec<SessionId>,
}

impl SessionRegistry {
    pub(crate) fn new() -> Self {
        Self { sessions: HashMap::new(), order: Vec::new() }
    }

    /// Apply one discovery scan: sessions it no longer lists vanish, new
    /// ones join, known ones take the new observation. `names` maps a cwd
    /// to the name config.toml gives its sessions. Returns the
    /// notifications the committed state changes call for.
    pub(crate) fn apply(
        &mut self,
        scan: Vec<DiscoverySnapshot>,
        names: &HashMap<String, String>,
    ) -> Vec<PendingNotification> {
        let live: HashSet<&str> = scan.iter().map(|s| s.id.as_str()).collect();
        let gone: Vec<SessionId> = self.order.iter().filter(|id| !live.contains(id.as_str())).cloned().collect();
        let mut membership_changed = false;
        for id in gone {
            membership_changed |= self.mark_stale(id.as_str());
        }

        let mut pending = Vec::new();
        for mut snap in scan {
            let (state, authoritative) = (snap.detected_state.clone(), snap.state_authoritative);
            let name = names.get(snap.cwd.to_string_lossy().as_ref()).map(String::as_str);
            let Some(existing) = self.sessions.get_mut(snap.id.as_str()) else {
                if let Some(name) = name {
                    snap.name = Sanitised::new(name);
                }
                // Seeded with the detected state: no Processing flash
                // while the debounce climbs from a default.
                self.upsert(Session::from_snapshot(snap));
                membership_changed = true;
                continue;
            };
            existing.merge_snapshot(snap, name);
            if existing.observe_state(state, authoritative) {
                if let Some(state) = existing.take_pending_notification() {
                    pending.push(PendingNotification {
                        id: existing.id.as_str().to_string(),
                        project: existing.project_dir(),
                        name: existing.name().clone(),
                        waiting_for: existing.waiting_for.clone(),
                        state,
                    });
                }
            }
        }
        // Only a change of membership can introduce a name clash.
        if membership_changed {
            self.re_disambiguate_names();
        }
        pending
    }

    fn upsert(&mut self, session: Session) {
        let id = session.id.clone();
        if !self.sessions.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.sessions.insert(id, session);
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(id)
    }

    /// Every session, in the order discovery first saw them.
    pub(crate) fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.order.iter().filter_map(|id| self.sessions.get(id))
    }

    /// Returns true iff the session existed and was not already `Vanished`.
    /// Callers use the return value to gate re-disambiguation — a
    /// session lingering in the grace window shouldn't churn it every tick.
    fn mark_stale(&mut self, id: &str) -> bool {
        let Some(session) = self.sessions.get_mut(id) else {
            return false;
        };
        if matches!(session.state(), SessionState::Vanished) {
            return false;
        }
        session.set_state(SessionState::Vanished);
        true
    }

    /// State-specific retention. `Vanished` (process gone) ages out
    /// after `vanished_grace_secs` so the user gets a brief notice
    /// that the session ended. `Dormant` (process alive, idle ≥ 48 h)
    /// is kept indefinitely — the user may resume it and the tab/
    /// pane info is still valid.
    pub(crate) fn remove_stale(&mut self, vanished_grace_secs: u64) {
        let active_cwds: HashSet<std::path::PathBuf> = self
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
        }
    }

    fn re_disambiguate_names(&mut self) {
        let reserved: HashSet<String> =
            self.sessions.values().filter(|s| s.is_renamed()).map(|s| s.name().to_string()).collect();

        let mut base_to_ids: HashMap<String, Vec<SessionId>> = HashMap::new();
        for (id, session) in &self.sessions {
            if session.is_renamed() {
                continue;
            }
            let base = strip_disambiguation_suffix(session.name().as_str());
            base_to_ids.entry(base).or_default().push(id.clone());
        }

        for (base, mut ids) in base_to_ids {
            if ids.len() <= 1 && !reserved.contains(&base) {
                if let Some(session) = self.sessions.get_mut(&ids[0]) {
                    if session.name().as_str() != base {
                        session.set_auto_name(base);
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
                    if session.name().as_str() != candidate {
                        session.set_auto_name(candidate);
                    }
                }
            }
        }
    }

    pub(crate) fn name_taken(&self, name: &str, exclude_id: &str) -> bool {
        self.sessions.iter().any(|(id, s)| s.name() == name && id.as_str() != exclude_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, cwd: &str, state: SessionState) -> DiscoverySnapshot {
        let mut s = DiscoverySnapshot::for_test(id, state);
        s.cwd = cwd.into();
        s.name = Sanitised::new(cwd.rsplit('/').next().unwrap_or(""));
        s.state_authoritative = true;
        s
    }

    #[test]
    fn apply_tracks_membership_names_and_notifications() {
        let mut r = SessionRegistry::new();
        let names = HashMap::from([("/w/api".to_string(), "Backend".to_string())]);
        let first = r.apply(
            vec![
                snap("a", "/w/app", SessionState::Processing),
                snap("b", "/x/app", SessionState::Processing),
                snap("c", "/w/api", SessionState::Idle),
            ],
            &names,
        );
        assert!(first.is_empty(), "joining is not a change of state");
        let named: Vec<_> = r.sessions().map(|s| s.name().to_string()).collect();
        assert_eq!(named, ["app (1)", "app (2)", "Backend"]);

        let next = r.apply(
            vec![snap("a", "/w/app", SessionState::WaitingForInput), snap("c", "/w/api", SessionState::Idle)],
            &names,
        );
        assert_eq!(next.len(), 1);
        assert_eq!((next[0].id.as_str(), &next[0].state), ("a", &SessionState::WaitingForInput));
        assert_eq!(next[0].project, "app");
        assert_eq!(r.get("b").map(|s| s.state()), Some(&SessionState::Vanished));
    }
}
