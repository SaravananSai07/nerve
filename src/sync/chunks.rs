//! Content-addressed chunks: what makes repeat transfers small.
//!
//! A file is cut at content-defined boundaries (a gear rolling hash, as
//! in FastCDC), so an append or an edit only changes the chunks around
//! it; every other chunk keeps its SHA-256 id. Each device keeps the
//! chunks it has sent or received in `<sync>/chunks/` and remembers, per
//! peer, which ids that peer has confirmed. A transfer then lists every
//! chunk of every file but carries only the bytes the peer lacks.
//!
//! Measured on Claude transcripts (append-only JSONL): a second hand-off
//! of a 730 KiB transcript after one more turn sends the last one or two
//! chunks, not the whole file. See research/sync/experiments/RESULTS.md.

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use bitcoin_hashes::{sha256, Hash};
use serde::{Deserialize, Serialize};

/// Chunk size bounds. A chunk must fit one message (NIP-44 caps the
/// plaintext at 64 KiB and base64 adds a third), so the maximum is the
/// wire's `CHUNK_BYTES`. An 8 KiB average keeps the id overhead (64 hex
/// chars per chunk in the manifest) under 1% while isolating a
/// transcript append to the last chunk or two.
pub(crate) const MIN_CHUNK: usize = 2 * 1024;
pub(crate) const AVG_CHUNK: usize = 8 * 1024;
pub(crate) const MAX_CHUNK: usize = super::protocol::CHUNK_BYTES;

/// Cut-point masks for an 8 KiB average (FastCDC "normalised chunking",
/// level 2): a stricter mask before the average and a looser one after
/// it pull chunk sizes towards the average.
const MASK_SMALL: u64 = 0x0000_d930_0353_0000; // 15 bits set
const MASK_LARGE: u64 = 0x0000_d900_0353_0000; // 11 bits set

/// 256 pseudo-random 64-bit values, fixed at compile time (splitmix64),
/// so every device cuts identical content at identical places.
const GEAR: [u64; 256] = {
    let mut t = [0u64; 256];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut i = 0;
    while i < 256 {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        t[i] = z ^ (z >> 31);
        i += 1;
    }
    t
};

/// Lengths of the chunks `data` splits into, in order.
pub(crate) fn cut(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let n = next_cut(rest);
        out.push(n);
        rest = &rest[n..];
    }
    out
}

fn next_cut(data: &[u8]) -> usize {
    if data.len() <= MIN_CHUNK {
        return data.len();
    }
    let end = data.len().min(MAX_CHUNK);
    let normal = AVG_CHUNK.min(end);
    let mut h: u64 = 0;
    let mut i = MIN_CHUNK;
    while i < normal {
        h = (h << 1).wrapping_add(GEAR[data[i] as usize]);
        if h & MASK_SMALL == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < end {
        h = (h << 1).wrapping_add(GEAR[data[i] as usize]);
        if h & MASK_LARGE == 0 {
            return i + 1;
        }
        i += 1;
    }
    end
}

pub(crate) fn id_of(bytes: &[u8]) -> String {
    sha256::Hash::hash(bytes).to_string()
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// One file of a transfer, as an ordered list of chunk ids.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct FileRef {
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) chunks: Vec<String>,
}

/// This device's chunk store plus what it knows each peer holds.
pub(crate) struct Store {
    dir: PathBuf,
    known_dir: PathBuf,
}

/// Chunks older than this, by mtime, are pruned when the store grows
/// past `PRUNE_ABOVE`. Re-sending a pruned chunk costs one extra round
/// trip (the receiver asks for it), never correctness.
const PRUNE_ABOVE: u64 = 512 * 1024 * 1024;

impl Store {
    pub(crate) fn new(sync_dir: &Path) -> Self {
        Self { dir: sync_dir.join("chunks"), known_dir: sync_dir.join("known") }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(&id[..2]).join(id)
    }

    pub(crate) fn has(&self, id: &str) -> bool {
        valid_id(id) && self.path(id).is_file()
    }

