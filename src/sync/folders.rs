//! Synced folders (`[[folders]]` in fleet.toml): a directory kept the
//! same on several devices. Each device remembers what is in its copy
//! (the index: path → size, mtime, sha256) and, per member, which
//! version of every file that member is known to hold. What a member
//! lacks travels as an ordinary transfer into the folder's place: small
//! files over the relay in batches, a file at or over `[xfer] bulk_over`
//! alone over rsync.
//!
//! Both devices changed a file since they last agreed on it: the copy
//! from the device whose key sorts first keeps the name, and the other
//! one lands beside it as `<file>.conflict-<device>`, so nothing is
//! lost. Every device decides alike, so both copies end up everywhere.
//! A file deleted on one side is deleted on the other only while that
//! side still has the version the deleting device knew.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use bitcoin_hashes::{sha256, Hash, HashEngine};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use super::protocol::FolderPush;
use crate::util::fs::safe_rel;

/// When a folder sends its changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum When {
    /// A session working in the folder finishes a turn.
    #[default]
    Turn,
    /// A file in it changes.
    Watch,
    /// Only on `s` in the TUI or `nerve sync <folder>`.
    Manual,
}

impl When {
    pub(crate) const ALL: [When; 3] = [When::Turn, When::Watch, When::Manual];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Turn => "When a turn ends",
            Self::Watch => "When files change",
            Self::Manual => "Only when I ask",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "turn" => Some(Self::Turn),
            "watch" => Some(Self::Watch),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// What the devices sharing a folder may sync next without a person's
/// yes. Anything else waits for one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Others {
    /// Nothing: every new folder asks first.
    #[default]
    Ask,
    /// Folders inside this one's parent folder.
    Inside,
    /// Any folder that isn't private or one nerve never syncs.
    Any,
}

impl Others {
    pub(crate) const ALL: [Others; 3] = [Others::Ask, Others::Inside, Others::Any];

    /// `parent` is the folder's parent, as the reader knows it.
    pub(crate) fn label(self, parent: &str) -> String {
        match self {
            Self::Ask => "Ask me first".into(),
            Self::Inside => format!("Allow folders inside {parent}"),
            Self::Any => "Allow any folder that isn't private".into(),
        }
    }

    /// One line under a folder on the Sync page.
    pub(crate) fn short(self, parent: &str) -> String {
        match self {
            Self::Ask => "Other folders ask first".into(),
            Self::Inside => format!("Other folders inside {parent} sync without asking"),
            Self::Any => "Any folder that isn't private syncs without asking".into(),
        }
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Inside => "inside",
            Self::Any => "any",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.word() == s)
    }

    /// May a new folder landing at `path` sync without asking, given a
    /// folder at `base` with these rules?
    pub(crate) fn covers(self, base: &Path, path: &Path) -> bool {
        match self {
            Self::Ask => false,
            Self::Inside => base.parent().is_some_and(|p| path.starts_with(p) && path != p),
            Self::Any => true,
        }
    }
}

/// Folders nerve never syncs or starts a session in, whatever a rule
/// says: keys and credentials, Claude's own settings, and `extra`
/// (nerve's config).
pub(crate) fn never_dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    let home = std::env::var("HOME").ok().filter(|h| h.starts_with('/')).map(PathBuf::from);
    let mut list: Vec<PathBuf> = extra.to_vec();
    if let Some(h) = &home {
        list.extend(
            [".ssh", ".gnupg", ".aws", ".kube", ".docker", ".config/gcloud", ".netrc"].iter().map(|d| h.join(d)),
        );
    }
    list.extend(crate::detect::claude_roots());
    list
}

/// The folder of `never_dirs` that `path` is in, or contains: a synced
/// folder holding one would carry it along.
pub(crate) fn never(path: &Path, extra: &[PathBuf]) -> Option<PathBuf> {
    never_dirs(extra).into_iter().find(|d| path.starts_with(d) || d.starts_with(path))
}

/// One file of the index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) size: u64,
    pub(crate) mtime_ns: i64,
    pub(crate) id: String,
}

