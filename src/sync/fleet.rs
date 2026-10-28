//! Who this device is and who it trusts. Lives in `<config>/sync/`:
//! `device.key` (0600, hex secret) and `fleet.toml` (relays, peers,
//! capabilities). Managed by `nerve sync init|pair`, editable by hand.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use nostr_sdk::prelude::{Keys, PublicKey, ToBech32};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Fleet {
    /// This device's name as peers see it.
    pub(crate) device: String,
    /// `wss://` public relays and/or `ws://` self-hosted ones (a Pi on
    /// the tailnet). Messages go to all; the first copy wins.
    pub(crate) relays: Vec<String>,
    /// name → npub/hex of every device allowed to talk to this one.
    pub(crate) peers: BTreeMap<String, String>,
    /// Extra capabilities on top of the detected ones (os, arch, tools).
    pub(crate) caps: Vec<String>,
    /// Where handed-off sessions and job checkouts land.
    pub(crate) handoff_root: Option<PathBuf>,
    /// Source path prefix → local prefix, e.g. "/Users/me/SS" = "/home/pi/SS".
    pub(crate) path_map: BTreeMap<String, String>,
    /// When Claude's AskUserQuestion may be answered remotely:
    /// "away" (only while away mode is on), "always", or "never".
    /// The question dialog is hidden while the hook waits, so "always"
    /// takes the question away from whoever sits at this device.
    pub(crate) remote_questions: String,
    /// Permission prompts stay on screen while the hook waits, so local
    /// and remote answers race safely; on by default.
    pub(crate) remote_permissions: bool,
    /// Accept Claude's folder-trust dialog for a session this device
    /// was explicitly asked to continue.
    pub(crate) auto_trust_handoffs: bool,
    /// Seconds a hook waits for a remote answer before falling back.
    pub(crate) hook_timeout_secs: u64,
}

impl Default for Fleet {
    fn default() -> Self {
        Self {
            device: hostname(),
            relays: Vec::new(),
            peers: BTreeMap::new(),
            caps: Vec::new(),
            handoff_root: None,
            path_map: BTreeMap::new(),
            remote_questions: "away".into(),
            remote_permissions: true,
            auto_trust_handoffs: true,
            hook_timeout_secs: 600,
        }
    }
}

pub(crate) fn sync_dir(paths: &Paths) -> PathBuf {
    paths.config_dir().join("sync")
}

pub(crate) fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.split('.').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "device".into())
}

impl Fleet {
    pub(crate) fn file(dir: &Path) -> PathBuf {
        dir.join("fleet.toml")
    }

    pub(crate) fn load(dir: &Path) -> Option<Self> {
        let s = std::fs::read_to_string(Self::file(dir)).ok()?;
        match toml::from_str::<Self>(&s) {
            Ok(f) => Some(f),
            Err(e) => {
                crate::log_warn!("sync: fleet.toml parse error: {e}");
                None
            }
        }
    }

    pub(crate) fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let body = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = dir.join("fleet.toml.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, Self::file(dir))
    }

    /// Parsed peer keys; bad entries are logged and skipped.
    pub(crate) fn peer_keys(&self) -> Vec<(String, PublicKey)> {
        self.peers
            .iter()
            .filter_map(|(name, k)| match PublicKey::parse(k) {
                Ok(pk) => Some((name.clone(), pk)),
                Err(e) => {
                    crate::log_warn!("sync: peer {name}: bad key: {e}");
                    None
                }
            })
            .collect()
    }

    pub(crate) fn handoff_root(&self) -> PathBuf {
        self.handoff_root
            .clone()
            .or_else(|| dirs::home_dir().map(|h| h.join("nerve-handoff")))
            .unwrap_or_else(|| PathBuf::from("/tmp/nerve-handoff"))
    }

    /// Longest matching `path_map` prefix wins; None when nothing maps.
    pub(crate) fn map_path(&self, src: &str) -> Option<PathBuf> {
        self.path_map
            .iter()
            .filter(|(from, _)| src == from.as_str() || src.starts_with(&format!("{}/", from.trim_end_matches('/'))))
            .max_by_key(|(from, _)| from.len())
            .map(|(from, to)| PathBuf::from(format!("{}{}", to.trim_end_matches('/'), &src[from.trim_end_matches('/').len()..])))
    }
}

/// Load the device key, creating it (mode 0600) on first use.
pub(crate) fn device_keys(dir: &Path) -> std::io::Result<Keys> {
    let path = dir.join("device.key");
    if let Ok(s) = std::fs::read_to_string(&path) {
        return Keys::parse(s.trim()).map_err(std::io::Error::other);
    }
    std::fs::create_dir_all(dir)?;
    let keys = Keys::generate();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(keys.secret_key().to_secret_hex().as_bytes())?;
    Ok(keys)
}

pub(crate) fn npub(pk: &PublicKey) -> String {
    pk.to_bech32().unwrap_or_else(|_| pk.to_hex())
}

/// Capabilities a job can require: OS, arch, and tools found on PATH.
/// Spawns processes, so only call it from a worker or CLI thread.
pub(crate) fn detect_caps(extra: &[String]) -> Vec<String> {
    let mut caps = vec![std::env::consts::OS.to_string(), std::env::consts::ARCH.to_string()];
    let ok = |argv: &[&str]| {
        std::process::Command::new(argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    for (cap, argv) in [
        ("tmux", &["tmux", "-V"][..]),
        ("docker", &["docker", "--version"][..]),
        ("git", &["git", "--version"][..]),
        ("node", &["node", "--version"][..]),
        ("cargo", &["cargo", "--version"][..]),
    ] {
        if ok(argv) {
            caps.push(cap.to_string());
        }
    }
    #[cfg(target_os = "macos")]
    {
        if ok(&["xcrun", "--find", "xcodebuild"]) {
            caps.push("xcode".into());
            if ok(&["xcrun", "simctl", "help"]) {
                caps.push("ios-sim".into());
            }
        }
    }
    for c in extra {
        if !caps.contains(c) {
            caps.push(c.clone());
        }
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_map_prefers_longest_prefix_and_respects_boundaries() {
        let mut f = Fleet::default();
        f.path_map.insert("/Users/me".into(), "/home/pi".into());
        f.path_map.insert("/Users/me/SS".into(), "/srv/ss".into());
        assert_eq!(f.map_path("/Users/me/SS/nerve"), Some(PathBuf::from("/srv/ss/nerve")));
        assert_eq!(f.map_path("/Users/me/x"), Some(PathBuf::from("/home/pi/x")));
        assert_eq!(f.map_path("/Users/me"), Some(PathBuf::from("/home/pi")));
        assert_eq!(f.map_path("/Users/meow/x"), None, "prefix must end at a path boundary");
    }

    #[test]
    fn device_key_is_created_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let a = device_keys(dir.path()).unwrap();
        let b = device_keys(dir.path()).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        let mode = std::fs::metadata(dir.path().join("device.key")).unwrap().permissions();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o600);
    }

    #[test]
    fn fleet_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = Fleet { device: "mac".into(), ..Default::default() };
        f.peers.insert("pi".into(), "npub1x".into());
        f.relays.push("ws://100.73.26.46:7777".into());
        f.save(dir.path()).unwrap();
        let g = Fleet::load(dir.path()).unwrap();
        assert_eq!(g.device, "mac");
        assert_eq!(g.peers["pi"], "npub1x");
        assert_eq!(g.remote_questions, "away");
    }
}
