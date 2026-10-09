use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Open for reading with `O_NOFOLLOW`. Use for files under Claude's
/// directories: a planted final-component symlink can't redirect the
/// read (e.g. a transcript swapped between `exists()` and open).
pub(crate) fn open_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
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
}