/// How a folder stands, in a few words, and whether it wants a look.
/// A transfer under way is not covered: callers show its progress.
pub(crate) fn status(v: &View, now: u64) -> (String, bool) {
    let files = |n: u32, what: &str| if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") };
    if !v.held.is_empty() {
        return (format!("Waiting for a yes for {}", crate::util::text::list_join(&v.held)), true);
    }
    if let Some(e) = &v.error {
        return (format!("Not synced: {e}"), true);
    }
    if let Some((dev, n)) = v.behind.first() {
        return (format!("{} on {dev}", files(*n, "newer file")), true);
    }
    if !v.offline.is_empty() {
        return (format!("Waiting for {}", crate::util::text::list_join(&v.offline)), false);
    }
    if v.unsent > 0 {
        return (format!("{} to send", files(v.unsent, "change")), false);
    }
    if v.last_sync == 0 {
        return ("Not synced yet".into(), false);
    }
    (format!("In sync, {}", crate::util::text::format_age(now.saturating_sub(v.last_sync) as f64)), false)
}

/// One finished transfer, for "What went where".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Moved {
    pub(crate) at: u64,
    /// The file, or "N files".
    pub(crate) what: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) bytes: u64,
    /// Went over rsync rather than the relay.
    pub(crate) direct: bool,
}

const LOG_KEEP: usize = 20;

/// How a folder stands, for this device's Sync page: devices by this
/// device's names for them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct View {
    pub(crate) when: When,
    #[serde(default)]
    pub(crate) others: Others,
    /// Members that haven't said yes to it yet.
    #[serde(default)]
    pub(crate) held: Vec<String>,
    /// The other members.
    pub(crate) with: Vec<String>,
    pub(crate) offline: Vec<String>,
    /// Members holding changes this device lacks: (name, files).
    pub(crate) behind: Vec<(String, u32)>,
    /// Files waiting to go out, to the member that lacks most.
    pub(crate) unsent: u32,
    pub(crate) moving: Option<Moving>,
    pub(crate) error: Option<String>,
    pub(crate) last_sync: u64,
    pub(crate) log: Vec<Moved>,
    /// Files a sync replaced, newest first: (version id, when, path, why).
    #[serde(default)]
    pub(crate) versions: Vec<(String, u64, String, String)>,
}

/// A transfer under way.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Moving {
    pub(crate) what: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) bytes: u64,
    /// Chunks so far and in all; both 0 when rsync carries it.
    pub(crate) done: usize,
    pub(crate) total: usize,
    pub(crate) direct: bool,
}

/// What this device knows about one folder: `<sync>/folders/<name>.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Book {
    pub(crate) index: BTreeMap<String, Entry>,
    /// Member key (hex) → path → the file id that member holds.
    pub(crate) peers: BTreeMap<String, BTreeMap<String, String>>,
    /// Newest last.
    pub(crate) log: Vec<Moved>,
    pub(crate) last_sync: u64,
}

impl Book {
    fn file(sync_dir: &Path, name: &str) -> PathBuf {
        sync_dir.join("folders").join(format!("{name}.json"))
    }

