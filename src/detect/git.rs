use std::fs;
use std::path::{Path, PathBuf};

/// Return the current branch name for the repo containing `cwd`, or `None`
/// if `cwd` is not inside a git working tree. Replaces a `git rev-parse`
/// fork — avoids paying for an exec per session per tick and avoids the
/// security exposure of running git in an attacker-controlled cwd (S7).
pub fn read_branch(cwd: &Path) -> Option<String> {
    let head_path = locate_head_file(cwd)?;
    let raw = fs::read_to_string(&head_path).ok()?;
    parse_head_contents(raw.trim())
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
            return Some(branch.to_string());
        }
        // Symbolic ref to something other than a local branch — return the
        // tail of the ref so the UI still shows something informative.
        return refname.rsplit('/').next().map(|s| s.to_string());
    }
    // Detached HEAD: 40-byte (or 64-byte SHA-256) hex string.
    if is_hex_sha(contents) {
        return Some("(detached)".to_string());
    }
    None
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

    #[test]
    fn reads_branch_from_normal_repo() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/main\n");
        assert_eq!(read_branch(tmp.path()).as_deref(), Some("main"));
    }

    #[test]
    fn walks_up_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path(), "ref: refs/heads/develop\n");
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(read_branch(&nested).as_deref(), Some("develop"));
    }

    #[test]
    fn detached_head_returns_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "abcdef1234567890abcdef1234567890abcdef12";
        make_repo(tmp.path(), &format!("{sha}\n"));
        assert_eq!(read_branch(tmp.path()).as_deref(), Some("(detached)"));
    }

    #[test]
    fn no_repo_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_branch(tmp.path()), None);
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
        assert_eq!(read_branch(&worktree_dir).as_deref(), Some("feature-x"));
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
}
