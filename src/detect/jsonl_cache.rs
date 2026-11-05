use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::state::session::SessionState;
use crate::state::session::TokenUsage;
use crate::util::fs::FileKey;

/// Per-JSONL hot-path cache. Stores the tail-parse state result and the
/// cumulative `TokenUsage` keyed by `(mtime, len, inode)`. On a tick
/// where the file has not changed the worker reuses both without opening
/// the file — that read + parse was the dominant per-tick cost on idle
/// sessions. The inode component also catches log rotation / truncation
/// at the same path so usage isn't double-counted across rotations.
#[derive(Default)]
pub(crate) struct JsonlCache {
    entries: HashMap<PathBuf, JsonlEntry>,
}

struct JsonlEntry {
    key: FileKey,
    cached_state: SessionState,
    cumulative_usage: TokenUsage,
    /// Byte offset reached on the last `parse_token_usage` walk. The
    /// next walk picks up from here when the inode is unchanged.
    last_offset: u64,
}

impl JsonlCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the cached `(state, usage)` for `path`, falling back to
    /// a fresh read + parse on a stat mismatch and updating the cache
    /// in place. State is the tail-parse result — `refine_with_runtime`
    /// still applies on top. Usage is the cumulative total to the
    /// file's current length.
    ///
    /// `parse_state` and `parse_usage_from` are passed in so the cache
    /// can live in the `detect` layer without depending on the JSONL
    /// parser concretely; the only call site is `load_session`, which
    /// supplies the real functions.
    pub(crate) fn read_or_refresh<S, U>(
        &mut self,
        path: &Path,
        parse_state: S,
        parse_usage_from: U,
    ) -> (SessionState, TokenUsage)
    where
        S: FnOnce(&Path) -> SessionState,
        U: FnOnce(&Path, u64) -> (TokenUsage, u64),
    {
        let Some(key) = FileKey::of(path) else {
            return (SessionState::Idle, TokenUsage::default());
        };

        if let Some(entry) = self.entries.get(path) {
            if entry.key == key {
                return (entry.cached_state.clone(), entry.cumulative_usage.clone());
            }
        }

        let new_state = parse_state(path);
        let (cumulative_usage, new_offset) = match self.entries.get(path) {
            Some(prior) if prior.key.inode == key.inode && key.len >= prior.key.len => {
                // File grew under the same inode: parse the new tail
                // from where we left off and merge into the cumulative
                // total so we never re-count earlier rows.
                let (delta, off) = parse_usage_from(path, prior.last_offset);
                let mut merged = prior.cumulative_usage.clone();
                merged += delta;
                (merged, off)
            }
            // New, replaced, or shorter under the same inode (truncated):
            // re-parse whole rather than merge into the old total.
            _ => parse_usage_from(path, 0),
        };

        self.entries.insert(
            path.to_path_buf(),
            JsonlEntry {
                key,
                cached_state: new_state.clone(),
                cumulative_usage: cumulative_usage.clone(),
                last_offset: new_offset,
            },
        );
        (new_state, cumulative_usage)
    }

    /// Drop cache entries whose JSONL files no longer reference an
    /// active session. Called once per discovery scan so the cache
    /// doesn't grow unbounded across days of uptime.
    pub(crate) fn retain_present<F: Fn(&Path) -> bool>(&mut self, keep: F) {
        self.entries.retain(|path, _| keep(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Write;

    fn write_file(path: &Path, contents: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn first_read_invokes_parsers_and_subsequent_short_circuits() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");

        let state_calls = Cell::new(0);
        let usage_calls = Cell::new(0);
        let parse_state = |_: &Path| {
            state_calls.set(state_calls.get() + 1);
            SessionState::Idle
        };
        let parse_usage = |_: &Path, _: u64| {
            usage_calls.set(usage_calls.get() + 1);
            (TokenUsage::default(), 0)
        };

        let (s, _u) = cache.read_or_refresh(&path, parse_state, parse_usage);
        assert_eq!(s, SessionState::Idle);
        assert_eq!(state_calls.get(), 1);
        assert_eq!(usage_calls.get(), 1);

        cache.read_or_refresh(&path, parse_state, parse_usage);
        assert_eq!(state_calls.get(), 1);
        assert_eq!(usage_calls.get(), 1);
    }

    #[test]
    fn mtime_change_invalidates_cache() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");

        let state_calls = Cell::new(0);
        let usage_calls = Cell::new(0);
        let parse_state = |_: &Path| {
            state_calls.set(state_calls.get() + 1);
            SessionState::Idle
        };
        let parse_usage = |_: &Path, _: u64| {
            usage_calls.set(usage_calls.get() + 1);
            (TokenUsage::default(), 0)
        };

        cache.read_or_refresh(&path, parse_state, parse_usage);
        std::thread::sleep(std::time::Duration::from_millis(15));
        write_file(&path, "different");
        cache.read_or_refresh(&path, parse_state, parse_usage);

        assert_eq!(state_calls.get(), 2);
        assert_eq!(usage_calls.get(), 2);
    }

    #[test]
    fn inode_change_resets_offset_to_zero() {
        // After a rotation the cumulative usage must reflect the new
        // file from byte 0, not continue from the prior offset which
        // would silently double-count or miss tokens.
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "first");

        let (_s, u1) = cache.read_or_refresh(
            &path,
            |_| SessionState::Idle,
            |_, _| (TokenUsage { input_tokens: 100, ..Default::default() }, 0),
        );
        assert_eq!(u1.input_tokens, 100);

        // Move the old file aside instead of deleting it: Linux readily
        // reuses a freed inode number for the next file, which would make
        // the replacement indistinguishable by (inode, len, mtime).
        std::fs::rename(&path, path.with_extension("old")).unwrap();
        write_file(&path, "second");

        let (_s, u2) = cache.read_or_refresh(
            &path,
            |_| SessionState::Idle,
            |_, _| (TokenUsage { input_tokens: 7, ..Default::default() }, 0),
        );
        assert_eq!(u2.input_tokens, 7); // replaced, not 107
    }

    #[test]
    fn truncation_under_the_same_inode_reparses_instead_of_merging() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "a long first body");
        let usage = |n| move |_: &Path, from: u64| (TokenUsage { input_tokens: n, ..Default::default() }, from + 1);

        let (_s, u1) = cache.read_or_refresh(&path, |_| SessionState::Idle, usage(100));
        assert_eq!(u1.input_tokens, 100);

        // `File::create` truncates in place, keeping the inode.
        write_file(&path, "short");
        let (_s, u2) = cache.read_or_refresh(
            &path,
            |_| SessionState::Idle,
            |_, from| {
                assert_eq!(from, 0);
                (TokenUsage { input_tokens: 7, ..Default::default() }, 5)
            },
        );
        assert_eq!(u2.input_tokens, 7);
    }

    #[test]
    fn retain_present_drops_missing_entries() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");

        cache.read_or_refresh(&path, |_| SessionState::Idle, |_, _| (TokenUsage::default(), 0));
        assert!(cache.entries.contains_key(&path));

        cache.retain_present(|_| false);
        assert!(!cache.entries.contains_key(&path));
    }
}