    pub(crate) fn load(sync_dir: &Path, name: &str) -> Self {
        std::fs::read_to_string(Self::file(sync_dir, name))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save(&self, sync_dir: &Path, name: &str) -> std::io::Result<()> {
        let path = Self::file(sync_dir, name);
        std::fs::create_dir_all(path.parent().unwrap_or(sync_dir))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    pub(crate) fn forget(sync_dir: &Path, name: &str) {
        let _ = std::fs::remove_file(Self::file(sync_dir, name));
    }

    pub(crate) fn record(&mut self, m: Moved) {
        self.last_sync = m.at;
        self.log.push(m);
        let over = self.log.len().saturating_sub(LOG_KEEP);
        self.log.drain(..over);
    }

    /// `peer` now holds these files and no longer the gone ones.
    pub(crate) fn holds(&mut self, peer: &str, push: &FolderPush) {
        let had = self.peers.entry(peer.to_string()).or_default();
        for (rel, id) in &push.files {
            had.insert(rel.clone(), id.clone());
        }
        for (rel, _) in &push.gone {
            had.remove(rel);
        }
    }

    /// What `peer` lacks: files whose version differs from what it is
    /// known to hold, and files it holds that are gone here.
    pub(crate) fn changes_for(&self, peer: &str) -> Changes {
        let empty = BTreeMap::new();
        let had = self.peers.get(peer).unwrap_or(&empty);
        Changes {
            files: self
                .index
                .iter()
                .filter(|(rel, e)| had.get(*rel) != Some(&e.id))
                .map(|(rel, e)| (rel.clone(), e.id.clone(), e.size))
                .collect(),
            gone: had
                .iter()
                .filter(|(rel, _)| !self.index.contains_key(*rel))
                .map(|(r, i)| (r.clone(), i.clone()))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Changes {
    /// (path, id, size)
    pub(crate) files: Vec<(String, String, u64)>,
    /// (path, the id the peer holds)
    pub(crate) gone: Vec<(String, String)>,
}

impl Changes {
    pub(crate) fn count(&self) -> usize {
        self.files.len() + self.gone.len()
    }
}

/// Paths per transfer, so the offer fits one relay message.
const BATCH_PATHS: usize = 150;

/// The next transfer to a peer: the deletions and small files up to a
/// quarter of `bulk_over` (at least one), or else one big file alone,
/// which then goes over rsync.
pub(crate) fn next_batch(c: &Changes, bulk_over: u64) -> FolderPush {
    let gone: Vec<(String, String)> = c.gone.iter().take(BATCH_PATHS).cloned().collect();
    let mut files = Vec::new();
    let mut bytes = 0u64;
    for (rel, id, size) in c.files.iter().filter(|f| f.2 < bulk_over) {
        if gone.len() + files.len() >= BATCH_PATHS || (!files.is_empty() && bytes + size > bulk_over / 4) {
            break;
        }
        files.push((rel.clone(), id.clone()));
        bytes += size;
    }
    if files.is_empty() && gone.len() < BATCH_PATHS {
        if let Some((rel, id, size)) = c.files.iter().find(|f| f.2 >= bulk_over) {
            files.push((rel.clone(), id.clone()));
            bytes = *size;
        }
    }
    FolderPush { files, gone, bytes }
}

/// Our own temp and hold files, which neither sync nor wake the watcher.
fn ours(rel: &str) -> bool {
    rel.rsplit('/').next().is_some_and(|leaf| leaf.contains(".nerve-"))
}

/// Re-read the folder, hashing only files whose size or mtime changed
/// since `old`. Inside a git repo the files are git's: tracked plus
/// untracked ones not ignored. Symlinks are skipped, as transfers skip
/// them. Blocking: run it off the agent's loop.
pub(crate) fn scan(base: &Path, old: &BTreeMap<String, Entry>) -> Result<BTreeMap<String, Entry>, String> {
    if !base.is_dir() {
        return Err(format!("{} is not a directory", base.display()));
    }
    let rels = if base.join(".git").exists() { git_files(base)? } else { walk(base) };
    let mut out = BTreeMap::new();
    for rel in rels {
        if ours(&rel) || safe_rel(&rel).is_none() {
            continue;
        }
        let p = base.join(&rel);
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if !m.is_file() {
            continue;
        }
        let mtime_ns = m.mtime() * 1_000_000_000 + m.mtime_nsec();
        let id = match old.get(&rel) {
            Some(e) if e.size == m.len() && e.mtime_ns == mtime_ns => e.id.clone(),
            _ => match hash_file(&p) {
                Ok(id) => id,
                Err(_) => continue,
            },
        };
        out.insert(rel, Entry { size: m.len(), mtime_ns, id });
    }
    Ok(out)
}

fn git_files(base: &Path) -> Result<Vec<String>, String> {
    let raw = crate::util::run(base, &["git", "ls-files", "-z", "-co", "--exclude-standard"])?;
    let mut v: Vec<String> =
        raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    v.sort();
    v.dedup();
    Ok(v)
}

fn walk(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    crate::util::fs::walk_skipping(base, "", &|name| name == ".git", &mut out);
    out.into_iter().map(|(rel, _)| rel).collect()
}

/// sha256 of a file, read in pieces so big files don't fill memory.
/// The same id `chunks::id_of` gives the whole contents.
fn hash_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut engine = sha256::Hash::engine();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        engine.input(&buf[..n]);
    }
    Ok(sha256::Hash::from_engine(engine).to_string())
}

fn current(base: &Path, rel: &str) -> Option<String> {
    let p = base.join(rel);
    std::fs::symlink_metadata(&p).ok().filter(|m| m.is_file())?;
    hash_file(&p).ok()
}

/// `rel` with a device's name added, safe for any file system.
fn conflict_name(base: &Path, rel: &str, device: &str) -> String {
    let dev: String =
        device.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    let first = format!("{rel}.conflict-{dev}");
    if !base.join(&first).exists() {
        return first;
    }
    (2..).map(|n| format!("{first}-{n}")).find(|c| !base.join(c).exists()).unwrap_or(first)
}

/// What the receiver does around one incoming transfer.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    /// Files this device already has as sent: the sender leaves them out.
    pub(crate) skip: Vec<String>,
    /// Incoming wins: our version, copied aside as a conflict file.
    pub(crate) copied: Vec<(String, String)>,
    /// Ours wins: (path, hidden copy of ours, where the incoming goes).
    pub(crate) held: Vec<(String, String, String)>,
    /// Deletions to make: (path, the id it must still have).
    pub(crate) delete: Vec<(String, String)>,
    /// Files here that the incoming versions replace.
    pub(crate) replaced: Vec<String>,
}

impl Plan {
    pub(crate) fn conflicts(&self) -> usize {
        self.copied.len() + self.held.len()
    }
}

fn copy_aside(base: &Path, rel: &str, to: &str) -> Result<(), String> {
    std::fs::copy(base.join(rel), base.join(to)).map(|_| ()).map_err(|e| format!("{rel}: {e}"))
}

/// Receiver, before anything is written: sort the incoming files into
/// ones we have, ones we take, and conflicts. `had` is what we know the
/// sender holds; `me_first` says whether our key sorts before the
/// sender's (our version wins a conflict). Copies are made, never
/// moves, so a transfer that never finishes leaves the folder as it was.
pub(crate) fn prepare(
    base: &Path,
    push: &FolderPush,
    had: &BTreeMap<String, String>,
    me: &str,
    sender: &str,
    me_first: bool,
) -> Result<Plan, String> {
    let mut plan = Plan::default();
    for (rel, _) in push.files.iter().chain(push.gone.iter()) {
        safe_rel(rel).ok_or_else(|| format!("unsafe path in transfer: {rel}"))?;
        if ours(rel) {
            return Err(format!("refusing nerve's own temp file {rel}"));
        }
    }
    for (rel, id) in &push.files {
        let cur = current(base, rel);
        if cur.as_deref() == Some(id) {
            plan.skip.push(rel.clone());
        } else if cur.is_none() {
            // New here: take it.
        } else if cur.as_ref() == had.get(rel) {
            // Unchanged since we last agreed: take it.
            plan.replaced.push(rel.clone());
        } else if me_first {
            let parent = Path::new(rel).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let leaf = Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let hold = if parent.is_empty() {
                format!(".{leaf}.nerve-hold-{}", super::protocol::new_id())
            } else {
                format!("{parent}/.{leaf}.nerve-hold-{}", super::protocol::new_id())
            };
            copy_aside(base, rel, &hold)?;
            plan.held.push((rel.clone(), hold, conflict_name(base, rel, sender)));
        } else {
            let to = conflict_name(base, rel, me);
            copy_aside(base, rel, &to)?;
            plan.copied.push((rel.clone(), to));
        }
    }
    for (rel, id) in &push.gone {
        if current(base, rel).as_deref() == Some(id) {
            plan.delete.push((rel.clone(), id.clone()));
        }
    }
    Ok(plan)
}

/// Receiver, once the files are written: move the incoming versions
/// that lost aside and put ours back, then make the deletions, handing
/// each file to `keep` just before it goes.
pub(crate) fn finish(base: &Path, plan: &Plan, keep: impl Fn(&str)) -> Result<(), String> {
    for (rel, hold, to) in &plan.held {
        std::fs::rename(base.join(rel), base.join(to)).map_err(|e| format!("{rel}: {e}"))?;
        std::fs::rename(base.join(hold), base.join(rel)).map_err(|e| format!("{rel}: {e}"))?;
    }
    for (rel, id) in &plan.delete {
        if current(base, rel).as_deref() != Some(id) {
            continue;
        }
        keep(rel);
        let p = base.join(rel);
        if std::fs::remove_file(&p).is_ok() {
            let mut dir = p.parent().map(Path::to_path_buf);
            while let Some(d) = dir.filter(|d| d.starts_with(base) && d != base) {
                if std::fs::remove_dir(&d).is_err() {
                    break;
                }
                dir = d.parent().map(Path::to_path_buf);
            }
        }
    }
    Ok(())
}

/// Receiver, when the transfer failed: put ours back where a write may
/// have reached it, and drop conflict copies nothing replaced.
pub(crate) fn undo(base: &Path, plan: &Plan) {
    for (rel, hold, _) in &plan.held {
        let _ = std::fs::rename(base.join(hold), base.join(rel));
    }
    for (rel, to) in &plan.copied {
        if current(base, rel).is_some() && current(base, rel) == current(base, to) {
            let _ = std::fs::remove_file(base.join(to));
        }
    }
}

/// Watch the folder; `changed` runs when something in it changes (not
/// inside `.git`, and not nerve's own temp files).
pub(crate) fn watch(base: &Path, changed: impl Fn() + Send + 'static) -> Result<RecommendedWatcher, String> {
    let root = base.to_path_buf();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if !matches!(ev.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)) {
            return;
        }
        let counts = ev.paths.iter().any(|p| {
            let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy();
            !rel.split('/').any(|c| c == ".git") && !ours(&rel)
        });
        if counts {
            changed();
        }
    })
    .map_err(|e| e.to_string())?;
    w.watch(base, RecursiveMode::Recursive).map_err(|e| e.to_string())?;
    Ok(w)
}

