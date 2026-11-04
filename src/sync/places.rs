//! Named places: `[places] proj = "/abs/path"` on every device,
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
//!
//! Big files take a third way, **bulk**: files at or over `[xfer] bulk_over`
//! (and any transfer the relay fails to carry) go straight between the
//! devices over rsync/ssh, run by whichever side has a route (`[ssh]`). The
//! relay carries only the offer, the other side's path and the outcome.
//! The rules above hold here too: roots are relative and stay inside the
//! place, symlinks are neither followed nor sent, and `--delete` reaches
//! only into the directories that were named. Each root is its own rsync,
//! so neither the path nor `--delete` depends on `--relative`, which
//! macOS's openrsync implements without the `/./` anchor.

use std::collections::{BTreeSet, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::chunks::{FileRef, Store};
use super::protocol::{BulkOffer, RefMap};
use crate::detect::git::{git_root, is_sha, repo_id};
use crate::util::fs::{safe_rel, tempdir, walk};
use crate::util::text::{format_bytes, shell_quote};
use crate::util::{run, text};

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
    safe_rel(r)
        .map(String::from)
        .ok_or_else(|| format!("{root}: paths must be relative to the place and stay inside it"))
}

fn join(base: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    }
}

/// Files as (path relative to the place, full path).
pub(crate) type Picked = Vec<(String, PathBuf)>;

/// Sender: the regular files under `roots`, each once and sorted by path
/// relative to the place, and which roots were directories. A symlinked
/// root is refused; symlinks inside one are skipped.
pub(crate) fn pick(base: &Path, roots: &[String]) -> Result<(Picked, Vec<String>), String> {
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
    Ok((picked, dirs))
}

