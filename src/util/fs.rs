use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

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

    #[test]
    fn read_capped_propagates_file_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.txt");
        let err = read_capped(&path, 1024).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
