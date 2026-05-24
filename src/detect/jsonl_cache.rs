use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::state::session::SessionState;

/// Per-JSONL tail-parse cache. The worker stores the previous
/// `read_tail_state` result keyed by (mtime, len, inode). On the next
/// tick, if those stats are unchanged, the worker can skip the
/// 256 KiB read + serde_json parse and reuse the cached result
/// (closes L4 + L28). The inode component also catches log rotation
/// / truncation where the path stays the same but the file changes
/// underneath (closes the L5 inode-change angle for the state path;
/// the usage path tracks inodes on `Session` directly).
#[derive(Default)]
pub struct JsonlCache {
    entries: HashMap<PathBuf, JsonlEntry>,
}

struct JsonlEntry {
    mtime: SystemTime,
    len: u64,
    inode: u64,
    cached_state: SessionState,
}

impl JsonlCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a cached `SessionState` if the file's mtime + len + inode
    /// have not changed since the last read. None means "you need to
    /// re-read the tail and call `record_state` with the result".
    pub fn cached_state(&self, path: &Path) -> Option<SessionState> {
        let entry = self.entries.get(path)?;
        let (mtime, len, inode) = stat(path)?;
        if entry.inode == inode && entry.mtime == mtime && entry.len == len {
            Some(entry.cached_state.clone())
        } else {
            None
        }
    }

    /// Record the result of a fresh state read along with the file
    /// stats that produced it. Subsequent ticks with identical stats
    /// will short-circuit via `cached_state`.
    pub fn record_state(&mut self, path: &Path, state: SessionState) {
        let Some((mtime, len, inode)) = stat(path) else {
            return;
        };
        self.entries.insert(
            path.to_path_buf(),
            JsonlEntry {
                mtime,
                len,
                inode,
                cached_state: state,
            },
        );
    }

    /// Drop cache entries whose JSONL files no longer reference an
    /// active session. Called once per discovery scan so the cache
    /// doesn't grow unbounded across days of uptime.
    pub fn retain_present<F: Fn(&Path) -> bool>(&mut self, keep: F) {
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
    use std::io::Write;

    fn write_file(path: &Path, contents: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn cached_state_returns_none_for_unseen_path() {
        let cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");
        assert!(cache.cached_state(&path).is_none());
    }

    #[test]
    fn cached_state_returns_value_when_unchanged() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");
        cache.record_state(&path, SessionState::Idle);
        assert_eq!(cache.cached_state(&path), Some(SessionState::Idle));
    }

    #[test]
    fn cached_state_invalidated_on_mtime_change() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");
        cache.record_state(&path, SessionState::Idle);
        // Touch the file with a different content / mtime.
        std::thread::sleep(std::time::Duration::from_millis(15));
        write_file(&path, "different");
        assert_eq!(cache.cached_state(&path), None);
    }

    #[test]
    fn cached_state_invalidated_on_inode_change() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");
        cache.record_state(&path, SessionState::Idle);
        assert_eq!(cache.cached_state(&path), Some(SessionState::Idle));

        // Remove + recreate at same path: new inode invalidates.
        std::fs::remove_file(&path).unwrap();
        write_file(&path, "rotated");
        assert_eq!(cache.cached_state(&path), None);
    }

    #[test]
    fn retain_present_drops_missing_entries() {
        let mut cache = JsonlCache::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        write_file(&path, "hi");
        cache.record_state(&path, SessionState::Idle);
        assert!(cache.cached_state(&path).is_some());

        // Pretend no sessions reference this path anymore.
        cache.retain_present(|_| false);
        assert!(cache.cached_state(&path).is_none());
    }
}
