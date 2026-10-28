//! Named places: `[places] v2-harness = "/abs/path"` on every device,
//! so a sender names the place and each side resolves its own path.
//!
//! Two things move into a place:
//!
//! - **Files** (`nerve sync put|get`): paths relative to the place, cut
//!   into the same content-defined chunks as a hand-off, so a repeat send
//!   moves only the chunks the peer is not known to hold. Each file is
//!   written to a temp file beside it and renamed into place, with its
//!   mode kept. Nothing is written outside the place, and never through
//!   a symlink.
//! - **Git refs** (`nerve sync refs`): a bundle of only the commits the
//!   target lacks (it reports its tips first, as for a hand-off), whose
//!   objects the target unbundles before pointing exactly the named
//!   destination refs at them. Nothing checks out, and HEAD, the index,
//!   the working tree and every other ref stay as they were.

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::chunks::{FileRef, Store};
use super::handoff::{git_root, is_sha, repo_id, run, safe_rel, tempdir, text, walk};
use super::protocol::RefMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct PlaceFile {
    /// `name` is the path relative to the place.
    pub(crate) file: FileRef,
    pub(crate) mode: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct RefTip {
    pub(crate) src: String,
    pub(crate) dst: String,
    pub(crate) sha: String,
}

/// What a transfer into a place carries; travels as the chunked index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum XferPack {
    Files {
        /// The roots that were directories on the sender: the scope of `delete`.
        dirs: Vec<String>,
        delete: bool,
        files: Vec<PlaceFile>,
    },
    Refs {
        refs: Vec<RefTip>,
        /// Absent when the target already had every commit.
        bundle: Option<FileRef>,
    },
}

impl XferPack {
    pub(crate) fn file_refs(&self) -> Vec<FileRef> {
        match self {
            Self::Files { files, .. } => files.iter().map(|f| f.file.clone()).collect(),
            Self::Refs { bundle, .. } => bundle.iter().cloned().collect(),
        }
    }
}

/// A root as given on the command line: relative to the place, inside
/// it; "." (or "") is the whole place.
pub(crate) fn norm_root(root: &str) -> Result<String, String> {
    let r = root.trim_end_matches('/');
    if r.is_empty() || r == "." {
        return Ok(String::new());
    }
    let r = r.strip_prefix("./").unwrap_or(r);
    safe_rel(r).map(String::from).ok_or_else(|| format!("{root}: paths must be relative to the place and stay inside it"))
}

fn join(base: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    }
}

/// Sender: chunk every regular file under `roots` into `store`.
/// Symlinks are not followed and not sent.
pub(crate) fn pack_files(base: &Path, roots: &[String], delete: bool, store: &Store) -> Result<XferPack, String> {
    if !base.is_dir() {
        return Err(format!("{} is not a directory", base.display()));
    }
    let mut picked: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs = Vec::new();
    for root in roots {
        let rel = norm_root(root)?;
        let p = join(base, &rel);
        let meta = std::fs::symlink_metadata(&p).map_err(|_| format!("{root} does not exist in {}", base.display()))?;
        if meta.file_type().is_symlink() {
            return Err(format!("{root} is a symlink; send what it points at instead"));
        } else if meta.is_dir() {
            walk(&p, &rel, &mut picked);
            dirs.push(rel);
        } else if meta.is_file() {
            picked.push((rel, p));
        }
    }
    let mut seen = HashSet::new();
    picked.retain(|(rel, _)| seen.insert(rel.clone()));
    picked.sort();
    let mut files = Vec::with_capacity(picked.len());
    for (rel, p) in picked {
        let bytes = std::fs::read(&p).map_err(|e| format!("{rel}: {e}"))?;
        let mode = std::fs::metadata(&p).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o644);
        files.push(PlaceFile { file: store.add_file(&rel, &bytes).map_err(|e| e.to_string())?, mode });
    }
    Ok(XferPack::Files { dirs, delete, files })
}

/// What landed, for the ack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) written: usize,
    pub(crate) unchanged: usize,
    pub(crate) deleted: usize,
}

/// Every existing ancestor of `rel` under `base` must be a real
/// directory: a symlink there could point the write anywhere.
fn check_parents(base: &Path, rel: &str, create: bool) -> Result<(), String> {
    let mut cur = base.to_path_buf();
    let comps: Vec<_> = Path::new(rel).components().collect();
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(format!("refusing to write through symlink {}", cur.display())),
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{} is a file, not a directory", cur.display())),
            Err(_) if create => std::fs::create_dir(&cur).map_err(|e| format!("{}: {e}", cur.display()))?,
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

/// Receiver of a bulk transfer, before rsync writes anything: the
/// same refusals as `apply_files`, and the parents rsync cannot make.
pub(crate) fn ready_for(base: &Path, rel: &str) -> Result<(), String> {
    check_parents(base, rel, true)?;
    let p = join(base, rel);
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_symlink() => Err(format!("refusing to write through symlink {}", p.display())),
        _ => Ok(()),
    }
}

