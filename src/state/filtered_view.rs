use super::registry::{SessionRegistry, SortMode};
use super::session::{Session, SessionId};

/// A versioned cache of the registry's sorted+filtered session list.
/// Rebuilt only when the registry's version changes or the search query
/// changes. Eliminates the previous 3-per-frame filter recomputation
/// (closes A1).
///
/// The cache stores `SessionId`s, not `&Session` references — references
/// can't outlive a `&mut Registry` borrow, but ids can. `get()` /
/// `iter()` resolve ids back into `&Session` via the registry on demand.
#[derive(Default)]
pub struct FilteredView {
    /// Ordered list of session ids matching the current sort + filter.
    ids: Vec<SessionId>,
    /// Snapshot of the registry version when `ids` was last built.
    /// A mismatch means the view is stale and must be rebuilt.
    registry_version: u64,
    /// Snapshot of the query (already ASCII-lowercased) used when `ids`
    /// was last built. `None` means "no filter — all sessions".
    query_lower: Option<String>,
    /// True after the first refresh — used to force the initial build.
    primed: bool,
}

impl FilteredView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the cached id list if the registry version or query has
    /// changed since the last refresh. Cheap when nothing has changed
    /// (one comparison, no allocations).
    pub fn refresh(&mut self, registry: &SessionRegistry, query: Option<&str>) {
        let new_query: Option<String> = query
            .filter(|q| !q.is_empty())
            .map(|q| q.to_ascii_lowercase());

        let version = registry.version();
        // SortMode::Age uses `state_duration()` which depends on the
        // current Instant — its result changes every tick even when no
        // session mutates. Force a rebuild in that mode rather than
        // pretending the cached order is still correct.
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

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
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

    #[allow(dead_code)]
    pub fn ids(&self) -> &[SessionId] {
        &self.ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::session::{Session, SessionState};
    use std::path::PathBuf;

    fn make_session(id: &str, name: &str) -> Session {
        let mut s = Session::new(SessionId::new(id), PathBuf::from(format!("/tmp/{name}")));
        s.name = name.to_string();
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
        assert_eq!(view.len(), 2); // empty query treated as no filter
    }

    #[test]
    fn get_and_iter_resolve_via_registry() {
        let mut reg = SessionRegistry::new();
        reg.upsert(make_session("a", "alpha"));
        reg.upsert(make_session("b", "bravo"));

        let mut view = FilteredView::new();
        view.refresh(&reg, None);

        let first = view.get(&reg, 0).expect("first session");
        assert!(first.name == "alpha" || first.name == "bravo");

        let names: Vec<_> = view.iter(&reg).map(|s| s.name.clone()).collect();
        assert_eq!(names.len(), 2);

        // out-of-range
        assert!(view.get(&reg, 99).is_none());

        // SessionState parameter unused but ensures the constructor compiled.
        let _ = SessionState::Idle;
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