    pub(crate) fn get(&self, id: &str) -> Option<Vec<u8>> {
        if !valid_id(id) {
            return None;
        }
        let bytes = std::fs::read(self.path(id)).ok()?;
        // A corrupt or truncated chunk is as good as missing.
        (id_of(&bytes) == id).then_some(bytes)
    }

    /// Store `bytes` under its id; returns the id. Idempotent.
    pub(crate) fn put(&self, bytes: &[u8]) -> std::io::Result<String> {
        let id = id_of(bytes);
        let p = self.path(&id);
        if !p.is_file() {
            std::fs::create_dir_all(p.parent().unwrap_or(&self.dir))?;
            let tmp = p.with_extension("tmp");
            std::fs::File::create(&tmp)?.write_all(bytes)?;
            std::fs::rename(tmp, &p)?;
        }
        Ok(id)
    }

    /// Accept a chunk from the wire: only under the id it hashes to.
    pub(crate) fn put_verified(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        if !valid_id(id) || id_of(bytes) != id {
            return Err("chunk does not match its id".into());
        }
        self.put(bytes).map(|_| ()).map_err(|e| e.to_string())
    }

    /// Cut `data` into chunks, store them, and describe the file.
    pub(crate) fn add_file(&self, name: &str, data: &[u8]) -> std::io::Result<FileRef> {
        let mut chunks = Vec::new();
        let mut at = 0;
        for n in cut(data) {
            chunks.push(self.put(&data[at..at + n])?);
            at += n;
        }
        Ok(FileRef { name: name.to_string(), size: data.len() as u64, chunks })
    }

    /// Rebuild a file from stored chunks; the ids still missing on error.
    pub(crate) fn assemble(&self, f: &FileRef) -> Result<Vec<u8>, Vec<String>> {
        let missing: Vec<String> = f.chunks.iter().filter(|c| !self.has(c)).cloned().collect();
        if !missing.is_empty() {
            return Err(dedup(missing));
        }
        let mut out = Vec::with_capacity(f.size as usize);
        let mut bad = Vec::new();
        for c in &f.chunks {
            match self.get(c) {
                Some(b) => out.extend_from_slice(&b),
                None => bad.push(c.clone()),
            }
        }
        if bad.is_empty() && out.len() as u64 == f.size {
            Ok(out)
        } else {
            Err(dedup(bad))
        }
    }

    fn known_file(&self, peer_hex: &str) -> PathBuf {
        self.known_dir.join(&peer_hex[..16.min(peer_hex.len())])
    }

    /// Chunk ids `peer` has confirmed holding.
    pub(crate) fn known(&self, peer_hex: &str) -> HashSet<String> {
        std::fs::read_to_string(self.known_file(peer_hex))
            .unwrap_or_default()
            .lines()
            .filter(|l| valid_id(l))
            .map(String::from)
            .collect()
    }

    pub(crate) fn mark_known(&self, peer_hex: &str, ids: impl IntoIterator<Item = String>) -> std::io::Result<()> {
        let mut all: BTreeSet<String> = self.known(peer_hex).into_iter().collect();
        all.extend(ids.into_iter().filter(|i| valid_id(i)));
        std::fs::create_dir_all(&self.known_dir)?;
        let body: String = all.iter().map(|i| format!("{i}\n")).collect();
        let p = self.known_file(peer_hex);
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, p)
    }

    /// A peer reported chunks it doesn't have after all (its store was
    /// pruned or wiped): stop assuming it does.
    pub(crate) fn forget_known(&self, peer_hex: &str, ids: &[String]) -> std::io::Result<()> {
        let drop: HashSet<&String> = ids.iter().collect();
        let keep: Vec<String> = self.known(peer_hex).into_iter().filter(|i| !drop.contains(i)).collect();
        std::fs::remove_file(self.known_file(peer_hex)).ok();
        self.mark_known(peer_hex, keep)
    }

    /// Drop the oldest chunks once the store passes its size cap.
    pub(crate) fn prune(&self) -> u64 {
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, m.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        if total <= PRUNE_ABOVE {
            return 0;
        }
        files.sort();
        let mut freed = 0;
        for (_, len, p) in files {
            if total <= PRUNE_ABOVE * 3 / 4 {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total -= len;
                freed += len;
            }
        }
        freed
    }
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
    v
}

