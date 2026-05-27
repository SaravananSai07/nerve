use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::state::session::SessionState;
use crate::state::token_usage::TokenUsage;

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
    mtime: SystemTime,
    len: u64,
    inode: u64,
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
        let Some((mtime, len, inode)) = stat(path) else {
            return (SessionState::Idle, TokenUsage::default());
        };

        if let Some(entry) = self.entries.get(path) {
            if entry.inode == inode && entry.mtime == mtime && entry.len == len {
                return (entry.cached_state.clone(), entry.cumulative_usage.clone());
            }
        }

        let new_state = parse_state(path);
        let (cumulative_usage, new_offset) = match self.entries.get(path) {
            Some(prior) if prior.inode == inode => {
                // File grew under the same inode: parse the new tail
                // from where we left off and merge into the cumulative
                // total so we never re-count earlier rows.
                let (delta, off) = parse_usage_from(path, prior.last_offset);
                let mut merged = prior.cumulative_usage.clone();
                merged.input_tokens = merged.input_tokens.saturating_add(delta.input_tokens);
                merged.output_tokens = merged.output_tokens.saturating_add(delta.output_tokens);
                merged.cache_read_tokens = merged
                    .cache_read_tokens
                    .saturating_add(delta.cache_read_tokens);
                merged.cache_creation_tokens = merged
                    .cache_creation_tokens
                    .saturating_add(delta.cache_creation_tokens);
                merged.cost_usd += delta.cost_usd;
                (merged, off)
            }
            _ => parse_usage_from(path, 0),
        };

        self.entries.insert(
            path.to_path_buf(),
            JsonlEntry {
                mtime,
                len,
                inode,
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

fn stat(path: &Path) -> Option<(SystemTime, u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?;
    let len = m.len();
    let inode = inode_of(&m);
    Some((mtime, len, inode))
}

#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> u64 {
    0
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
            |_, _| {
                (
                    TokenUsage {
                        input_tokens: 100,
                        ..Default::default()
                    },
                    0,
                )
            },
        );
        assert_eq!(u1.input_tokens, 100);

        std::fs::remove_file(&path).unwrap();
        write_file(&path, "second");

        let (_s, u2) = cache.read_or_refresh(
            &path,
            |_| SessionState::Idle,
            |_, _| {
                (
                    TokenUsage {
                        input_tokens: 7,
                        ..Default::default()
                    },
                    0,
                )
            },
        );
        assert_eq!(u2.input_tokens, 7); // replaced, not 107
    }

    #[test]
    fn retain_present_drops_missing_entries() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");

        cache.read_or_refresh(
            &path,
            |_| SessionState::Idle,
            |_, _| (TokenUsage::default(), 0),
        );
        assert!(cache.entries.contains_key(&path));

        cache.retain_present(|_| false);
        assert!(!cache.entries.contains_key(&path));
    }
}
