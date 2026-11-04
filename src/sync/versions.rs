//! Earlier versions of files that a sync replaced or deleted, kept for
//! `keep_versions_hours` (48 by default) so a bad sync can be undone.
//!
//! Contents go into a chunk store of their own, `<sync>/versions/chunks/`:
//! cut at content-defined boundaries and named by SHA-256, so versions
//! of one file share most of their chunks. What each version is (folder,
//! path, size, when and why) is plain JSON, one line per version, in
//! `<sync>/versions/log.jsonl`. The directory is 0700. Files at or over
//! `[xfer] bulk_over` are not kept: they would double the disk they use.

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::chunks::{FileRef, Store};
use super::protocol::new_id;
use crate::util::fs::safe_rel;
use crate::util::now_secs;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Version {
    pub(crate) id: String,
    pub(crate) at: u64,
    pub(crate) folder: String,
    pub(crate) rel: String,
    /// "Replaced by studio", "Deleted by studio", "Before a restore".
    pub(crate) why: String,
    pub(crate) mode: u32,
    pub(crate) file: FileRef,
}

pub(crate) struct Versions {
    dir: PathBuf,
    store: Store,
    /// Bigger files are not kept.
    max: u64,
}

impl Versions {
    pub(crate) fn new(sync_dir: &Path, max: u64) -> Self {
        let dir = sync_dir.join("versions");
        Self { store: Store::at(dir.join("chunks")), dir, max }
    }

    fn log(&self) -> PathBuf {
        self.dir.join("log.jsonl")
    }

    /// Keep the file at `base/rel` as it is now, before a sync changes it.
    pub(crate) fn keep(&self, folder: &str, base: &Path, rel: &str, why: &str) -> std::io::Result<()> {
        let p = base.join(rel);
        let m = std::fs::metadata(&p)?;
        if m.len() >= self.max {
            return Ok(());
        }
        let mode = m.permissions().mode() & 0o777;
        let data = std::fs::read(&p)?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        let file = self.store.add_file(rel, &data)?;
        let v = Version {
            id: new_id()[..8].to_string(),
            at: now_secs(),
            folder: folder.into(),
            rel: rel.into(),
            why: why.into(),
            mode,
            file,
        };
        let mut out = std::fs::OpenOptions::new().create(true).append(true).open(self.log())?;
        writeln!(out, "{}", serde_json::to_string(&v).map_err(std::io::Error::other)?)
    }

    /// Every version kept, newest first.
    pub(crate) fn list(&self) -> Vec<Version> {
        let mut v: Vec<Version> = std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        v.reverse();
        v
    }

    /// Put `v` back into its folder at `base`. What is there now is kept
    /// first, so a restore can be undone too.
    pub(crate) fn restore(&self, v: &Version, base: &Path) -> Result<(), String> {
        let rel = safe_rel(&v.rel).ok_or_else(|| format!("unsafe path {}", v.rel))?;
        let data = self.store.assemble(&v.file).map_err(|_| format!("version {} is incomplete", v.id))?;
        let p = base.join(rel);
        if p.is_file() {
            self.keep(&v.folder, base, &v.rel, "Before a restore").map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(p.parent().unwrap_or(base)).map_err(|e| e.to_string())?;
        let tmp = p.with_file_name(format!(".{}.nerve-restore", p.file_name().unwrap_or_default().to_string_lossy()));
        std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(v.mode)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
    }

    /// Forget versions older than `keep_secs`, and the chunks only they used.
    pub(crate) fn prune(&self, keep_secs: u64) {
        let all = self.list();
        let cut = now_secs().saturating_sub(keep_secs);
        if all.iter().all(|v| v.at >= cut) {
            return;
        }
        let kept: Vec<&Version> = all.iter().rev().filter(|v| v.at >= cut).collect();
        let body: String =
            kept.iter().filter_map(|v| serde_json::to_string(v).ok()).flat_map(|l| [l, "\n".into()]).collect();
        let tmp = self.log().with_extension("tmp");
        if std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, self.log())).is_ok() {
            let used: HashSet<String> = kept.iter().flat_map(|v| v.file.chunks.iter().cloned()).collect();
            self.store.sweep(&used);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_restores_and_prunes_versions() {
        let t = tempfile::tempdir().unwrap();
        let (sync, base) = (t.path().join("sync"), t.path().join("site"));
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/a.txt"), "one").unwrap();
        std::fs::set_permissions(base.join("src/a.txt"), std::fs::Permissions::from_mode(0o640)).unwrap();
        let vs = Versions::new(&sync, 1 << 20);
        vs.keep("site", &base, "src/a.txt", "Replaced by studio").unwrap();
        std::fs::write(base.join("src/a.txt"), "two").unwrap();
        assert_eq!(std::fs::metadata(sync.join("versions")).unwrap().permissions().mode() & 0o777, 0o700);

        let v = vs.list().remove(0);
        assert_eq!(
            (v.folder.as_str(), v.rel.as_str(), v.why.as_str(), v.file.size),
            ("site", "src/a.txt", "Replaced by studio", 3)
        );
        vs.restore(&v, &base).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("src/a.txt")).unwrap(), "one");
        assert_eq!(std::fs::metadata(base.join("src/a.txt")).unwrap().permissions().mode() & 0o777, 0o640);
        // What the restore replaced is kept too, newest first.
        let all = vs.list();
        assert_eq!(all.iter().map(|v| v.why.as_str()).collect::<Vec<_>>(), ["Before a restore", "Replaced by studio"]);

        vs.prune(3600);
        assert_eq!(vs.list().len(), 2, "nothing is old yet");
        let mut log: Vec<Version> = vs.list();
        log[1].at = 1;
        std::fs::write(
            vs.log(),
            log.iter().rev().map(|v| serde_json::to_string(v).unwrap() + "\n").collect::<String>(),
        )
        .unwrap();
        vs.prune(3600);
        let left = vs.list();
        assert_eq!(left.len(), 1);
        assert!(vs.store.assemble(&left[0].file).is_ok(), "chunks still used stay");
        assert!(vs.store.assemble(&log[1].file).is_err(), "chunks only the old version used go");
    }

    #[test]
    fn a_version_never_restores_outside_its_folder() {
        let t = tempfile::tempdir().unwrap();
        let vs = Versions::new(t.path(), 1 << 20);
        std::fs::write(t.path().join("big"), "12345").unwrap();
        Versions::new(t.path(), 5).keep("f", t.path(), "big", "Replaced").unwrap();
        assert!(vs.list().is_empty(), "files at the limit are not kept");
        let v = Version { rel: "../x".into(), ..Default::default() };
        assert!(vs.restore(&v, t.path()).is_err());
    }
}
