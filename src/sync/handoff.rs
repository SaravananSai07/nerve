//! Moving a session to another device: pack (source), unpack + resume
//! (target). Validated by hand in research/sync/experiments (E7) before
//! being written here: a git bundle + working-tree patch + untracked
//! files, plus the transcript installed under the *target* cwd's project
//! slug, is enough for `claude --resume <id>` to continue with full
//! memory — absolute source paths inside the transcript don't matter.
//!
//! Only the first hand-off of a repo is big. The bundle carries just the
//! commits beyond what the target's checkout reports having, and every
//! other file goes through the chunk store, so the target fetches only
//! chunks it lacks: a second hand-off of the same session moves the new
//! commits and the transcript's last few chunks (E19).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use super::chunks::{FileRef, Store};
use super::fleet::Fleet;
use super::protocol::{project_slug, HandoffOffer};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Manifest {
    pub(crate) session_id: String,
    pub(crate) src_cwd: String,
    pub(crate) repo_name: String,
    /// The repo's root commit; the target only reuses a checkout whose
    /// root commit matches.
    #[serde(default)]
    pub(crate) repo_id: String,
    pub(crate) branch: String,
    pub(crate) head: String,
    pub(crate) src_host: String,
}

/// Everything a hand-off moves, as chunked files. The index travels as
/// a chunked file too (`HandoffManifest.index`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Pack {
    pub(crate) meta: Manifest,
    /// `repo.bundle` (commits the target lacks; absent when it has them
    /// all), `worktree.patch`, `transcript.jsonl`, `u/<path>` untracked
    /// files, `s/<path>` transcript sidecar files.
    pub(crate) files: Vec<FileRef>,
}

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

pub(crate) fn text(dir: &Path, argv: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&run(dir, argv)?).trim().to_string())
}

pub(crate) fn git_root(cwd: &Path) -> Result<PathBuf, String> {
    Ok(PathBuf::from(text(cwd, &["git", "rev-parse", "--show-toplevel"])?))
}

/// The first commit reachable from HEAD: the same on every clone.
pub(crate) fn repo_id(repo: &Path) -> Option<String> {
    let out = text(repo, &["git", "rev-list", "--max-parents=0", "HEAD"]).ok()?;
    out.lines().last().map(String::from)
}

/// Facts about the repo a session runs in, for the offer.
pub(crate) fn describe(cwd: &Path) -> Result<Manifest, String> {
    let root = git_root(cwd).map_err(|e| format!("not a git repo ({e}); only git projects can be handed off"))?;
    Ok(Manifest {
        session_id: String::new(),
        src_cwd: cwd.to_string_lossy().into_owned(),
        repo_name: root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into()),
        repo_id: repo_id(&root).unwrap_or_default(),
        branch: text(&root, &["git", "rev-parse", "--abbrev-ref", "HEAD"])?,
        head: text(&root, &["git", "rev-parse", "HEAD"])?,
        src_host: super::fleet::hostname(),
    })
}

/// Commits the target's checkout already has, when it is the same repo:
/// the tips of its branches and of earlier hand-offs.
pub(crate) fn local_tips(dest: &Path, repo_id: &str) -> Vec<String> {
    if repo_id.is_empty() || !dest.join(".git").exists() || self::repo_id(dest).as_deref() != Some(repo_id) {
        return Vec::new();
    }
    text(dest, &["git", "for-each-ref", "--format=%(objectname)", "refs/heads", "refs/remotes/nerve-handoff"])
        .map(|s| s.lines().filter(|l| is_sha(l)).map(String::from).collect())
        .unwrap_or_default()
}

pub(crate) fn is_sha(s: &str) -> bool {
    (40..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Find a session's transcript (and sidecar dir) under any Claude root.
pub(crate) fn find_transcript(roots: &[PathBuf], session_id: &str) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        std::fs::read_dir(root.join("projects")).ok()?.flatten().find_map(|d| {
            let p = d.path().join(format!("{session_id}.jsonl"));
            p.is_file().then_some(p)
        })
    })
}

pub(crate) fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&e.path(), &r, out),
            Ok(t) if t.is_file() => out.push((r, e.path())),
            _ => {}
        }
    }
}