/// Where a folder another device shares lands here: an existing place
/// of that name, then `path_map` of the sender's path, then the same
/// path under this device's home, then `handoff_root/<name>`.
pub(crate) fn landing(
    fleet: &super::fleet::Fleet,
    name: &str,
    src_path: &str,
    home_rel: Option<&str>,
) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").ok().filter(|h| h.starts_with('/'));
    let path = if let Some(p) = fleet.places.get(name) {
        PathBuf::from(p)
    } else if let Some(p) = fleet.map_path(src_path) {
        p
    } else if let (Some(h), Some(rel)) = (&home, home_rel.and_then(safe_rel)) {
        Path::new(h).join(rel)
    } else if let Some(root) = &fleet.handoff_root {
        root.join(name)
    } else {
        return Err(format!("{} has no place for '{name}': add `{name} = \"/abs/path\"` under [places]", fleet.device));
    };
    if !path.is_absolute() || home.as_deref().is_some_and(|h| path == Path::new(h)) {
        return Err(format!("{} is not a folder nerve will sync into", path.display()));
    }
    Ok(path)
}

/// A name for a new folder: its directory's, made unique among places.
pub(crate) fn name_for(path: &Path, taken: &BTreeSet<String>) -> String {
    let base: String = path
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
                .collect()
        })
        .filter(|s: &String| !s.is_empty() && !s.starts_with('.'))
        .unwrap_or_else(|| "folder".into());
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !taken.contains(c)).unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(base: &Path, rel: &str, body: &str) {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn id(body: &str) -> String {
        super::super::chunks::id_of(body.as_bytes())
    }

    #[test]
    fn scan_hashes_files_skips_git_and_temp_files_and_reuses_unchanged_ids() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        write(b, "a.txt", "one");
        write(b, "src/b.rs", "two");
        write(b, "src/.b.rs.nerve-123", "temp");
        std::os::unix::fs::symlink(b.join("a.txt"), b.join("link")).unwrap();
        let idx = scan(b, &BTreeMap::new()).unwrap();
        assert_eq!(idx.keys().collect::<Vec<_>>(), ["a.txt", "src/b.rs"]);
        assert_eq!(idx["a.txt"].id, id("one"), "same id as a chunked file's");
        // An unchanged size and mtime trust the old id, without reading.
        let mut stale = idx.clone();
        stale.get_mut("a.txt").unwrap().id = "kept".into();
        assert_eq!(scan(b, &stale).unwrap()["a.txt"].id, "kept");
        assert_eq!(hash_file(&b.join("src/b.rs")).unwrap(), id("two"));
    }

    #[test]
    fn scan_of_a_git_repo_leaves_ignored_files_out() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git").args(args).current_dir(b).output().unwrap().status.success())
        };
        git(&["init", "-q"]);
        write(b, ".gitignore", "target/\n");
        write(b, "target/big.bin", "x");
        write(b, "new.txt", "untracked but not ignored");
        let idx = scan(b, &BTreeMap::new()).unwrap();
        assert_eq!(idx.keys().collect::<Vec<_>>(), [".gitignore", "new.txt"]);
    }

    #[test]
    fn changes_are_what_the_peer_lacks_and_batches_split_by_size() {
        let mut book = Book::default();
        for (rel, body) in [("a", "1"), ("b", "2"), ("c", "3")] {
            book.index.insert(rel.into(), Entry { size: 1, mtime_ns: 0, id: id(body) });
        }
        book.index.insert("big".into(), Entry { size: 100, mtime_ns: 0, id: id("big") });
        let peer = "pk";
        book.peers.entry(peer.into()).or_default().extend([
            ("a".to_string(), id("1")),
            ("b".to_string(), id("old")),
            ("gone".to_string(), id("g")),
        ]);
        let c = book.changes_for(peer);
        assert_eq!(c.files.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), ["b", "big", "c"]);
        assert_eq!(c.gone, [("gone".to_string(), id("g"))]);

        let first = next_batch(&c, 100);
        assert_eq!((first.files.len(), first.gone.len(), first.bytes), (2, 1, 2), "small files and deletions first");
        book.holds(peer, &first);
        let second = next_batch(&book.changes_for(peer), 100);
        assert_eq!(
            (&second.files, second.bytes),
            (&vec![("big".to_string(), id("big"))], 100),
            "then the big file alone"
        );
        book.holds(peer, &second);
        assert_eq!(book.changes_for(peer).count(), 0);
    }

    fn push(files: &[(&str, &str)], gone: &[(&str, &str)]) -> FolderPush {
        let v = |x: &[(&str, &str)]| x.iter().map(|(r, b)| (r.to_string(), id(b))).collect();
        FolderPush { files: v(files), gone: v(gone), bytes: 0 }
    }

    #[test]
    fn prepare_takes_new_and_unchanged_files_and_skips_what_matches() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        write(b, "same", "x");
        write(b, "old", "v1");
        let had = BTreeMap::from([("old".to_string(), id("v1"))]);
        let plan =
            prepare(b, &push(&[("same", "x"), ("old", "v2"), ("new", "n")], &[]), &had, "me", "them", true).unwrap();
        assert_eq!(plan.skip, ["same"]);
        assert_eq!(plan.conflicts(), 0);
    }

    #[test]
    fn conflict_where_the_incoming_version_wins_keeps_ours_beside_it() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        write(b, "f.txt", "mine");
        let had = BTreeMap::from([("f.txt".to_string(), id("base"))]);
        let plan = prepare(b, &push(&[("f.txt", "theirs")], &[]), &had, "laptop", "studio", false).unwrap();
        assert_eq!(plan.copied, [("f.txt".to_string(), "f.txt.conflict-laptop".to_string())]);
        write(b, "f.txt", "theirs"); // what the transfer writes
        finish(b, &plan, |_| {}).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("f.txt")).unwrap(), "theirs");
        assert_eq!(std::fs::read_to_string(b.join("f.txt.conflict-laptop")).unwrap(), "mine");
    }

    #[test]
    fn conflict_where_our_version_wins_puts_the_incoming_one_beside_it() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        write(b, "d/f.txt", "mine");
        let had = BTreeMap::from([("d/f.txt".to_string(), id("base"))]);
        let plan = prepare(b, &push(&[("d/f.txt", "theirs")], &[]), &had, "laptop", "studio", true).unwrap();
        assert_eq!(plan.held.len(), 1);
        assert_eq!(std::fs::read_to_string(b.join("d/f.txt")).unwrap(), "mine", "ours stays until the transfer lands");
        write(b, "d/f.txt", "theirs");
        finish(b, &plan, |_| {}).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("d/f.txt")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(b.join("d/f.txt.conflict-studio")).unwrap(), "theirs");
        assert_eq!(std::fs::read_dir(b.join("d")).unwrap().count(), 2, "no hold file left");
    }

    #[test]
    fn a_failed_transfer_is_undone() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        write(b, "a", "mine-a");
        write(b, "b", "mine-b");
        let had = BTreeMap::from([("a".to_string(), id("base")), ("b".to_string(), id("base"))]);
        let held = prepare(b, &push(&[("a", "theirs")], &[]), &had, "me", "them", true).unwrap();
        write(b, "a", "half written");
        undo(b, &held);
        assert_eq!(std::fs::read_to_string(b.join("a")).unwrap(), "mine-a");
        let copied = prepare(b, &push(&[("b", "theirs")], &[]), &had, "me", "them", false).unwrap();
        undo(b, &copied);
        assert!(!b.join("b.conflict-me").exists(), "an unneeded conflict copy goes");
        assert_eq!(std::fs::read_dir(b).unwrap().count(), 2);
    }

    #[test]
    fn deletions_only_touch_files_still_as_the_sender_knew_them() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        write(b, "dir/old.txt", "v1");
        write(b, "edited.txt", "changed here");
        let plan = prepare(
            b,
            &push(&[], &[("dir/old.txt", "v1"), ("edited.txt", "v1"), ("missing", "v1")]),
            &BTreeMap::new(),
            "me",
            "them",
            true,
        )
        .unwrap();
        finish(b, &plan, |_| {}).unwrap();
        assert!(!b.join("dir").exists(), "the file and its emptied directory go");
        assert!(b.join("edited.txt").exists(), "a local edit outlives the deletion");
    }

    #[test]
    fn unsafe_paths_are_refused_before_anything_happens() {
        let t = tempfile::tempdir().unwrap();
        assert!(prepare(t.path(), &push(&[("../x", "1")], &[]), &BTreeMap::new(), "a", "b", true).is_err());
        assert!(prepare(t.path(), &push(&[], &[("/etc/passwd", "1")]), &BTreeMap::new(), "a", "b", true).is_err());
        assert!(prepare(t.path(), &push(&[(".x.nerve-1", "1")], &[]), &BTreeMap::new(), "a", "b", true).is_err());
    }

    #[test]
    fn status_puts_errors_and_newer_files_first() {
        let mut v = View { last_sync: 100, ..Default::default() };
        assert_eq!(status(&v, 220), ("In sync, 2m ago".to_string(), false));
        v.unsent = 1;
        assert_eq!(status(&v, 220).0, "1 change to send");
        v.offline = vec!["pi".into()];
        assert_eq!(status(&v, 220).0, "Waiting for pi");
        v.behind = vec![("mini".into(), 3)];
        assert_eq!(status(&v, 220), ("3 newer files on mini".to_string(), true));
        v.error = Some("mini: no space".into());
        assert_eq!(status(&v, 220), ("Not synced: mini: no space".to_string(), true));
        v.held = vec!["mini".into()];
        assert_eq!(
            status(&v, 220),
            ("Waiting for a yes for mini".to_string(), true),
            "a folder still waiting can't have synced"
        );
    }

    #[test]
    fn rules_cover_new_folders_and_never_reach_secrets() {
        let (base, home) = (Path::new("/h/code/site"), std::env::var("HOME").unwrap());
        assert!(!Others::Ask.covers(base, Path::new("/h/code/api")));
        assert!(Others::Inside.covers(base, Path::new("/h/code/api")));
        assert!(!Others::Inside.covers(base, Path::new("/h/code")), "not the parent itself");
        assert!(!Others::Inside.covers(base, Path::new("/h/notes")));
        assert!(Others::Any.covers(base, Path::new("/h/notes")));
        assert_eq!(Others::parse("inside"), Some(Others::Inside));
        assert!(never(&Path::new(&home).join(".ssh/keys"), &[]).is_some());
        assert!(never(Path::new(&home), &[]).is_some(), "home holds secrets");
        assert!(never(Path::new("/srv/cfg/nerve"), &[PathBuf::from("/srv/cfg/nerve")]).is_some());
        assert!(never(&Path::new(&home).join("code/site"), &[]).is_none());
    }

    #[test]
    fn names_and_landing_places() {
        let taken = BTreeSet::from(["site".to_string()]);
        assert_eq!(name_for(Path::new("/x/site"), &taken), "site-2");
        assert_eq!(name_for(Path::new("/x/my app"), &taken), "my-app");
        let mut f = super::super::fleet::Fleet { device: "studio".into(), ..Default::default() };
        f.places.insert("site".into(), "/srv/site".into());
        assert_eq!(landing(&f, "site", "/Users/a/site", Some("site")).unwrap(), PathBuf::from("/srv/site"));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            landing(&f, "api", "/Users/a/code/api", Some("code/api")).unwrap(),
            Path::new(&home).join("code/api")
        );
        assert!(landing(&f, "api", "/opt/api", None).unwrap_err().contains("[places]"));
        assert!(landing(&f, "api", "/opt/api", Some("../etc")).is_err());
        assert_eq!(conflict_name(Path::new("/nonexistent"), "a/b.txt", "my mac"), "a/b.txt.conflict-my-mac");
    }
}
