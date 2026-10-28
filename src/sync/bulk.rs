//! Bulk files over rsync. The relay holds every chunk in memory until
//! it is deleted or expires, so files at or over `[xfer] bulk_over`
//! (and any transfer the relay fails to carry) go straight between the
//! devices over ssh, run by whichever side has a route (`[ssh]`). The
//! relay, when it is up, carries only the offer, the other side's path
//! and the outcome.
//!
//! The rules of a place hold here too: roots are relative and stay
//! inside it, symlinks are neither followed nor sent (rsync without
//! `-l` skips them), nothing is written through one on the receiver,
//! and `--delete` reaches only into the directories that were named.
//! Each root is its own rsync, so neither the path nor `--delete`
//! depends on `--relative`, which macOS's openrsync implements without
//! the `/./` anchor.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::handoff::walk;
use super::places;
use super::protocol::BulkOffer;

const ATTEMPTS: u32 = 3;
const SSH: &str = "ssh -o BatchMode=yes -o ConnectTimeout=15";
/// How often a running rsync says it is still going, so a follower
/// can tell a long copy from a stalled one.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(60);

/// Sender: what the roots hold. A symlinked root is refused, as the
/// relay path refuses it.
pub(crate) fn scan(base: &Path, roots: &[String]) -> Result<BulkOffer, String> {
    let mut picked: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs = Vec::new();
    for root in roots {
        let rel = places::norm_root(root)?;
        let p = if rel.is_empty() { base.to_path_buf() } else { base.join(&rel) };
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
    picked.sort();
    picked.dedup_by(|a, b| a.0 == b.0);
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

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Whether this device's rsync needs remote paths quoted for the
/// remote shell: GNU rsync from 3.2.4 escapes them itself.
fn rsync_quotes() -> bool {
    static Q: OnceLock<bool> = OnceLock::new();
    *Q.get_or_init(|| {
        let v = Command::new("rsync").arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
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
pub(crate) fn run(src: &End, dst: &End, roots: &[String], offer: &BulkOffer, delete: bool, beat: &dyn Fn(String)) -> Result<String, String> {
    let started = Instant::now();
    let quote = rsync_quotes();
    for root in roots {
        let rel = places::norm_root(root)?;
        if let End::Local(base) = dst {
            places::ready_for(base, &rel)?;
        }
        let is_dir = offer.dirs.contains(&rel);
        let a = argv(src, dst, &rel, is_dir, delete, quote);
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            match run_one(&a, &|secs| beat(format!("rsync {} attempt {attempt}: {secs}s", if rel.is_empty() { "." } else { &rel }))) {
                Ok(()) => {
                    last.clear();
                    break;
                }
                Err(e) => {
                    last = e;
                    if attempt < ATTEMPTS {
                        beat(format!("rsync {} attempt {attempt} failed ({last}); retrying", if rel.is_empty() { "." } else { &rel }));
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
        human(offer.bytes),
        started.elapsed().as_secs_f64()
    ))
}

fn run_one(argv: &[String], beat: &dyn Fn(u64)) -> Result<(), String> {
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
            Err(e) => return Err(e.to_string()),
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
    let mut cmd = format!(
        "PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH\" exec nerve sync place {} {}",
        shell_quote(place),
        if as_src { "--src" } else { "--dst" }
    );
    for r in roots {
        cmd.push(' ');
        cmd.push_str(&shell_quote(r));
    }
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", host, &cmd])
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
            places::ready_for(base, &places::norm_root(r)?)?;
        }
        None
    };
    Ok(PlaceInfo { path: base.display().to_string(), scan })
}

pub(crate) fn human(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.1} KiB", b as f64 / 1024.0),
        _ => format!("{:.1} MiB", b as f64 / 1_048_576.0),
    }
}

/// How a files transfer travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Relay,
    /// This device runs rsync.
    RsyncHere,
    /// The recipient runs rsync, pulling from us.
    RsyncThere,
}

pub(crate) fn route(bytes: u64, bulk_over: u64, have_ssh: bool) -> Route {
    match (bytes >= bulk_over, have_ssh) {
        (false, _) => Route::Relay,
        (true, true) => Route::RsyncHere,
        (true, false) => Route::RsyncThere,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_picks_the_route() {
        let mib = 1 << 20;
        assert_eq!(route(63 * mib, 64 * mib, true), Route::Relay);
        assert_eq!(route(64 * mib, 64 * mib, true), Route::RsyncHere, "at the threshold counts as over");
        assert_eq!(route(1 << 30, 64 * mib, false), Route::RsyncThere);
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
        let msg = run(&src, &dst, &["a".into()], &offer, true, &|_| {}).unwrap();
        assert!(msg.starts_with("via rsync, 1 file(s)"), "{msg}");
        assert_eq!(std::fs::read(d.path().join("a/b/f")).unwrap(), b"x");
        assert!(std::fs::symlink_metadata(d.path().join("a/link")).is_err(), "symlink not sent");
        assert!(!d.path().join("a/stale").exists(), "--delete inside the named dir");
        assert!(d.path().join("keep").exists(), "--delete stays inside it");
        // A symlinked directory on the receiver is never written through.
        std::fs::remove_dir_all(d.path().join("a")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), d.path().join("a")).unwrap();
        assert!(run(&src, &dst, &["a".into()], &offer, false, &|_| {}).unwrap_err().contains("symlink"));
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }
}