/// Collect the session's files into `store`. `have` are commits the
/// target reported; the git bundle only carries what lies beyond them.
pub(crate) fn pack(cwd: &Path, session_id: &str, roots: &[PathBuf], have: &[String], store: &Store) -> Result<Pack, String> {
    let root = git_root(cwd).map_err(|e| format!("not a git repo ({e}); only git projects can be handed off"))?;
    let transcript = find_transcript(roots, session_id).ok_or("transcript not found")?;
    let mut meta = describe(cwd)?;
    meta.session_id = session_id.to_string();
    let mut files = Vec::new();
    let mut add = |name: &str, bytes: &[u8]| -> Result<(), String> {
        files.push(store.add_file(name, bytes).map_err(|e| e.to_string())?);
        Ok(())
    };

    // Only prerequisites this repo actually has can bound the bundle.
    let known: Vec<&String> = have
        .iter()
        .filter(|c| is_sha(c) && run(&root, &["git", "cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok())
        .collect();
    let stage = tempdir()?;
    let bundle = stage.path().join("repo.bundle");
    let mut argv: Vec<String> = ["git", "bundle", "create", "-q"].map(String::from).to_vec();
    argv.push(bundle.to_string_lossy().into_owned());
    argv.extend(["--all".to_string(), "HEAD".to_string()]);
    if !known.is_empty() {
        argv.push("--not".into());
        argv.extend(known.iter().map(|c| c.to_string()));
    }
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    match run(&root, &argv_ref) {
        Ok(_) => add("repo.bundle", &std::fs::read(&bundle).map_err(|e| e.to_string())?)?,
        // Nothing new to send: the target has every commit already.
        Err(e) if e.contains("empty bundle") => {}
        Err(e) => return Err(e),
    }
    add("worktree.patch", &run(&root, &["git", "diff", "--binary", "HEAD"])?)?;
    for path in run(&root, &["git", "ls-files", "--others", "--exclude-standard", "-z"])?.split(|b| *b == 0) {
        let rel = String::from_utf8_lossy(path).into_owned();
        if rel.is_empty() {
            continue;
        }
        if let Ok(bytes) = std::fs::read(root.join(&rel)) {
            add(&format!("u/{rel}"), &bytes)?;
        }
    }
    add("transcript.jsonl", &std::fs::read(&transcript).map_err(|e| e.to_string())?)?;
    let mut side = Vec::new();
    walk(&transcript.with_extension(""), "", &mut side);
    for (rel, p) in side {
        add(&format!("s/{rel}"), &std::fs::read(p).map_err(|e| e.to_string())?)?;
    }
    Ok(Pack { meta, files })
}

/// Where the repo lands on the target, in order: the place named with
/// `--to`, the `path_map`-mapped source path, or under an explicitly set
/// `handoff_root`. With none of them the hand-off is refused, naming the
/// config to add, rather than landing somewhere nobody chose.
pub(crate) fn destination(offer: &HandoffOffer, fleet: &Fleet, fleet_file: &Path) -> Result<PathBuf, String> {
    if let Some(p) = &offer.place {
        return fleet.place(p, fleet_file);
    }
    if let Some(p) = fleet.map_path(&offer.src_cwd) {
        return Ok(p);
    }
    if let Some(root) = &fleet.handoff_root {
        return Ok(root.join(&offer.repo_name));
    }
    Err(format!(
        "no place for '{}' on {}: hand off with --to <place> and add `<place> = \"/abs/path\"` under [places], or map {} under [path_map], in {}",
        offer.repo_name,
        fleet.device,
        offer.src_cwd,
        fleet_file.display()
    ))
}

#[derive(Debug)]
pub(crate) struct Installed {
    pub(crate) cwd: PathBuf,
    pub(crate) manifest: Manifest,
}

/// A relative path from the wire that stays inside its directory.
pub(crate) fn safe_rel(rel: &str) -> Option<&str> {
    let ok = !rel.is_empty()
        && !rel.starts_with('/')
        && Path::new(rel).components().all(|c| matches!(c, std::path::Component::Normal(_)));
    ok.then_some(rel)
}

/// Restore the repo state at `dest` and install the transcript under the
/// target cwd's slug in `claude_root`. Every chunk must be in `store`.
/// An existing checkout keeps its branch: one on another branch, or
/// whose branch has commits the hand-off lacks, is refused unless `force`.
pub(crate) fn unpack(pack: &Pack, dest: &Path, store: &Store, claude_root: &Path, force: bool) -> Result<Installed, String> {
    let manifest = pack.meta.clone();
    if !valid_session_id(&manifest.session_id) || !is_sha(&manifest.head) {
        return Err("bad session id or head in manifest".into());
    }
    let file = |name: &str| -> Result<Option<Vec<u8>>, String> {
        match pack.files.iter().find(|f| f.name == name) {
            None => Ok(None),
            Some(f) => store.assemble(f).map(Some).map_err(|m| format!("{} chunk(s) of {name} missing", m.len())),
        }
    };
    let stage = tempdir()?;
    let bundle = file("repo.bundle")?;
    let bundle_path = stage.path().join("repo.bundle");
    if let Some(b) = &bundle {
        std::fs::write(&bundle_path, b).map_err(|e| e.to_string())?;
    }
    let bundle_s = bundle_path.to_string_lossy().into_owned();
    if !dest.join(".git").exists() {
        if bundle.is_none() {
            return Err("no checkout here and no commits sent; hand off again".into());
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        run(Path::new("/"), &["git", "clone", "-q", &bundle_s, &dest.to_string_lossy()])?;
    } else {
        // Refuse to clobber uncommitted work already on this device.
        let dirty = run(dest, &["git", "status", "--porcelain"])?;
        if !dirty.is_empty() {
            return Err(format!("{} has uncommitted changes; commit or stash them first", dest.display()));
        }
        let current = text(dest, &["git", "rev-parse", "--abbrev-ref", "HEAD"])?;
        if current != handoff_branch(&manifest) && !force {
            return Err(format!(
                "{} is on branch '{current}', not '{}'; switch it yourself or hand off with --force",
                dest.display(),
                handoff_branch(&manifest)
            ));
        }
    }
    if bundle.is_some() {
        run(dest, &["git", "fetch", "-q", &bundle_s, "+refs/heads/*:refs/remotes/nerve-handoff/*"])?;
    }
    let branch = handoff_branch(&manifest);
    // `checkout -B` resets the branch: never past commits only the target has.
    let tip = text(dest, &["git", "rev-parse", "-q", "--verify", &format!("refs/heads/{branch}")]).ok();
    if let Some(tip) = tip.filter(|t| !force && *t != manifest.head) {
        if run(dest, &["git", "merge-base", "--is-ancestor", &tip, &manifest.head]).is_err() {
            return Err(format!(
                "branch '{branch}' in {} has commits the hand-off doesn't; merge them on the source first, or hand off with --force",
                dest.display()
            ));
        }
    }
    run(dest, &["git", "checkout", "-q", "-B", branch, &manifest.head])?;
    if let Some(patch) = file("worktree.patch")?.filter(|p| !p.is_empty()) {
        let p = stage.path().join("worktree.patch");
        std::fs::write(&p, patch).map_err(|e| e.to_string())?;
        run(dest, &["git", "apply", "--binary", &p.to_string_lossy()])?;
    }
    let dest = dest.canonicalize().map_err(|e| e.to_string())?;
    let proj = claude_root.join("projects").join(project_slug(&dest.to_string_lossy()));
    std::fs::create_dir_all(&proj).map_err(|e| e.to_string())?;
    for f in &pack.files {
        let (base, rel) = if let Some(r) = f.name.strip_prefix("u/") {
            (dest.clone(), r)
        } else if let Some(r) = f.name.strip_prefix("s/") {
            (proj.join(&manifest.session_id), r)
        } else {
            continue;
        };
        let rel = safe_rel(rel).ok_or_else(|| format!("unsafe path in pack: {}", f.name))?;
        let bytes = store.assemble(f).map_err(|m| format!("{} chunk(s) of {} missing", m.len(), f.name))?;
        let out = base.join(rel);
        std::fs::create_dir_all(out.parent().unwrap_or(&base)).map_err(|e| e.to_string())?;
        std::fs::write(out, bytes).map_err(|e| e.to_string())?;
    }
    let transcript = file("transcript.jsonl")?.ok_or("pack has no transcript")?;
    std::fs::write(proj.join(format!("{}.jsonl", manifest.session_id)), transcript).map_err(|e| e.to_string())?;
    Ok(Installed { cwd: dest, manifest })
}

/// The branch a hand-off checks out; a detached source gets its own.
fn handoff_branch(m: &Manifest) -> &str {
    if m.branch == "HEAD" {
        "nerve-handoff"
    } else {
        m.branch.as_str()
    }
}

pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Start the resumed session interactively in a detached tmux session
/// (so it shows up in nerve, is injectable, and survives the agent).
/// `--fork-session` gives it a fresh id so the two devices never append
/// to one transcript.
pub(crate) fn resume_in_tmux(inst: &Installed, claude_root: &Path, sync_settings: &Path) -> Result<String, String> {
    let name = format!("nerve-{}", &inst.manifest.session_id[..8.min(inst.manifest.session_id.len())]);
    let cwd = inst.cwd.to_string_lossy().into_owned();
    // The tmux server, not this process, runs the command, with the
    // server's environment: resolve claude here and pass env with -e.
    let claude = super::bins::find("claude")?;
    let mut cmd = vec![
        claude.to_string_lossy().into_owned(),
        "--resume".into(),
        inst.manifest.session_id.clone(),
        "--fork-session".into(),
    ];
    if sync_settings.exists() {
        cmd.push("--settings".into());
        cmd.push(sync_settings.to_string_lossy().into_owned());
    }
    let mut tmux = Command::new("tmux");
    // Only for a relocated root: setting it to the default ~/.claude makes
    // Claude look for ~/.claude/.claude.json and rerun onboarding.
    let default_root = dirs::home_dir().map(|h| h.join(".claude"));
    let env_root = if default_root.as_deref() == Some(claude_root) {
        "NERVE_HANDOFF=1".to_string()
    } else {
        format!("CLAUDE_CONFIG_DIR={}", claude_root.display())
    };
    let env_path = format!("PATH={}", std::env::var("PATH").unwrap_or_default());
    let env_nerve = format!("NERVE_CONFIG_DIR={}", std::env::var("NERVE_CONFIG_DIR").unwrap_or_default());
    tmux.args(["new-session", "-d", "-s", &name, "-x", "200", "-y", "50", "-c", &cwd])
        .args(["-e", &env_root, "-e", &env_path, "-e", &env_nerve])
        .args(&cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let out = tmux.output().map_err(|e| format!("tmux: {e}"))?;
    if !out.status.success() {
        return Err(format!("tmux new-session failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    let alive = Command::new("tmux").args(["has-session", "-t", &name]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !alive {
        return Err(format!("claude exited right after starting in {cwd}"));
    }
    Ok(name)
}

/// Accept the folder-trust dialog if it is what the pane shows. Only
/// called for a session this device was explicitly asked to continue.
pub(crate) fn accept_trust_dialog(tmux_session: &str) -> bool {
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let Ok(out) = Command::new("tmux").args(["capture-pane", "-p", "-t", tmux_session]).output() else {
            return false;
        };
        let screen = String::from_utf8_lossy(&out.stdout);
        if screen.contains("Yes, I trust this folder") {
            let keys = |k: &str| Command::new("tmux").args(["send-keys", "-t", tmux_session, k]).status();
            // Option 1 is "No, exit"; option 2 is trust.
            let _ = keys("Down");
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = keys("Enter");
            return true;
        }
        if screen.contains("? for shortcuts") || screen.contains("manual mode") {
            return false;
        }
    }
    false
}

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

pub(crate) fn tempdir() -> Result<TempDir, String> {
    let p = std::env::temp_dir().join(format!("nerve-handoff-{}", super::protocol::new_id()));
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    Ok(TempDir(p))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    struct Fixture {
        tmp: tempfile::TempDir,
        src: PathBuf,
        root_a: PathBuf,
        sid: &'static str,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src/proj");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q"]);
        // Enough history that a full bundle and a thin one differ clearly.
        for i in 0..20 {
            std::fs::write(src.join(format!("f{i}.txt")), format!("{i}\n").repeat(2000)).unwrap();
            git(&src, &["add", "."]);
            git(&src, &["commit", "-qm", &format!("c{i}")]);
        }
        std::fs::write(src.join("f0.txt"), "edited\n").unwrap(); // uncommitted
        std::fs::write(src.join("new.txt"), "fresh\n").unwrap(); // untracked
        let root_a = tmp.path().join("claudeA");
        let sid = "11111111-2222-3333-4444-555555555555";
        let pdir = root_a.join("projects").join(project_slug(&src.to_string_lossy()));
        std::fs::create_dir_all(pdir.join(sid).join("subagents")).unwrap();
        let line = "{\"type\":\"assistant\",\"message\":{\"content\":\"some reply text that goes on\"}}\n";
        std::fs::write(pdir.join(format!("{sid}.jsonl")), line.repeat(8000)).unwrap();
        std::fs::write(pdir.join(sid).join("subagents/agent-1.jsonl"), "{}\n").unwrap();
        Fixture { tmp, src, root_a, sid }
    }

    /// Move every chunk `pack` needs from `from` to `to`, as the wire
    /// would; returns how many bytes that took.
    fn transfer(pack: &Pack, from: &Store, to: &Store) -> u64 {
        let have: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut sent = 0;
        for id in super::super::chunks::needed(&pack.files, &have) {
            if !to.has(&id) {
                let b = from.get(&id).unwrap();
                sent += b.len() as u64;
                to.put_verified(&id, &b).unwrap();
            }
        }
        sent
    }

    // End to end on one machine: two different absolute paths stand in
    // for two devices, each with its own chunk store.
    #[test]
    fn pack_then_unpack_reproduces_repo_state_and_installs_transcript() {
        let f = fixture();
        let (store_a, store_b) = (Store::new(&f.tmp.path().join("syncA")), Store::new(&f.tmp.path().join("syncB")));
        let pack1 = pack(&f.src, f.sid, std::slice::from_ref(&f.root_a), &[], &store_a).unwrap();
        assert_eq!((pack1.meta.repo_name.as_str(), pack1.meta.branch.as_str()), ("proj", "main"));
        assert!(pack1.files.iter().any(|x| x.name == "repo.bundle"));
        let first = transfer(&pack1, &store_a, &store_b);

        let root_b = f.tmp.path().join("claudeB");
        let dest = f.tmp.path().join("other/place/proj");
        let inst = unpack(&pack1, &dest, &store_b, &root_b, false).unwrap();
        assert_eq!(std::fs::read_to_string(inst.cwd.join("f0.txt")).unwrap(), "edited\n");
        assert_eq!(std::fs::read_to_string(inst.cwd.join("new.txt")).unwrap(), "fresh\n");
        let installed = root_b.join("projects").join(project_slug(&inst.cwd.to_string_lossy()));
        assert!(installed.join(format!("{}.jsonl", f.sid)).is_file());
        assert!(installed.join(f.sid).join("subagents/agent-1.jsonl").is_file());

        // A second hand-off onto now-dirty work is refused, not merged.
        assert!(unpack(&pack1, &dest, &store_b, &root_b, false).unwrap_err().contains("uncommitted"));

        // The second hand-off after one more commit and one more turn: the
        // target reports its tips, so only the new commit and the tail of
        // the transcript travel.
        git(&inst.cwd, &["checkout", "-q", "--", "."]);
        std::fs::remove_file(inst.cwd.join("new.txt")).unwrap();
        git(&f.src, &["add", "f0.txt"]);
        git(&f.src, &["commit", "-qm", "next"]);
        let t = f.root_a.join("projects").join(project_slug(&f.src.to_string_lossy())).join(format!("{}.jsonl", f.sid));
        let mut body = std::fs::read(&t).unwrap();
        body.extend_from_slice(b"{\"type\":\"user\",\"message\":{\"content\":\"one more turn\"}}\n");
        std::fs::write(&t, body).unwrap();
        let tips = local_tips(&dest, &pack1.meta.repo_id);
        assert!(!tips.is_empty(), "same repo id: tips reported");
        let pack2 = pack(&f.src, f.sid, std::slice::from_ref(&f.root_a), &tips, &store_a).unwrap();
        let second = transfer(&pack2, &store_a, &store_b);
        assert!(second * 10 < first, "second hand-off moved {second} bytes, first {first}");
        let inst2 = unpack(&pack2, &dest, &store_b, &root_b, false).unwrap();
        assert_eq!(std::fs::read_to_string(inst2.cwd.join("f0.txt")).unwrap(), "edited\n");
        assert_eq!(text(&inst2.cwd, &["git", "log", "-1", "--format=%s"]).unwrap(), "next");
    }

    #[test]
    fn nothing_new_means_no_bundle_and_unknown_tips_are_ignored() {
        let f = fixture();
        let store = Store::new(&f.tmp.path().join("s"));
        let head = text(&f.src, &["git", "rev-parse", "HEAD"]).unwrap();
        let p = pack(&f.src, f.sid, std::slice::from_ref(&f.root_a), std::slice::from_ref(&head), &store).unwrap();
        assert!(!p.files.iter().any(|x| x.name == "repo.bundle"), "target has every commit");
        let bogus = "a".repeat(40);
        let p = pack(&f.src, f.sid, std::slice::from_ref(&f.root_a), &[bogus], &store).unwrap();
        assert!(p.files.iter().any(|x| x.name == "repo.bundle"), "a tip we don't have doesn't shrink the bundle");
        assert!(local_tips(&f.src, "not-this-repo").is_empty(), "other repos report nothing");
    }

    #[test]
    fn pack_paths_cannot_escape() {
        assert_eq!(safe_rel("a/b.txt"), Some("a/b.txt"));
        for bad in ["../x", "/etc/passwd", "a/../../x", ""] {
            assert_eq!(safe_rel(bad), None, "{bad}");
        }
    }

    #[test]
    fn destination_is_a_place_then_path_map_then_an_explicit_root_else_refused() {
        let file = Path::new("/cfg/sync/fleet.toml");
        let offer = HandoffOffer { repo_name: "nerve".into(), src_cwd: "/Users/me/code/nerve".into(), ..Default::default() };
        let mut f = Fleet { device: "server".into(), ..Default::default() };
        let e = destination(&offer, &f, file).unwrap_err();
        assert!(e.contains("--to <place>") && e.contains("[places]") && e.contains("[path_map]") && e.contains("/cfg/sync/fleet.toml"), "{e}");
        f.handoff_root = Some("/h".into());
        assert_eq!(destination(&offer, &f, file).unwrap(), PathBuf::from("/h/nerve"));
        f.path_map.insert("/Users/me/code".into(), "/srv/ss".into());
        assert_eq!(destination(&offer, &f, file).unwrap(), PathBuf::from("/srv/ss/nerve"));
        let to = HandoffOffer { place: Some("harness".into()), ..offer.clone() };
        assert!(destination(&to, &f, file).unwrap_err().contains("no place 'harness'"));
        f.places.insert("harness".into(), "/srv/harness".into());
        assert_eq!(destination(&to, &f, file).unwrap(), PathBuf::from("/srv/harness"));
    }

    #[test]
    fn an_existing_checkout_keeps_its_branch_unless_forced() {
        let f = fixture();
        let (store_a, store_b) = (Store::new(&f.tmp.path().join("syncA")), Store::new(&f.tmp.path().join("syncB")));
        let root_b = f.tmp.path().join("claudeB");
        let dest = f.tmp.path().join("b/proj");
        let p = pack(&f.src, f.sid, std::slice::from_ref(&f.root_a), &[], &store_a).unwrap();
        transfer(&p, &store_a, &store_b);
        let inst = unpack(&p, &dest, &store_b, &root_b, false).unwrap();
        git(&inst.cwd, &["checkout", "-q", "--", "."]);
        std::fs::remove_file(inst.cwd.join("new.txt")).unwrap();

        // The target's checkout is on another branch: refused, HEAD stays.
        git(&dest, &["checkout", "-qb", "mine"]);
        let e = unpack(&p, &dest, &store_b, &root_b, false).unwrap_err();
        assert!(e.contains("on branch 'mine'") && e.contains("--force"), "{e}");
        assert_eq!(text(&dest, &["git", "rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), "mine");

        // The target's branch has a commit the hand-off lacks: refused.
        git(&dest, &["checkout", "-q", "main"]);
        std::fs::write(dest.join("local.txt"), "only here\n").unwrap();
        git(&dest, &["add", "local.txt"]);
        git(&dest, &["commit", "-qm", "target-only"]);
        let e = unpack(&p, &dest, &store_b, &root_b, false).unwrap_err();
        assert!(e.contains("has commits the hand-off doesn't"), "{e}");
        assert_eq!(text(&dest, &["git", "log", "-1", "--format=%s"]).unwrap(), "target-only");

        // --force does what it always did.
        git(&dest, &["checkout", "-q", "mine"]);
        let inst = unpack(&p, &dest, &store_b, &root_b, true).unwrap();
        assert_eq!(text(&inst.cwd, &["git", "rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), "main");
        assert_eq!(std::fs::read_to_string(inst.cwd.join("f0.txt")).unwrap(), "edited\n");
    }

    #[test]
    fn session_ids_are_validated() {
        assert!(valid_session_id("cebc016b-999f-445c-9fea-c47cff632e85"));
        assert!(!valid_session_id("../../etc"));
        assert!(!valid_session_id(""));
    }
}