/// Sender: chunk every regular file under `roots` into `store`.
/// Symlinks are not followed and not sent.
pub(crate) fn pack_files(base: &Path, roots: &[String], delete: bool, store: &Store) -> Result<XferPack, String> {
    if !base.is_dir() {
        return Err(format!("{} is not a directory", base.display()));
    }
    let (picked, dirs) = pick(base, roots)?;
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
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("refusing to write through symlink {}", cur.display()))
            }
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
        let same = std::fs::symlink_metadata(&dest)
            .is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64 && m.permissions().mode() & 0o777 == mode)
            && std::fs::read(&dest).is_ok_and(|b| b == bytes);
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
            if !d.is_empty()
                && (check_parents(&base, &format!("{d}/x"), false).is_err()
                    || !std::fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()))
            {
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
    text(
        repo,
        &[
            "git",
            "for-each-ref",
            "--sort=-committerdate",
            "--count=500",
            "--format=%(objectname)",
            "refs/heads",
            "refs/remotes",
        ],
    )
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
        return Err(format!("{dst}: destination must be a full ref name such as refs/remotes/laptop/main"));
    }
    if text(repo, &["git", "symbolic-ref", "-q", "HEAD"]).is_ok_and(|h| h == dst) {
        return Err(format!(
            "{dst} is checked out in {}; pick a ref that isn't, such as refs/remotes/<peer>/…",
            repo.display()
        ));
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
        Ok(_) => Some(
            store
                .add_file("refs.bundle", &std::fs::read(&file).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?,
        ),
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
        run(&repo, &["git", "cat-file", "-e", &format!("{}^{{commit}}", r.sha)])
            .map_err(|_| format!("commit {} for {} did not arrive", &r.sha[..12], r.dst))?;
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

// ---- bulk: big files over rsync ----

const ATTEMPTS: u32 = 3;
const SSH: &str = "ssh -o BatchMode=yes -o ConnectTimeout=15";
/// How often a running rsync says it is still going, so a follower
/// can tell a long copy from a stalled one.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(60);

/// Sender: what the roots hold. A symlinked root is refused, as the
/// relay path refuses it.
pub(crate) fn scan(base: &Path, roots: &[String]) -> Result<BulkOffer, String> {
    let (picked, dirs) = pick(base, roots)?;
    let bytes = picked.iter().filter_map(|(_, p)| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    Ok(BulkOffer { dirs, files: picked.len(), bytes, src: None })
}

/// One end of a copy: a local directory, or one on an ssh host.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum End {
    Local(PathBuf),
    Remote { host: String, path: PathBuf },
}

/// The rsync for one root. `quote`: the client passes remote paths
/// through the remote shell as they are (openrsync, rsync < 3.2.4).
pub(crate) fn argv(src: &End, dst: &End, rel: &str, is_dir: bool, delete: bool, quote: bool) -> Vec<String> {
    let spec = |e: &End| {
        let (host, base) = match e {
            End::Local(p) => (None, p),
            End::Remote { host, path } => (Some(host), path),
        };
        let mut s = if rel.is_empty() { base.display().to_string() } else { base.join(rel).display().to_string() };
        if is_dir {
            s.push('/');
        }
        match host {
            Some(h) => format!("{h}:{}", if quote { shell_quote(&s) } else { s }),
            None => s,
        }
    };
    // -rtp, not -a: no -l (symlinks are skipped, never sent), no -D, and
    // owners stay whoever the receiving user is.
    let mut a: Vec<String> = ["rsync", "-rtp", "--partial", "-e", SSH].iter().map(|s| s.to_string()).collect();
    if delete && is_dir {
        a.push("--delete".into());
    }
    a.push(spec(src));
    a.push(spec(dst));
    a
}

/// Whether this device's rsync needs remote paths quoted for the
/// remote shell: GNU rsync from 3.2.4 escapes them itself.
fn rsync_quotes() -> bool {
    static Q: OnceLock<bool> = OnceLock::new();
    *Q.get_or_init(|| {
        let v = Command::new("rsync")
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        needs_quoting(&v)
    })
}

fn needs_quoting(version: &str) -> bool {
    let first = version.lines().next().unwrap_or("");
    if first.contains("openrsync") {
        return true;
    }
    let ver = first.split_whitespace().skip_while(|w| *w != "version").nth(1).unwrap_or("0");
    let n: Vec<u32> = ver.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    (n.first().copied().unwrap_or(0), n.get(1).copied().unwrap_or(0), n.get(2).copied().unwrap_or(0)) < (3, 2, 4)
}

/// Copy every root, each up to three times (`--partial` resumes a
/// cut-off file). `beat` hears a line every `HEARTBEAT` while it runs.
pub(crate) fn copy(
    src: &End,
    dst: &End,
    roots: &[String],
    offer: &BulkOffer,
    delete: bool,
    beat: &dyn Fn(String),
) -> Result<String, String> {
    let started = Instant::now();
    let quote = rsync_quotes();
    for root in roots {
        let rel = norm_root(root)?;
        if let End::Local(base) = dst {
            ready_for(base, &rel)?;
        }
        let is_dir = offer.dirs.contains(&rel);
        let a = argv(src, dst, &rel, is_dir, delete, quote);
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            match rsync_one(&a, &|secs| {
                beat(format!("rsync {} attempt {attempt}: {secs}s", if rel.is_empty() { "." } else { &rel }))
            }) {
                Ok(()) => {
                    last.clear();
                    break;
                }
                Err(e) => {
                    last = e;
                    if attempt < ATTEMPTS {
                        beat(format!(
                            "rsync {} attempt {attempt} failed ({last}); retrying",
                            if rel.is_empty() { "." } else { &rel }
                        ));
                        std::thread::sleep(Duration::from_secs(5 * attempt as u64));
                    }
                }
            }
        }
        if !last.is_empty() {
            return Err(format!("rsync failed after {ATTEMPTS} attempts: {last}"));
        }
    }
    Ok(format!(
        "via rsync, {} file(s), {} in {:.1}s",
        offer.files,
        format_bytes(offer.bytes),
        started.elapsed().as_secs_f64()
    ))
}

fn rsync_one(argv: &[String], beat: &dyn Fn(u64)) -> Result<(), String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start rsync: {e}"))?;
    // Read stderr off-thread, or a chatty rsync blocks on a full pipe.
    let mut err = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let started = Instant::now();
    let mut next = HEARTBEAT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
        if started.elapsed() >= next {
            beat(started.elapsed().as_secs());
            next += HEARTBEAT;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let stderr = reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    // "skipping non-regular file" is rsync obeying the symlink rule.
    let why = stderr.lines().rfind(|l| !l.starts_with("skipping non-regular file")).unwrap_or("").trim().to_string();
    Err(if why.is_empty() { format!("rsync exited with {status}") } else { why })
}

/// The far side's answer to `nerve sync place`: its own path of the
/// place, and (as a source) what the roots hold.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct PlaceInfo {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) scan: Option<BulkOffer>,
}

