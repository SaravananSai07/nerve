use super::registry::{SessionRegistry, SortMode};
use super::session::{Session, SessionId};

/// Versioned cache of the registry's sorted+filtered session list.
/// Stores `SessionId`s, not `&Session` references: references can't
/// outlive a `&mut Registry` borrow, but ids can. `get()` / `iter()`
/// resolve ids back to `&Session` via the registry on demand.
#[derive(Default)]
pub struct FilteredView {
    ids: Vec<SessionId>,
    registry_version: u64,
    query_lower: Option<String>,
    primed: bool,
}

impl FilteredView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild only when the registry version or query has changed
    /// since the last refresh. Cheap when nothing has changed (one
    /// comparison, no allocations).
    pub fn refresh(&mut self, registry: &SessionRegistry, query: Option<&str>) {
        let new_query: Option<String> = query
            .filter(|q| !q.is_empty())
            .map(|q| q.to_ascii_lowercase());

        let version = registry.version();
        // SortMode::Age's key is `state_duration()`, which depends on
        // `Instant::now()` — its ordering can change between ticks
        // even when no session mutates. Force a rebuild in that mode.
        let force = matches!(registry.sort_mode(), SortMode::Age);
        if !force
            && self.primed
            && version == self.registry_version
            && new_query == self.query_lower
        {
            return;
        }

        self.ids.clear();
        let filter = new_query.as_deref();
        for session in registry.sorted_sessions() {
            let keep = match filter {
                Some(q) => session.matches_query(q),
                None => true,
            };
            if keep {
                self.ids.push(session.id.clone());
            }
        }

        self.registry_version = version;
        self.query_lower = new_query;
        self.primed = true;
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn get<'r>(&self, registry: &'r SessionRegistry, index: usize) -> Option<&'r Session> {
        let id = self.ids.get(index)?;
        registry.get(id.as_str())
    }

    pub fn iter<'r>(
        &'r self,
        registry: &'r SessionRegistry,
    ) -> impl Iterator<Item = &'r Session> + 'r {
        self.ids
            .iter()
            .filter_map(move |id| registry.get(id.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::session::Session;
    use std::path::PathBuf;

    fn make_session(id: &str, name: &str) -> Session {
        let mut s = Session::new(SessionId::new(id), PathBuf::from(format!("/tmp/{name}")));
        s.set_auto_name(name.to_string());
        s
    }

    #[test]
    fn rebuilds_on_first_refresh() {
        let mut reg = SessionRegistry::new();
        reg.upsert(make_session("a", "alpha"));
        reg.upsert(make_session("b", "bravo"));

        let mut view = FilteredView::new();
        assert_eq!(view.len(), 0); // not primed
        view.refresh(&reg, None);
        assert_eq!(view.len(), 2);
    }

    #[test]
    fn skips_rebuild_when_unchanged() {
        let mut reg = SessionRegistry::new();
        reg.upsert(make_session("a", "alpha"));
        let version_before = reg.version();

        let mut view = FilteredView::new();
        view.refresh(&reg, None);

        // Force a known-bad rebuild by mutating ids out of band, then
        // refresh with same registry version + query. The bad state
        // should survive (proving refresh short-circuited).
        view.ids.clear();
        view.refresh(&reg, None);
        assert_eq!(view.len(), 0);
        assert_eq!(reg.version(), version_before);
    }

    #[test]
    fn rebuilds_when_registry_version_changes() {
        let mut reg = SessionRegistry::new();
        reg.upsert(make_session("a", "alpha"));

        let mut view = FilteredView::new();
        view.refresh(&reg, None);
        assert_eq!(view.len(), 1);

        reg.upsert(make_session("b", "bravo"));
        view.refresh(&reg, None);
        assert_eq!(view.len(), 2);
    }

    #[test]
    fn rebuilds_when_query_changes() {
        let mut reg = SessionRegistry::new();
        reg.upsert(make_session("a", "alpha"));
        reg.upsert(make_session("b", "bravo"));

        let mut view = FilteredView::new();
        view.refresh(&reg, None);
        assert_eq!(view.len(), 2);

        view.refresh(&reg, Some("alp"));
        assert_eq!(view.len(), 1);

        view.refresh(&reg, Some(""));
        assert_eq!(view.len(), 2);
    }

    #[test]
    fn get_and_iter_resolve_via_registry() {
        let mut reg = SessionRegistry::new();
        reg.upsert(make_session("a", "alpha"));
        reg.upsert(make_session("b", "bravo"));

        let mut view = FilteredView::new();
        view.refresh(&reg, None);

        let first = view.get(&reg, 0).expect("first session");
        assert!(first.name() == "alpha" || first.name() == "bravo");

        let names: Vec<_> = view.iter(&reg).map(|s| s.name().to_string()).collect();
        assert_eq!(names.len(), 2);

        assert!(view.get(&reg, 99).is_none());
    }

    #[test]
    fn previous_query_then_no_filter() {
        let mut reg = SessionRegistry::new();
        reg.upsert(make_session("a", "alpha"));
        reg.upsert(make_session("b", "bravo"));

        let mut view = FilteredView::new();
        view.refresh(&reg, Some("alp"));
        assert_eq!(view.len(), 1);
        view.refresh(&reg, None);
        assert_eq!(view.len(), 2);
    }
}