/// Receiver: write the files into the place. Every path is checked
/// before anything is written, so one bad path refuses the whole set.
pub(crate) fn apply_files(pack: &XferPack, base: &Path, store: &Store) -> Result<Applied, String> {
    let XferPack::Files { dirs, delete, files } = pack else {
        return Err("not a file transfer".into());
    };
    std::fs::create_dir_all(base).map_err(|e| format!("{}: {e}", base.display()))?;
    let base = base.canonicalize().map_err(|e| e.to_string())?;
    for f in files {
        safe_rel(&f.file.name).ok_or_else(|| format!("unsafe path in transfer: {}", f.file.name))?;
        check_parents(&base, &f.file.name, false)?;
        if std::fs::symlink_metadata(base.join(&f.file.name)).is_ok_and(|m| m.is_dir()) {
            return Err(format!("{} is a directory here", f.file.name));
        }
    }
    for d in dirs {
        if !d.is_empty() {
            safe_rel(d).ok_or_else(|| format!("unsafe path in transfer: {d}"))?;
        }
    }
    let mut out = Applied::default();
    for f in files {
        let bytes = store.assemble(&f.file).map_err(|m| format!("{} chunk(s) of {} missing", m.len(), f.file.name))?;
        let dest = base.join(&f.file.name);
        let mode = f.mode & 0o777;
        let same = std::fs::symlink_metadata(&dest).is_ok_and(|m| {
            m.is_file() && m.len() == bytes.len() as u64 && m.permissions().mode() & 0o777 == mode
        }) && std::fs::read(&dest).is_ok_and(|b| b == bytes);
        if same {
            out.unchanged += 1;
            continue;
        }
        check_parents(&base, &f.file.name, true)?;
        let parent = dest.parent().unwrap_or(&base);
        let leaf = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = parent.join(format!(".{leaf}.nerve-{}", super::protocol::new_id()));
        let write = || -> std::io::Result<()> {
            let mut fh = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            fh.write_all(&bytes)?;
            fh.set_permissions(std::fs::Permissions::from_mode(mode))?;
            std::fs::rename(&tmp, &dest)
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("{}: {e}", f.file.name));
        }
        out.written += 1;
    }
    if *delete {
        let keep: HashSet<&str> = files.iter().map(|f| f.file.name.as_str()).collect();
        for d in dirs {
            let root = join(&base, d);
            if !d.is_empty() && (check_parents(&base, &format!("{d}/x"), false).is_err() || !std::fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir())) {
                continue;
            }
            let mut have = Vec::new();
            walk_all(&root, d, &mut have);
            let mut parents = BTreeSet::new();
            for (rel, p) in have {
                if !keep.contains(rel.as_str()) && std::fs::remove_file(&p).is_ok() {
                    out.deleted += 1;
                    let mut q = p.parent().map(Path::to_path_buf);
                    while let Some(dir) = q.filter(|q| q.starts_with(&root) && *q != root) {
                        parents.insert(dir.clone());
                        q = dir.parent().map(Path::to_path_buf);
                    }
                }
            }
            // Deepest first; remove_dir leaves any directory still in use.
            for dir in parents.iter().rev() {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
    Ok(out)
}

/// Files *and symlinks* under `dir` (symlinked directories are not
/// entered), for `--delete`.
fn walk_all(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_all(&e.path(), &r, out),
            Ok(_) => out.push((r, e.path())),
            Err(_) => {}
        }
    }
}

// ---- refs ----

/// The repo a place holds (the place may be any directory inside it).
pub(crate) fn place_repo(place: &Path) -> Result<PathBuf, String> {
    git_root(place).map_err(|_| format!("{} is not inside a git repository", place.display()))
}

/// Commits the target already has: its branch and remote-tracking tips,
/// newest first, capped so the answer fits one message.
pub(crate) fn repo_tips(repo: &Path) -> Vec<String> {
    text(repo, &["git", "for-each-ref", "--sort=-committerdate", "--count=500", "--format=%(objectname)", "refs/heads", "refs/remotes"])
        .map(|s| s.lines().filter(|l| is_sha(l)).map(String::from).collect())
        .unwrap_or_default()
}

