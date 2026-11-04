//! Moving a session to another device: pack (source), unpack + resume
//! (target); and starting a new one on this device for another. Both run
//! `claude` in a detached tmux session. Validated by hand in research/sync/experiments (E7) before
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
use super::protocol::{project_slug, HandoffOffer, StartSpec};
use crate::detect::git::{git_root, is_sha, repo_id};
use crate::util::fs::{safe_rel, tempdir, walk};
use crate::util::{run, text};

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

/// Find a session's transcript (and sidecar dir) under any Claude root.
fn find_transcript(roots: &[PathBuf], session_id: &str) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        std::fs::read_dir(root.join("projects")).ok()?.flatten().find_map(|d| {
            let p = d.path().join(format!("{session_id}.jsonl"));
            p.is_file().then_some(p)
        })
    })
}

/// Collect the session's files into `store`. `have` are commits the
/// target reported; the git bundle only carries what lies beyond them.
pub(crate) fn pack(
    cwd: &Path,
    session_id: &str,
    roots: &[PathBuf],
    have: &[String],
    store: &Store,
) -> Result<Pack, String> {
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

/// Restore the repo state at `dest` and install the transcript under the
/// target cwd's slug in `claude_root`. Every chunk must be in `store`.
/// An existing checkout keeps its branch: one on another branch, or
/// whose branch has commits the hand-off lacks, is refused unless `force`.
pub(crate) fn unpack(
    pack: &Pack,
    dest: &Path,
    store: &Store,
    claude_root: &Path,
    force: bool,
) -> Result<Installed, String> {
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

/// The tmux session a hand-off or a start runs in.
pub(crate) fn tmux_name(session_id: &str) -> String {
    format!("nerve-{}", super::protocol::short_id(session_id, 8))
}

/// Resume a handed-off session. `--fork-session` gives it a fresh id so
/// the two devices never append to one transcript.
pub(crate) fn resume_in_tmux(inst: &Installed, claude_root: &Path, sync_settings: &Path) -> Result<String, String> {
    let name = tmux_name(&inst.manifest.session_id);
    let args = vec!["--resume".into(), inst.manifest.session_id.clone(), "--fork-session".into()];
    claude_in_tmux(&name, &inst.cwd, args, claude_root, sync_settings)?;
    Ok(name)
}

/// Prompts longer than this are refused: tmux carries the whole command
/// in one message of at most 16 KiB, environment included.
pub(crate) const PROMPT_MAX: usize = 8 * 1024;

/// claude's arguments for a new session: the id the sender chose, then
/// the prompt after `--`, so a prompt starting with `-` is no option.
pub(crate) fn start_args(session_id: &str, prompt: &str) -> Result<Vec<String>, String> {
    if !super::protocol::is_uuid(session_id) {
        return Err("bad session id: not a UUID".into());
    }
    if prompt.trim().is_empty() || prompt.len() > PROMPT_MAX || prompt.contains('\0') {
        return Err(format!("the prompt must be 1 to {PROMPT_MAX} bytes, with no NUL"));
    }
    Ok(vec!["--session-id".into(), session_id.into(), "--".into(), prompt.into()])
}

/// Where a session started here runs: `dir` is an absolute path, `~` or
/// `~/…`, or a place. It must be an existing directory in `home`, a place
/// or `handoff_root` (where hand-offs land), and in neither a private
/// folder nor one of `never` (keys, credentials, Claude's and nerve's
/// own config).
pub(crate) fn start_dir(
    dir: &str,
    fleet: &Fleet,
    fleet_file: &Path,
    home: &Path,
    never: &[PathBuf],
) -> Result<PathBuf, String> {
    let device = &fleet.device;
    let raw = match dir.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ if dir.starts_with('/') => PathBuf::from(dir),
        _ if fleet.places.contains_key(dir) => fleet.place(dir, fleet_file)?,
        _ => return Err(format!("no folder '{dir}' on {device}: give an absolute path, ~/…, or a place name")),
    };
    let path = raw.canonicalize().map_err(|e| format!("{} on {device}: {e}", raw.display()))?;
    if !path.is_dir() {
        return Err(format!("{} on {device} is not a folder", path.display()));
    }
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut roots = vec![canon(home)];
    roots.extend(fleet.places.values().map(|p| canon(Path::new(p))));
    roots.extend(fleet.handoff_root.as_deref().map(canon));
    if !roots.iter().any(|r| path.starts_with(r)) {
        return Err(format!(
            "{} is outside the home folder on {device}; to allow it, add it under [places] in {}",
            path.display(),
            fleet_file.display()
        ));
    }
    if let Some(d) = never.iter().map(|d| canon(d)).find(|d| path.starts_with(d)) {
        return Err(format!("{device} never starts sessions in {}", d.display()));
    }
    if super::fleet::inside(&fleet.private, &path) {
        return Err(format!("{} is kept private on {device}", path.display()));
    }
    Ok(path)
}

/// Start `spec` on this device for a peer: `claude --session-id <id> --
/// <prompt>` in a detached tmux session, the way a hand-off resumes one.
pub(crate) fn start(spec: &StartSpec, fleet: &Fleet, sync_dir: &Path, nerve_config: &Path) -> Result<String, String> {
    let args = start_args(&spec.session, &spec.prompt)?;
    let home = dirs::home_dir().ok_or("no home folder")?;
    let never = super::folders::never_dirs(&[nerve_config.to_path_buf()]);
    let cwd = start_dir(&spec.dir, fleet, &Fleet::file(sync_dir), &home, &never)?;
    let root = claude_root()?;
    let name = tmux_name(&spec.session);
    claude_in_tmux(&name, &cwd, args, &root, &sync_dir.join("claude-settings.json"))?;
    if fleet.auto_trust_handoffs {
        accept_trust_dialog(&name);
    }
    Ok(format!("{} started {} in tmux session {name} at {}", fleet.device, spec.session, cwd.display()))
}

/// Where sessions started or resumed here keep their data.
pub(crate) fn claude_root() -> Result<PathBuf, String> {
    crate::detect::claude_roots()
        .into_iter()
        .next()
        .ok_or_else(|| "no Claude config directory: no home directory and no $CLAUDE_CONFIG_DIR".into())
}

/// `tmux new-session` arguments that run `claude <args>` detached in
/// `cwd`. Given as several arguments, the command is exec'd directly,
/// never through a shell, so each of `args` reaches claude unchanged.
fn tmux_argv(name: &str, cwd: &Path, env: &[String], claude: &Path, args: &[String]) -> Vec<String> {
    let mut argv: Vec<String> =
        ["new-session", "-d", "-s", name, "-x", "200", "-y", "50", "-c"].map(String::from).into();
    argv.push(cwd.to_string_lossy().into_owned());
    for e in env {
        argv.extend(["-e".to_string(), e.clone()]);
    }
    argv.push(claude.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    argv
}

/// Run `claude <args>` interactively in a detached tmux session `name`
/// at `cwd`, so it shows up in nerve, takes prompts and survives the agent.
fn claude_in_tmux(
    name: &str,
    cwd: &Path,
    args: Vec<String>,
    claude_root: &Path,
    sync_settings: &Path,
) -> Result<(), String> {
    // The tmux server, not this process, runs the command, with the
    // server's environment: resolve claude here and pass env with -e.
    let claude = super::bins::find("claude")?;
    let mut all = Vec::new();
    if sync_settings.exists() {
        all.push("--settings".into());
        all.push(sync_settings.to_string_lossy().into_owned());
    }
    all.extend(args);
    // Only for a relocated root: setting it to the default ~/.claude makes
    // Claude look for ~/.claude/.claude.json and rerun onboarding.
    let env_root = if crate::detect::default_root().as_deref() == Some(claude_root) {
        "NERVE_HANDOFF=1".to_string()
    } else {
        format!("CLAUDE_CONFIG_DIR={}", claude_root.display())
    };
    let env = [
        env_root,
        format!("PATH={}", std::env::var("PATH").unwrap_or_default()),
        format!("NERVE_CONFIG_DIR={}", std::env::var("NERVE_CONFIG_DIR").unwrap_or_default()),
    ];
    let out = Command::new("tmux")
        .args(tmux_argv(name, cwd, &env, &claude, &all))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("tmux: {e}"))?;
    if !out.status.success() {
        return Err(format!("tmux new-session failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    let alive = Command::new("tmux")
        .args(["has-session", "-t", &format!("={name}")])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !alive {
        return Err(format!("claude exited right after starting in {}", cwd.display()));
    }
    Ok(())
}

/// Accept the folder-trust dialog if it is what the pane shows. Only
/// called for a session this device was explicitly asked to continue or
/// start.
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
        let offer =
            HandoffOffer { repo_name: "nerve".into(), src_cwd: "/Users/me/code/nerve".into(), ..Default::default() };
        let mut f = Fleet { device: "server".into(), ..Default::default() };
        let e = destination(&offer, &f, file).unwrap_err();
        assert!(
            e.contains("--to <place>")
                && e.contains("[places]")
                && e.contains("[path_map]")
                && e.contains("/cfg/sync/fleet.toml"),
            "{e}"
        );
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
    fn a_prompt_is_one_argument_whatever_it_holds() {
        let sid = "0f8e2b1c-3d4a-4b5c-9d6e-7f8091a2b3c4";
        let prompt = "say \"hi\"; $(touch /tmp/pwned) `id` && echo 'x'\nsecond line\n--dangerously-skip-permissions";
        let args = start_args(sid, prompt).unwrap();
        assert_eq!(args, ["--session-id", sid, "--", prompt]);
        let argv = tmux_argv("nerve-0f8e2b1c", Path::new("/w d"), &["A=1".into()], Path::new("/bin/claude"), &args);
        assert_eq!(
            argv,
            [
                "new-session",
                "-d",
                "-s",
                "nerve-0f8e2b1c",
                "-x",
                "200",
                "-y",
                "50",
                "-c",
                "/w d",
                "-e",
                "A=1",
                "/bin/claude",
                "--session-id",
                sid,
                "--",
                prompt
            ]
        );
        assert!(start_args("../x", "hi").is_err());
        assert!(start_args(sid, "  \n").is_err());
        assert!(start_args(sid, "a\0b").is_err());
        assert!(start_args(sid, &"x".repeat(PROMPT_MAX + 1)).is_err());
    }

    #[test]
    fn a_session_starts_only_in_an_allowed_existing_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let (code, secret, private, srv) =
            (home.join("code"), home.join(".ssh"), home.join("code/secret"), root.join("srv/proj"));
        for d in [&code, &secret, &private, &srv] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join("file.txt"), "x").unwrap();
        let file = Path::new("/cfg/sync/fleet.toml");
        let mut f = Fleet { device: "beta".into(), private: vec![private.display().to_string()], ..Default::default() };
        let never = [secret.clone()];
        let at = |d: &str, f: &Fleet| start_dir(d, f, file, &home, &never);
        assert_eq!(at("~", &f).unwrap(), home);
        assert_eq!(at("~/code", &f).unwrap(), code);
        assert_eq!(at(&code.join("../code").display().to_string(), &f).unwrap(), code);
        assert!(at("~/missing", &f).is_err());
        assert!(at("~/file.txt", &f).unwrap_err().contains("not a folder"));
        assert!(at("code", &f).unwrap_err().contains("place name"), "relative paths are not guessed at");
        assert!(at("~/.ssh", &f).unwrap_err().contains("never starts sessions"));
        assert!(at("~/code/secret", &f).unwrap_err().contains("kept private"));
        let e = at(&srv.display().to_string(), &f).unwrap_err();
        assert!(e.contains("outside the home folder") && e.contains("[places]"), "{e}");
        assert!(at("/", &f).is_err());
        f.places.insert("proj".into(), srv.display().to_string());
        assert_eq!(at("proj", &f).unwrap(), srv);
        assert_eq!(at(&srv.display().to_string(), &f).unwrap(), srv, "a place admits its path too");
        // A symlink in home that points outside it lands outside: refused.
        std::os::unix::fs::symlink(root.join("srv"), home.join("link")).unwrap();
        f.places.clear();
        assert!(at("~/link/proj", &f).unwrap_err().contains("outside the home folder"));
    }

    #[test]
    fn session_ids_are_validated() {
        assert!(valid_session_id("cebc016b-999f-445c-9fea-c47cff632e85"));
        assert!(!valid_session_id("../../etc"));
        assert!(!valid_session_id(""));
    }
}
