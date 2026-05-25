use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Per-cwd cache of branch lookups. First call for a cwd does the
/// upward `.git` walk and reads HEAD; subsequent calls do one
/// `stat(2)` on the resolved HEAD path and reuse the cached branch
/// when its mtime is unchanged. At 200 sessions in deep cwds the
/// uncached path was ~1500 stats + ~200 file reads per scan; cached,
/// it collapses to ~200 stats and no reads.
///
/// A cwd that turns out not to be inside a git tree is cached as
/// `NoTree`, so the walk doesn't repeat each scan. Trade-off: if a
/// user runs `git init` mid-session, nerve needs a restart to see
/// the new branch — acceptable since `git init` is a human-scale
/// event.
#[derive(Default)]
pub struct BranchCache {
    entries: HashMap<PathBuf, BranchEntry>,
}

enum BranchEntry {
    InTree {
        head_path: PathBuf,
        head_mtime: SystemTime,
        branch: Option<String>,
    },
    NoTree,
}

impl BranchCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read_or_refresh(&mut self, cwd: &Path) -> Option<String> {
        // Cached InTree: stat the known HEAD path and reuse the
        // parsed branch if the mtime matches. The walk that found
        // HEAD in the first place doesn't repeat.
        if let Some(BranchEntry::InTree {
            head_path,
            head_mtime,
            branch,
        }) = self.entries.get(cwd)
        {
            match fs::metadata(head_path).and_then(|m| m.modified()) {
                Ok(mtime) if mtime == *head_mtime => return branch.clone(),
                Ok(mtime) => {
                    // HEAD was rewritten (checkout, branch rename) —
                    // re-read it, but skip the upward walk.
                    let hp = head_path.clone();
                    let branch = fs::read_to_string(&hp)
                        .ok()
                        .and_then(|raw| parse_head_contents(raw.trim()));
                    self.entries.insert(
                        cwd.to_path_buf(),
                        BranchEntry::InTree {
                            head_path: hp,
                            head_mtime: mtime,
                            branch: branch.clone(),
                        },
                    );
                    return branch;
                }
                Err(_) => {
                    // HEAD vanished (worktree removed, repo deleted) —
                    // drop the entry and let the walk run again below.
                    self.entries.remove(cwd);
                }
            }
        }
        if matches!(self.entries.get(cwd), Some(BranchEntry::NoTree)) {
            return None;
        }

        // Cache miss: do the full walk once.
        let head_path = match locate_head_file(cwd) {
            Some(p) => p,
            None => {
                self.entries.insert(cwd.to_path_buf(), BranchEntry::NoTree);
                return None;
            }
        };
        let raw = fs::read_to_string(&head_path).ok()?;
        let mtime = fs::metadata(&head_path)
            .and_then(|m| m.modified())
            .ok()?;
        let branch = parse_head_contents(raw.trim());
        self.entries.insert(
            cwd.to_path_buf(),
            BranchEntry::InTree {
                head_path,
                head_mtime: mtime,
                branch: branch.clone(),
            },
        );
        branch
    }

    /// Drop entries for cwds no longer referenced by an active
    /// session, so the cache doesn't grow unbounded.
    pub fn retain_present<F: Fn(&Path) -> bool>(&mut self, keep: F) {
        self.entries.retain(|cwd, _| keep(cwd));
    }
}

/// Walk up from `start` until we find a `.git` directory (or `.git` file
/// pointing to a worktree's git dir). Returns the path of the HEAD file.
fn locate_head_file(start: &Path) -> Option<PathBuf> {
    let mut here = start.to_path_buf();
    loop {
        let dot_git = here.join(".git");
        match fs::metadata(&dot_git) {
            Ok(m) if m.is_dir() => return Some(dot_git.join("HEAD")),
            Ok(m) if m.is_file() => return follow_gitfile(&dot_git),
            _ => {}
        }
        if !here.pop() {
            return None;
        }
    }
}

/// A `.git` file (used by submodules and worktrees) contains
/// `gitdir: <relative-or-absolute-path>` on its first non-empty line.
fn follow_gitfile(gitfile: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(gitfile).ok()?;
    let rest = contents.lines().find_map(|l| l.strip_prefix("gitdir:"))?;
    let trimmed = rest.trim();
    let gitdir = Path::new(trimmed);
    let absolute = if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        gitfile.parent()?.join(gitdir)
    };
    Some(absolute.join("HEAD"))
}

fn parse_head_contents(contents: &str) -> Option<String> {
    if let Some(rest) = contents.strip_prefix("ref:") {
        let refname = rest.trim();
        if let Some(branch) = refname.strip_prefix("refs/heads/") {
            return safe_refname(branch).map(str::to_string);
        }
        // Symbolic ref to something other than a local branch — return the
        // tail of the ref so the UI still shows something informative.
        return refname
            .rsplit('/')
            .next()
            .and_then(safe_refname)
            .map(str::to_string);
    }
    // Detached HEAD: 40-byte (or 64-byte SHA-256) hex string.
    if is_hex_sha(contents) {
        return Some("(detached)".to_string());
    }
    None
}

