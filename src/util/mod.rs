pub(crate) mod fs;
pub(crate) mod sanitize;
pub(crate) mod text;

use std::path::Path;
use std::process::{Command, Stdio};

/// Run `f` on a named thread; why it could not start, if it could not.
pub(crate) fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), String> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(drop)
        .map_err(|e| format!("could not start the {name} thread: {e}"))
}

/// This machine's short host name: what a device is called until sync
/// gives it a name.
pub(crate) fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.split('.').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "device".into())
}

/// `macOS`, `Linux`: an OS as people write it.
pub(crate) fn os_name(os: &str) -> String {
    match os {
        "macos" => "macOS".into(),
        "linux" => "Linux".into(),
        "android" => "Android".into(),
        other => other.to_string(),
    }
}

/// Seconds since the Unix epoch; 0 if the clock is before it.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Longest id `is_safe` accepts: a bridge id is a UUID plus a tag, and a
/// cap stops `nerve --focus <huge>` from forcing a large allocation.
const SAFE_MAX_LEN: usize = 160;

/// True if `c` belongs in an id handed to a shell or a script: ASCII
/// alphanumerics plus the delimiters bridge ids use (`: _ - % . $`).
pub(crate) fn is_safe_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '%' | '.' | '$')
}

/// Fit to hand to a shell inside quotes: non-empty, short, safe chars.
pub(crate) fn is_safe(s: &str) -> bool {
    !s.is_empty() && s.len() <= SAFE_MAX_LEN && s.chars().all(is_safe_id_char)
}

/// Run `argv` in `dir`; its stdout, or why it failed.
pub(crate) fn run(dir: &Path, argv: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if !out.status.success() {
        return Err(format!("{} failed: {}", argv.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// `run`, as trimmed text.
pub(crate) fn text(dir: &Path, argv: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&run(dir, argv)?).trim().to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn safe_ids() {
        for ok in ["ghostty:abc-123_def", "tmux:%42", "laptop"] {
            assert!(super::is_safe(ok), "{ok:?}");
        }
        let long = "a".repeat(super::SAFE_MAX_LEN + 1);
        for bad in ["", &long, "a;b", "a`b", "a$(b)", "a|b", "a&b", "a b", "a'b", "a\"b", "a\nb", "café"] {
            assert!(!super::is_safe(bad), "{bad:?}");
        }
    }
}