/// Ids across `files` that `known` doesn't cover, each once, in order.
pub(crate) fn needed(files: &[FileRef], known: &HashSet<String>) -> Vec<String> {
    dedup(files.iter().flat_map(|f| f.chunks.iter()).filter(|c| !known.contains(*c)).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn chunks_respect_bounds_and_cover_the_input() {
        let d = data(1_000_000, 7);
        let cuts = cut(&d);
        assert_eq!(cuts.iter().sum::<usize>(), d.len());
        assert!(cuts[..cuts.len() - 1].iter().all(|&n| (MIN_CHUNK..=MAX_CHUNK).contains(&n)));
        let avg = d.len() / cuts.len();
        assert!((AVG_CHUNK / 2..AVG_CHUNK * 2).contains(&avg), "average {avg}");
        assert!(cut(b"").is_empty());
        assert_eq!(cut(b"tiny"), vec![4]);
    }

    // The property the whole module exists for.
    #[test]
    fn appending_changes_only_the_tail_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path());
        let before = data(730 * 1024, 3);
        let mut after = before.clone();
        after.extend_from_slice(&data(9_000, 11));
        let a = s.add_file("t", &before).unwrap();
        let b = s.add_file("t", &after).unwrap();
        let known: HashSet<String> = a.chunks.iter().cloned().collect();
        let new = needed(std::slice::from_ref(&b), &known);
        let bytes: usize = new.iter().map(|c| s.get(c).unwrap().len()).sum();
        // The old last chunk is re-cut together with the appended bytes;
        // everything before it is reused.
        assert!(bytes <= 9_000 + MAX_CHUNK, "{bytes} bytes in {} of {} chunks", new.len(), b.chunks.len());
        assert!(new.len() * 10 < b.chunks.len());
    }

    #[test]
    fn an_edit_in_the_middle_stays_local() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path());
        let before = data(500 * 1024, 5);
        let mut after = before.clone();
        after.splice(250_000..250_010, b"0123456789abcdef".iter().copied());
        let a = s.add_file("f", &before).unwrap();
        let b = s.add_file("f", &after).unwrap();
        let known: HashSet<String> = a.chunks.iter().cloned().collect();
        assert!(needed(std::slice::from_ref(&b), &known).len() <= 3);
    }

    #[test]
    fn store_round_trips_and_reports_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path());
        let d = data(200_000, 9);
        let f = s.add_file("x", &d).unwrap();
        assert_eq!(s.assemble(&f).unwrap(), d);

        let other = tempfile::tempdir().unwrap();
        let t = Store::new(other.path());
        assert_eq!(t.assemble(&f).unwrap_err().len(), f.chunks.len());
        for c in &f.chunks[1..] {
            t.put_verified(c, &s.get(c).unwrap()).unwrap();
        }
        assert_eq!(t.assemble(&f).unwrap_err(), vec![f.chunks[0].clone()]);
        assert!(t.put_verified(&f.chunks[0], b"forged").is_err(), "ids are checked");
        t.put_verified(&f.chunks[0], &s.get(&f.chunks[0]).unwrap()).unwrap();
        assert_eq!(t.assemble(&f).unwrap(), d);
    }

    #[test]
    fn knowledge_is_per_peer_and_can_be_retracted() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path());
        let (a, b) = (id_of(b"a"), id_of(b"b"));
        s.mark_known("peer000000000000aa", [a.clone(), b.clone()]).unwrap();
        assert_eq!(s.known("peer000000000000aa").len(), 2);
        assert!(s.known("mac0000000000000bb").is_empty());
        s.forget_known("peer000000000000aa", std::slice::from_ref(&a)).unwrap();
        assert_eq!(s.known("peer000000000000aa"), HashSet::from([b]));
        s.mark_known("peer000000000000aa", ["../../etc/passwd".to_string()]).unwrap();
        assert_eq!(s.known("peer000000000000aa").len(), 1, "junk ids are ignored");
    }
}