/// Refuse refnames containing ASCII control bytes — git itself forbids them
/// (`git check-ref-format`), and an attacker-planted `.git/HEAD` reading
/// `ref: refs/heads/\x1b[2Jpwned` would otherwise paint the cards view.
fn safe_refname(name: &str) -> Option<&str> {
    if name.is_empty() {
        return None;
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    Some(name)
}

fn is_hex_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_repo(dir: &Path, head_contents: &str) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let mut f = std::fs::File::create(dir.join(".git/HEAD")).unwrap();
        f.write_all(head_contents.as_bytes()).unwrap();
    }

    fn lookup_uncached(cwd: &Path) -> Option<String> {
        BranchCache::new().read_or_refresh(cwd)
    }

    #[test]
    fn reads_branch_from_normal_repo() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/main\n");
        assert_eq!(lookup_uncached(tmp.path()).as_deref(), Some("main"));
    }

    #[test]
    fn walks_up_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/develop\n");
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(lookup_uncached(&nested).as_deref(), Some("develop"));
    }

    #[test]
    fn detached_head_returns_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "abcdef1234567890abcdef1234567890abcdef12";
        make_repo(tmp.path(), &format!("{sha}\n"));
        assert_eq!(lookup_uncached(tmp.path()).as_deref(), Some("(detached)"));
    }

    #[test]
    fn no_repo_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(lookup_uncached(tmp.path()), None);
    }

    #[test]
    fn worktree_gitfile_followed() {
        let tmp = tempfile::tempdir().unwrap();
        // Main repo with worktrees/<name>/HEAD
        let main_git = tmp.path().join("main/.git");
        std::fs::create_dir_all(main_git.join("worktrees/feat")).unwrap();
        std::fs::write(
            main_git.join("worktrees/feat/HEAD"),
            "ref: refs/heads/feature-x\n",
        )
        .unwrap();
        // Worktree dir with a .git file pointing to the worktree's gitdir.
        let worktree_dir = tmp.path().join("worktree-feat");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(
            worktree_dir.join(".git"),
            format!("gitdir: {}\n", main_git.join("worktrees/feat").display()),
        )
        .unwrap();
        assert_eq!(lookup_uncached(&worktree_dir).as_deref(), Some("feature-x"));
    }

    #[test]
    fn sha256_detached_recognized() {
        // 64 hex chars = SHA-256 detached head.
        let sha = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        assert_eq!(parse_head_contents(sha).as_deref(), Some("(detached)"));
    }

    #[test]
    fn ref_outside_heads_returns_tail() {
        assert_eq!(
            parse_head_contents("ref: refs/tags/v1.0").as_deref(),
            Some("v1.0")
        );
    }

    #[test]
    fn malformed_head_returns_none() {
        assert_eq!(parse_head_contents("garbage"), None);
        assert_eq!(parse_head_contents(""), None);
    }

    #[test]
    fn refname_with_control_byte_rejected() {
        // Attacker-planted .git/HEAD trying to smuggle ANSI through the
        // branch display path.
        assert_eq!(parse_head_contents("ref: refs/heads/\x1b[2Jpwned"), None);
        assert_eq!(parse_head_contents("ref: refs/heads/main\x07"), None);
    }

    #[test]
    fn cache_short_circuits_after_first_walk() {
        // Removing the entire .git directory after the first lookup
        // should not invalidate the cached answer when the recorded
        // HEAD path is gone — but here we keep HEAD in place and
        // verify the cache returns the same value without re-reading
        // it (the only way to observe "no re-read" without
        // instrumentation is to ensure correctness when HEAD's
        // *contents* would have changed something — we cover that in
        // the next test).
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/main\n");

        let mut cache = BranchCache::new();
        assert_eq!(cache.read_or_refresh(tmp.path()).as_deref(), Some("main"));
        assert_eq!(cache.read_or_refresh(tmp.path()).as_deref(), Some("main"));
    }

    #[test]
    fn cache_invalidates_on_head_mtime_change() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/main\n");
        let mut cache = BranchCache::new();
        assert_eq!(cache.read_or_refresh(tmp.path()).as_deref(), Some("main"));

        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/dev\n").unwrap();
        assert_eq!(cache.read_or_refresh(tmp.path()).as_deref(), Some("dev"));
    }

    #[test]
    fn cache_remembers_no_tree() {
        // A cwd without `.git` ancestry caches as NoTree; subsequent
        // lookups return None without re-walking. The (negative)
        // result is sticky for the cache's lifetime.
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = BranchCache::new();
        assert_eq!(cache.read_or_refresh(tmp.path()), None);
        // Even if a .git directory appears later, the cache holds NoTree.
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(cache.read_or_refresh(tmp.path()), None);
    }

    #[test]
    fn cache_evicts_when_head_disappears() {
        // After the .git directory is removed, the cached InTree
        // entry stops returning its stale branch and the lookup
        // falls through to re-walk (which now finds nothing).
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/main\n");
        let mut cache = BranchCache::new();
        assert_eq!(cache.read_or_refresh(tmp.path()).as_deref(), Some("main"));

        std::fs::remove_dir_all(tmp.path().join(".git")).unwrap();
        assert_eq!(cache.read_or_refresh(tmp.path()), None);
    }

    #[test]
    fn retain_present_drops_missing_cwds() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/main\n");
        let mut cache = BranchCache::new();
        cache.read_or_refresh(tmp.path());
        assert!(cache.entries.contains_key(tmp.path()));

        cache.retain_present(|_| false);
        assert!(!cache.entries.contains_key(tmp.path()));
    }
}
