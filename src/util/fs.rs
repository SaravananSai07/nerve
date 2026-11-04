use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Open for reading with `O_NOFOLLOW`. Use for files under Claude's
/// directories: a planted final-component symlink can't redirect the
/// read (e.g. a transcript swapped between `exists()` and open).
pub(crate) fn open_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).custom_flags(nix::libc::O_NOFOLLOW).open(path)
}

/// `read_capped` for files under Claude's directories.
pub(crate) fn read_capped_nofollow(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut buf = String::new();
    open_nofollow(path)?.take(max_bytes).read_to_string(&mut buf)?;
    Ok(buf)
}

/// Last `max_bytes` of a line-oriented file, `O_NOFOLLOW`. Decoded
/// lossily: a window that starts inside a multi-byte character must not
/// fail the whole read. When the window starts mid-file the first,
/// partial line is dropped.
pub(crate) fn read_tail_nofollow(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut file = open_nofollow(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    Ok(match (start > 0, text.split_once('\n')) {
        (true, Some((_, rest))) => rest.to_string(),
        (true, None) => String::new(),
        (false, _) => text.into_owned(),
    })
}

/// What a file looked like when last read: caches compare it to decide
/// whether to re-parse. mtime alone misses a rewrite within the
/// filesystem's timestamp resolution; the length and inode catch it, and
/// a replaced file at the same path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileKey {
    mtime: SystemTime,
    pub(crate) len: u64,
    pub(crate) inode: u64,
}

impl FileKey {
    pub(crate) fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(Self { mtime: m.modified().ok()?, len: m.len(), inode: m.ino() })
    }
}

/// Read and parse a TOML file, capped at `max_bytes`. A missing file is
/// `Ok(None)`; an unreadable or malformed one is an error message the
/// caller can show.
pub(crate) fn load_toml<T: serde::de::DeserializeOwned>(path: &Path, max_bytes: u64) -> Result<Option<T>, String> {
    let text = match read_capped(path, max_bytes) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    toml::from_str(&text).map(Some).map_err(|e| format!("parse error at {} ({e})", path.display()))
}

/// Read up to `max_bytes` from `path`. Bytes beyond the cap are silently
/// truncated — callers either re-parse and reject malformed input
/// (TOML/JSON parsers do) or work on small, well-typed contents (a PID
/// string, a git ref). The cap exists to bound memory if a sibling
/// process, dotfile-sync glitch, or local mis-write plants a huge file
/// where a small one was expected.
pub(crate) fn read_capped(path: &Path, max_bytes: u64) -> io::Result<String> {
    let file = File::open(path)?;
    let mut buf = String::new();
    file.take(max_bytes).read_to_string(&mut buf)?;
    Ok(buf)
}

/// Regular files under `dir` as (path relative to it, full path). Symlinks are skipped.
pub(crate) fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    walk_skipping(dir, rel, &|_| false, out)
}

/// `walk`, not entering directories `skip` names.
pub(crate) fn walk_skipping(dir: &Path, rel: &str, skip: &dyn Fn(&str) -> bool, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        match e.file_type() {
            Ok(t) if t.is_dir() && !skip(&name) => walk_skipping(&e.path(), &r, skip, out),
            Ok(t) if t.is_file() => out.push((r, e.path())),
            _ => {}
        }
    }
}

/// A relative path from the wire that stays inside its directory.
pub(crate) fn safe_rel(rel: &str) -> Option<&str> {
    let ok = !rel.is_empty()
        && !rel.starts_with('/')
        && Path::new(rel).components().all(|c| matches!(c, std::path::Component::Normal(_)));
    ok.then_some(rel)
}

/// A scratch directory that goes when dropped.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fresh scratch directory, removed when dropped.
pub(crate) fn tempdir() -> Result<TempDir, String> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "nerve-{}-{}-{}",
        std::process::id(),
        crate::util::now_secs(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    Ok(TempDir(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_capped_returns_full_contents_when_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("small.txt");
        let data = "a".repeat(100);
        std::fs::write(&path, &data).unwrap();
        let result = read_capped(&path, 1024).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn read_capped_truncates_silently_when_over_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("large.txt");
        let data = "b".repeat(200);
        std::fs::write(&path, &data).unwrap();
        let result = read_capped(&path, 100).unwrap();
        assert_eq!(result.len(), 100);
    }

    // A tail window that starts inside a multi-byte character used to make
    // `read_to_string` fail, blanking the whole log preview.
    #[test]
    fn tail_survives_window_starting_mid_character() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        std::fs::write(&path, "first line\nnaïve ünïcödé\nlast line\n").unwrap();
        // 12 bytes from the end lands inside the two-byte "é".
        let tail = read_tail_nofollow(&path, 12).unwrap();
        assert_eq!(tail, "last line\n");
        assert_eq!(read_tail_nofollow(&path, 1_000).unwrap().lines().count(), 3);
    }

    #[test]
    fn nofollow_refuses_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real.txt");
        std::fs::write(&target, "secret").unwrap();
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_capped_nofollow(&link, 100).is_err());
    }

    #[test]
    fn read_capped_propagates_file_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.txt");
        let err = read_capped(&path, 1024).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn load_toml_tells_missing_from_broken() {
        #[derive(serde::Deserialize, Debug)]
        struct T {
            n: u32,
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.toml");
        assert!(load_toml::<T>(&path, 100).unwrap().is_none());
        std::fs::write(&path, "n = 4").unwrap();
        assert_eq!(load_toml::<T>(&path, 100).unwrap().map(|t| t.n), Some(4));
        std::fs::write(&path, "[bad @@").unwrap();
        assert!(load_toml::<T>(&path, 100).unwrap_err().contains("parse error"));
    }
}