pub(crate) fn this_repo_id(repo: &Path) -> String {
    repo_id(repo).unwrap_or_default()
}

/// A destination ref the target may write: a full, well-formed ref name
/// that is not what HEAD points at (moving the checked-out branch would
/// leave the working tree out of step with it).
pub(crate) fn check_dst(repo: &Path, dst: &str) -> Result<(), String> {
    if !dst.starts_with("refs/") || run(repo, &["git", "check-ref-format", dst]).is_err() {
        return Err(format!("{dst}: destination must be a full ref name such as refs/remotes/mac/main"));
    }
    if text(repo, &["git", "symbolic-ref", "-q", "HEAD"]).is_ok_and(|h| h == dst) {
        return Err(format!("{dst} is checked out in {}; pick a ref that isn't, such as refs/remotes/<peer>/…", repo.display()));
    }
    Ok(())
}

/// Sender: resolve each source ref and bundle only the commits beyond
/// the target's `have` tips.
pub(crate) fn pack_refs(place: &Path, map: &[RefMap], have: &[String], store: &Store) -> Result<XferPack, String> {
    let repo = place_repo(place)?;
    let mut refs = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for m in map {
        if m.src.is_empty() || m.src.starts_with('-') {
            return Err(format!("bad source ref {:?}", m.src));
        }
        // Bundles list refs by name, so a bare commit id can't be sent.
        let full = text(&repo, &["git", "rev-parse", "--symbolic-full-name", &m.src]).unwrap_or_default();
        if full.is_empty() || full.contains('\n') {
            return Err(format!("{} is not a ref in {} (give a branch, tag or HEAD)", m.src, repo.display()));
        }
        let sha = text(&repo, &["git", "rev-parse", "--verify", "-q", &format!("{}^{{commit}}", m.src)])
            .map_err(|_| format!("{} does not name a commit in {}", m.src, repo.display()))?;
        refs.push(RefTip { src: m.src.clone(), dst: m.dst.clone(), sha });
        if !names.contains(&full) {
            names.push(full);
        }
    }
    let known: Vec<&String> = have
        .iter()
        .filter(|c| is_sha(c) && run(&repo, &["git", "cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok())
        .collect();
    let stage = tempdir()?;
    let file = stage.path().join("refs.bundle");
    let mut argv: Vec<String> = ["git", "bundle", "create", "-q"].map(String::from).to_vec();
    argv.push(file.to_string_lossy().into_owned());
    argv.extend(names);
    if !known.is_empty() {
        argv.push("--not".into());
        argv.extend(known.iter().map(|c| c.to_string()));
    }
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let bundle = match run(&repo, &argv) {
        Ok(_) => Some(store.add_file("refs.bundle", &std::fs::read(&file).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?),
        Err(e) if e.contains("empty bundle") => None,
        Err(e) => return Err(e),
    };
    Ok(XferPack::Refs { refs, bundle })
}

/// Receiver: add the bundle's objects, then point each destination ref
/// at its commit. Returns one line per ref.
pub(crate) fn apply_refs(pack: &XferPack, place: &Path, store: &Store) -> Result<Vec<String>, String> {
    let XferPack::Refs { refs, bundle } = pack else {
        return Err("not a refs transfer".into());
    };
    let repo = place_repo(place)?;
    for r in refs {
        check_dst(&repo, &r.dst)?;
        if !is_sha(&r.sha) {
            return Err(format!("bad commit id for {}", r.dst));
        }
    }
    if let Some(b) = bundle {
        let bytes = store.assemble(b).map_err(|m| format!("{} chunk(s) of the bundle missing", m.len()))?;
        let stage = tempdir()?;
        let file = stage.path().join("refs.bundle");
        std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
        // Writes objects only; unlike `fetch` it updates no refs at all.
        run(&repo, &["git", "bundle", "unbundle", &file.to_string_lossy()])?;
    }
    let mut lines = Vec::new();
    for r in refs {
        run(&repo, &["git", "cat-file", "-e", &format!("{}^{{commit}}", r.sha)]).map_err(|_| format!("commit {} for {} did not arrive", &r.sha[..12], r.dst))?;
        let old = text(&repo, &["git", "rev-parse", "-q", "--verify", &r.dst]).ok();
        if old.as_deref() == Some(r.sha.as_str()) {
            lines.push(format!("{} = {} (unchanged)", r.dst, &r.sha[..10]));
            continue;
        }
        run(&repo, &["git", "update-ref", "-m", &format!("nerve sync refs: {}", r.src), &r.dst, &r.sha])?;
        lines.push(match old {
            Some(o) => format!("{} {}..{}", r.dst, &o[..10.min(o.len())], &r.sha[..10]),
            None => format!("{} = {} (new)", r.dst, &r.sha[..10]),
        });
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn move_chunks(pack: &XferPack, from: &Store, to: &Store) -> (usize, u64) {
        let (mut n, mut bytes) = (0, 0);
        for id in super::super::chunks::needed(&pack.file_refs(), &Default::default()) {
            if !to.has(&id) {
                let b = from.get(&id).unwrap();
                bytes += b.len() as u64;
                n += 1;
                to.put_verified(&id, &b).unwrap();
            }
        }
        (n, bytes)
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn put_writes_inside_the_place_keeps_modes_and_repeats_cheaply() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(a.join("bin")).unwrap();
        std::fs::create_dir_all(a.join("units/S5")).unwrap();
        for i in 0..40 {
            let body: String = (0..400).map(|j| format!("unit {i} line {j}\n")).collect();
            std::fs::write(a.join(format!("units/S5/f{i}.md")), body).unwrap();
        }
        std::fs::write(a.join("bin/v2.mjs"), "#!/usr/bin/env node\n").unwrap();
        std::fs::set_permissions(a.join("bin/v2.mjs"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let (sa, sb) = (Store::new(&t.path().join("sa")), Store::new(&t.path().join("sb")));
        let roots = vec!["units".to_string(), "bin/v2.mjs".to_string()];

        let p1 = pack_files(&a, &roots, false, &sa).unwrap();
        let (first_n, first_bytes) = move_chunks(&p1, &sa, &sb);
        let got = apply_files(&p1, &b, &sb).unwrap();
        assert_eq!((got.written, got.unchanged), (41, 0));
        assert_eq!(std::fs::read(b.join("units/S5/f7.md")).unwrap(), std::fs::read(a.join("units/S5/f7.md")).unwrap());
        assert_eq!(mode(&b.join("bin/v2.mjs")), 0o755);

        // One file changes: only its chunks move, only it is rewritten.
        let mut body = std::fs::read_to_string(a.join("units/S5/f3.md")).unwrap();
        body.push_str("one more line\n");
        std::fs::write(a.join("units/S5/f3.md"), body).unwrap();
        let p2 = pack_files(&a, &roots, false, &sa).unwrap();
        let (n, bytes) = move_chunks(&p2, &sa, &sb);
        assert!(n <= 2 && bytes * 20 < first_bytes, "repeat moved {n} chunks / {bytes} B; first {first_n} / {first_bytes} B");
        let got = apply_files(&p2, &b, &sb).unwrap();
        assert_eq!((got.written, got.unchanged), (1, 40));
        // No temp files left behind.
        assert!(std::fs::read_dir(b.join("units/S5")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with('.')));
    }

    #[test]
    fn delete_removes_only_what_the_sender_dropped_under_the_given_dirs() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(a.join("d/sub")).unwrap();
        std::fs::write(a.join("d/keep.txt"), "k").unwrap();
        std::fs::create_dir_all(b.join("d/sub/deeper")).unwrap();
        std::fs::create_dir_all(b.join("other")).unwrap();
        std::fs::write(b.join("d/stale.txt"), "s").unwrap();
        std::fs::write(b.join("d/sub/deeper/old.txt"), "o").unwrap();
        std::fs::write(b.join("other/untouched.txt"), "u").unwrap();
        let (sa, sb) = (Store::new(&t.path().join("sa")), Store::new(&t.path().join("sb")));
        let p = pack_files(&a, &["d".into()], true, &sa).unwrap();
        move_chunks(&p, &sa, &sb);
        let got = apply_files(&p, &b, &sb).unwrap();
        assert_eq!((got.written, got.deleted), (1, 2));
        assert!(b.join("d/keep.txt").is_file() && !b.join("d/stale.txt").exists());
        assert!(!b.join("d/sub").exists(), "emptied directories go too");
        assert!(b.join("other/untouched.txt").is_file(), "outside the given dirs nothing is deleted");
        // Without --delete, nothing is.
        std::fs::write(b.join("d/stale.txt"), "s").unwrap();
        let p = pack_files(&a, &["d".into()], false, &sa).unwrap();
        assert_eq!(apply_files(&p, &b, &sb).unwrap().deleted, 0);
    }

    #[test]
    fn writes_cannot_escape_the_place() {
        let t = tempfile::tempdir().unwrap();
        let (b, outside) = (t.path().join("b"), t.path().join("outside"));
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, b.join("link")).unwrap();
        let s = Store::new(&t.path().join("s"));
        let f = |name: &str| PlaceFile { file: s.add_file(name, b"x").unwrap(), mode: 0o644 };
        let via_link = XferPack::Files { dirs: vec![], delete: false, files: vec![f("ok.txt"), f("link/evil.txt")] };
        assert!(apply_files(&via_link, &b, &s).unwrap_err().contains("symlink"));
        assert!(!b.join("ok.txt").exists(), "checked before anything is written");
        assert!(!outside.join("evil.txt").exists());
        let dotdot = XferPack::Files { dirs: vec![], delete: false, files: vec![f("../evil.txt")] };
        assert!(apply_files(&dotdot, &b, &s).unwrap_err().contains("unsafe"));
        assert!(norm_root("../x").is_err() && norm_root("/etc").is_err());
        assert_eq!(norm_root("./").unwrap(), "");
        // --delete doesn't follow a symlinked dir out of the place either.
        std::fs::write(outside.join("precious.txt"), "p").unwrap();
        let p = XferPack::Files { dirs: vec!["link".into()], delete: true, files: vec![] };
        apply_files(&p, &b, &s).unwrap();
        assert!(outside.join("precious.txt").exists());
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn refs_land_in_exactly_the_named_refs_and_nothing_else_moves() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        git(&a, &["init", "-q"]);
        for i in 0..5 {
            std::fs::write(a.join("f"), format!("{i}\n").repeat(500)).unwrap();
            git(&a, &["add", "."]);
            git(&a, &["commit", "-qm", &format!("c{i}")]);
        }
        git(t.path(), &["clone", "-q", &a.to_string_lossy(), &b.to_string_lossy()]);
        git(&a, &["checkout", "-qb", "v2-build"]);
        std::fs::write(a.join("g"), "new\n").unwrap();
        git(&a, &["add", "."]);
        git(&a, &["commit", "-qm", "on v2-build"]);
        std::fs::write(b.join("f"), "local edit\n").unwrap(); // dirty worktree on the target
        let head_before = text(&b, &["git", "rev-parse", "HEAD"]).unwrap();
        let refs_before = text(&b, &["git", "for-each-ref", "--format=%(refname)"]).unwrap();
        let (sa, sb) = (Store::new(&t.path().join("sa")), Store::new(&t.path().join("sb")));

        let map = vec![RefMap { src: "v2-build".into(), dst: "refs/remotes/mac/v2-build".into() }];
        let tips = repo_tips(&b);
        let p = pack_refs(&a, &map, &tips, &sa).unwrap();
        let (n, _) = move_chunks(&p, &sa, &sb);
        assert!(n >= 1);
        let lines = apply_refs(&p, &b, &sb).unwrap();
        assert!(lines[0].contains("refs/remotes/mac/v2-build") && lines[0].contains("(new)"), "{lines:?}");
        assert_eq!(text(&b, &["git", "rev-parse", "refs/remotes/mac/v2-build"]).unwrap(), text(&a, &["git", "rev-parse", "v2-build"]).unwrap());
        assert_eq!(text(&b, &["git", "rev-parse", "HEAD"]).unwrap(), head_before, "HEAD did not move");
        assert_eq!(std::fs::read_to_string(b.join("f")).unwrap(), "local edit\n", "worktree untouched");
        assert!(!b.join("g").exists());
        let refs_after = text(&b, &["git", "for-each-ref", "--format=%(refname)"]).unwrap();
        assert_eq!(refs_after.lines().count(), refs_before.lines().count() + 1, "exactly one new ref");

        // Again with nothing new: no bundle, ref unchanged.
        let p = pack_refs(&a, &map, &repo_tips(&b), &sa).unwrap();
        assert!(matches!(&p, XferPack::Refs { bundle: None, .. }));
        assert!(apply_refs(&p, &b, &sb).unwrap()[0].contains("unchanged"));

        // The checked-out branch and malformed names are refused.
        assert!(check_dst(&b, "refs/heads/main").unwrap_err().contains("checked out"));
        assert!(check_dst(&b, "main").is_err() && check_dst(&b, "refs/heads/a..b").is_err());
        let bad = vec![RefMap { src: text(&a, &["git", "rev-parse", "HEAD"]).unwrap(), dst: "refs/x".into() }];
        assert!(pack_refs(&a, &bad, &[], &sa).unwrap_err().contains("not a ref"));
    }
}