/// When the relay cannot carry the coordination: ask the far side's
/// own nerve, over the same ssh route, where its place is (and check
/// the roots there by its own rules). A service's or a bare ssh
/// session's PATH may lack the per-user bin directories nerve sits in.
pub(crate) fn remote_place(host: &str, place: &str, as_src: bool, roots: &[String]) -> Result<PlaceInfo, String> {
    let extra: Vec<String> =
        super::bins::candidates(Some(Path::new("$HOME"))).iter().map(|d| d.display().to_string()).collect();
    let mut cmd = format!(
        "PATH=\"{}:$PATH\" exec nerve sync place {} {}",
        extra.join(":"),
        shell_quote(place),
        if as_src { "--src" } else { "--dst" }
    );
    for r in roots {
        cmd.push(' ');
        cmd.push_str(&shell_quote(r));
    }
    let out = Command::new("ssh")
        .args(SSH.split_whitespace().skip(1))
        .args([host, &cmd])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ssh {host}: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{host}: {}", e.lines().last().unwrap_or("nerve sync place failed").trim()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("{host}: unreadable answer from nerve sync place: {e}"))
}

/// `nerve sync place <name> --src|--dst <root>...`, on the far side.
pub(crate) fn place_info(base: &Path, as_src: bool, roots: &[String]) -> Result<PlaceInfo, String> {
    let scan = if as_src {
        Some(scan(base, roots)?)
    } else {
        for r in roots {
            ready_for(base, &norm_root(r)?)?;
        }
        None
    };
    Ok(PlaceInfo { path: base.display().to_string(), scan })
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
            let body = (0..400).map(|j| format!("unit {i} line {j}")).collect::<Vec<_>>().join("\n") + "\n";
            std::fs::write(a.join(format!("units/S5/f{i}.md")), body).unwrap();
        }
        std::fs::write(a.join("bin/check.mjs"), "#!/usr/bin/env node\n").unwrap();
        std::fs::set_permissions(a.join("bin/check.mjs"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let (sa, sb) = (Store::new(&t.path().join("sa")), Store::new(&t.path().join("sb")));
        let roots = vec!["units".to_string(), "bin/check.mjs".to_string()];

        let p1 = pack_files(&a, &roots, false, &sa).unwrap();
        let (first_n, first_bytes) = move_chunks(&p1, &sa, &sb);
        let got = apply_files(&p1, &b, &sb).unwrap();
        assert_eq!((got.written, got.unchanged), (41, 0));
        assert_eq!(std::fs::read(b.join("units/S5/f7.md")).unwrap(), std::fs::read(a.join("units/S5/f7.md")).unwrap());
        assert_eq!(mode(&b.join("bin/check.mjs")), 0o755);

        // One file changes: only its chunks move, only it is rewritten.
        let mut body = std::fs::read_to_string(a.join("units/S5/f3.md")).unwrap();
        body.push_str("one more line\n");
        std::fs::write(a.join("units/S5/f3.md"), body).unwrap();
        let p2 = pack_files(&a, &roots, false, &sa).unwrap();
        let (n, bytes) = move_chunks(&p2, &sa, &sb);
        assert!(
            n <= 2 && bytes * 20 < first_bytes,
            "repeat moved {n} chunks / {bytes} B; first {first_n} / {first_bytes} B"
        );
        let got = apply_files(&p2, &b, &sb).unwrap();
        assert_eq!((got.written, got.unchanged), (1, 40));
        // No temp files left behind.
        assert!(std::fs::read_dir(b.join("units/S5"))
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with('.')));
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
        git(&a, &["checkout", "-qb", "feature"]);
        std::fs::write(a.join("g"), "new\n").unwrap();
        git(&a, &["add", "."]);
        git(&a, &["commit", "-qm", "on feature"]);
        std::fs::write(b.join("f"), "local edit\n").unwrap(); // dirty worktree on the target
        let head_before = text(&b, &["git", "rev-parse", "HEAD"]).unwrap();
        let refs_before = text(&b, &["git", "for-each-ref", "--format=%(refname)"]).unwrap();
        let (sa, sb) = (Store::new(&t.path().join("sa")), Store::new(&t.path().join("sb")));

        let map = vec![RefMap { src: "feature".into(), dst: "refs/remotes/laptop/feature".into() }];
        let tips = repo_tips(&b);
        let p = pack_refs(&a, &map, &tips, &sa).unwrap();
        let (n, _) = move_chunks(&p, &sa, &sb);
        assert!(n >= 1);
        let lines = apply_refs(&p, &b, &sb).unwrap();
        assert!(lines[0].contains("refs/remotes/laptop/feature") && lines[0].contains("(new)"), "{lines:?}");
        assert_eq!(
            text(&b, &["git", "rev-parse", "refs/remotes/laptop/feature"]).unwrap(),
            text(&a, &["git", "rev-parse", "feature"]).unwrap()
        );
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

    #[test]
    fn scan_counts_files_and_refuses_what_the_relay_refuses() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/f"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.path().join("top"), b"12").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", d.path().join("a/link")).unwrap();
        std::os::unix::fs::symlink(d.path().join("a"), d.path().join("ln")).unwrap();
        let s = scan(d.path(), &["a".into(), "top".into(), "a/b".into()]).unwrap();
        assert_eq!((s.files, s.bytes), (2, 1002), "symlinks inside are not counted (or sent)");
        assert_eq!(s.dirs, ["a", "a/b"]);
        assert!(scan(d.path(), &["ln".into()]).unwrap_err().contains("symlink"));
        assert!(scan(d.path(), &["../x".into()]).unwrap_err().contains("inside"));
        assert!(scan(d.path(), &["/etc".into()]).is_err());
        assert!(scan(d.path(), &["nope".into()]).unwrap_err().contains("does not exist"));
    }

    #[test]
    fn argv_copies_one_root_without_links_and_deletes_only_inside_named_dirs() {
        let src = End::Local("/h/place".into());
        let dst = End::Remote { host: "server".into(), path: "/home/p/it's here".into() };
        let a = argv(&src, &dst, "out/units", true, true, true);
        assert_eq!(&a[..5], ["rsync", "-rtp", "--partial", "-e", SSH]);
        assert!(!a.iter().any(|x| x == "-a" || x == "-l" || x == "-L" || x == "--relative"), "{a:?}");
        assert!(a.contains(&"--delete".to_string()));
        assert_eq!(a[a.len() - 2], "/h/place/out/units/");
        assert_eq!(a[a.len() - 1], r"server:'/home/p/it'\''s here/out/units/'");
        // A file root never deletes, and a GNU client is given the path as it is.
        let f = argv(&src, &dst, "LOG.md", false, true, false);
        assert!(!f.contains(&"--delete".to_string()));
        assert_eq!(f[f.len() - 1], "server:/home/p/it's here/LOG.md");
        // The whole place.
        let all = argv(&dst, &End::Local("/m/p".into()), "", true, false, false);
        assert_eq!(all[all.len() - 2..], ["server:/home/p/it's here/", "/m/p/"]);
    }

    #[test]
    fn quoting_follows_the_client() {
        assert!(needs_quoting("openrsync: protocol version 29\nrsync version 2.6.9 compatible"));
        assert!(needs_quoting("rsync  version 3.2.3  protocol version 31"));
        assert!(!needs_quoting("rsync  version 3.2.7  protocol version 31"));
        assert!(!needs_quoting("rsync  version 3.5.0  protocol version 32"));
    }

    #[test]
    fn a_local_copy_skips_symlinks_and_refuses_writing_through_one() {
        if Command::new("rsync").arg("--version").output().is_err() {
            return;
        }
        let s = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(s.path().join("a/b")).unwrap();
        std::fs::write(s.path().join("a/b/f"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", s.path().join("a/link")).unwrap();
        std::fs::create_dir_all(d.path().join("a")).unwrap();
        std::fs::write(d.path().join("a/stale"), b"old").unwrap();
        std::fs::write(d.path().join("keep"), b"k").unwrap();
        let offer = scan(s.path(), &["a".into()]).unwrap();
        let (src, dst) = (End::Local(s.path().into()), End::Local(d.path().into()));
        let msg = copy(&src, &dst, &["a".into()], &offer, true, &|_| {}).unwrap();
        assert!(msg.starts_with("via rsync, 1 file(s)"), "{msg}");
        assert_eq!(std::fs::read(d.path().join("a/b/f")).unwrap(), b"x");
        assert!(std::fs::symlink_metadata(d.path().join("a/link")).is_err(), "symlink not sent");
        assert!(!d.path().join("a/stale").exists(), "--delete inside the named dir");
        assert!(d.path().join("keep").exists(), "--delete stays inside it");
        // A symlinked directory on the receiver is never written through.
        std::fs::remove_dir_all(d.path().join("a")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), d.path().join("a")).unwrap();
        assert!(copy(&src, &dst, &["a".into()], &offer, false, &|_| {}).unwrap_err().contains("symlink"));
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }
}
