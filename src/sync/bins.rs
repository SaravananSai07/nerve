//! Finding executables from a service. launchd and systemd user units
//! start with a minimal PATH (`/usr/bin:/bin`), so tools that per-user
//! installers put in `~/.local/bin` (Claude Code), `~/.cargo/bin` or
//! Homebrew are not found by name. Setup writes a fuller PATH into the
//! unit, and lookups fall back to the same directories when PATH misses.

use std::path::{Path, PathBuf};

/// Per-user and Homebrew bin directories worth adding, in this order.
fn candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = home {
        v.push(h.join(".local/bin"));
        v.push(h.join(".cargo/bin"));
    }
    v.push(PathBuf::from("/opt/homebrew/bin"));
    v.push(PathBuf::from("/usr/local/bin"));
    v
}

/// The extra directories that exist on this device.
pub(crate) fn extra_dirs() -> Vec<PathBuf> {
    candidates(dirs::home_dir().as_deref()).into_iter().filter(|d| d.is_dir()).collect()
}

/// `current` with every directory in `extra` that it lacks appended, so
/// the user's own order wins and nothing is listed twice.
pub(crate) fn service_path(current: &str, extra: &[PathBuf]) -> String {
    let mut parts: Vec<String> = current.split(':').filter(|p| !p.is_empty()).map(String::from).collect();
    for base in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if !parts.iter().any(|p| p == base) {
            parts.push(base.to_string());
        }
    }
    for d in extra {
        let d = d.to_string_lossy().into_owned();
        if !parts.contains(&d) {
            parts.push(d);
        }
    }
    parts.join(":")
}

fn in_dirs(bin: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter().map(|d| d.join(bin)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `bin` on PATH, else in the extra directories. A name with a slash is
/// taken as a path. The error names every place that was searched.
pub(crate) fn find(bin: &str) -> Result<PathBuf, String> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return if is_executable(&p) { Ok(p) } else { Err(format!("{bin} is not an executable file")) };
    }
    let path: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(p) = in_dirs(bin, path.clone()) {
        return Ok(p);
    }
    let extra = candidates(dirs::home_dir().as_deref());
    if let Some(p) = in_dirs(bin, extra.clone()) {
        return Ok(p);
    }
    let looked: Vec<String> = path.iter().chain(&extra).map(|d| d.display().to_string()).collect();
    Err(format!("{bin} not found; looked in PATH and the usual install dirs: {}", looked.join(":")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_path_keeps_order_and_adds_missing_dirs_once() {
        let extra = [PathBuf::from("/home/user/.local/bin"), PathBuf::from("/usr/bin")];
        assert_eq!(service_path("/usr/bin:/bin", &extra), "/usr/bin:/bin:/usr/sbin:/sbin:/home/user/.local/bin");
        assert_eq!(service_path("", &[]), "/usr/bin:/bin:/usr/sbin:/sbin");
    }

    #[test]
    fn candidates_cover_user_and_homebrew_dirs() {
        let c = candidates(Some(Path::new("/home/user")));
        assert_eq!(c[0], PathBuf::from("/home/user/.local/bin"));
        assert!(c.contains(&PathBuf::from("/home/user/.cargo/bin")) && c.contains(&PathBuf::from("/opt/homebrew/bin")));
    }

    #[test]
    fn find_resolves_paths_and_names_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(find(&exe.to_string_lossy()).unwrap(), exe);
        assert!(find("sh").is_ok());
        let e = find("nerve-no-such-tool-xyz").unwrap_err();
        assert!(e.contains(".local/bin") && e.contains("not found"), "{e}");
    }
}
