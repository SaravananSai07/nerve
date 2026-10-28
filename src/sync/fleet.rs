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
    /// `wss://` public relays and/or `ws://` self-hosted ones (a small always-on box on
    /// a private network). Messages go to all; the first copy wins.
    pub(crate) relays: Vec<String>,
    /// name → npub/hex of every device allowed to talk to this one.
    pub(crate) peers: BTreeMap<String, String>,
    /// Extra capabilities on top of the detected ones (os, arch, tools).
    pub(crate) caps: Vec<String>,
    /// Where a hand-off of a repo that neither `--to <place>` nor
    /// `path_map` places lands. Unset, such a hand-off is refused.
    pub(crate) handoff_root: Option<PathBuf>,
    /// Name → absolute directory on *this* device. Peers address places
    /// by name, so the same name can map to a different path per device.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) places: BTreeMap<String, String>,
    /// Commands peers may run here (`nerve sync run`).
    #[serde(skip_serializing_if = "RunConfig::is_empty")]
    pub(crate) run: RunConfig,
    /// Source path prefix → local prefix, e.g. "/Users/me/code" = "/home/user/code".
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
    /// Peer name → the ssh host this device reaches it at. Files at or
    /// over `[xfer] bulk_over`, and any the relay fails to carry, go over
    /// rsync on it; one side of a pair having a route is enough.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) ssh: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "XferConfig::is_default")]
    pub(crate) xfer: XferConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct XferConfig {
    /// Bytes from which files skip the relay. The relay holds every
    /// chunk in memory until it is deleted or expires, so it suits many
    /// small transfers, not one big one.
    pub(crate) bulk_over: u64,
}

impl Default for XferConfig {
    fn default() -> Self {
        Self { bulk_over: 64 << 20 }
    }
}

impl XferConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for Fleet {
    fn default() -> Self {
        Self {
            device: hostname(),
            relays: Vec::new(),
            peers: BTreeMap::new(),
            caps: Vec::new(),
            handoff_root: None,
            places: BTreeMap::new(),
            run: RunConfig::default(),
            path_map: BTreeMap::new(),
            remote_questions: "away".into(),
            remote_permissions: true,
            auto_trust_handoffs: true,
            hook_timeout_secs: 600,
            ssh: BTreeMap::new(),
            xfer: XferConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct RunConfig {
    pub(crate) allow: Vec<RunRule>,
}

impl RunConfig {
    fn is_empty(&self) -> bool {
        self.allow.is_empty()
    }
}

/// One allowed command. `argv` matches literally, except that a final
/// `"*"` matches any further arguments (none included).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct RunRule {
    /// Only inside this place; unset matches any place or directory.
    pub(crate) place: Option<String>,
    /// Only inside this absolute directory.
    pub(crate) cwd: Option<String>,
    pub(crate) argv: Vec<String>,
    /// Join the argv with spaces and run it with `sh -c`. A trailing `*`
    /// then admits arbitrary shell, so keep shell rules literal.
    pub(crate) shell: bool,
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

    /// The local directory of place `name`, or what to add to `file`
    /// (this device's fleet.toml) to define it.
    pub(crate) fn place(&self, name: &str, file: &Path) -> Result<PathBuf, String> {
        let add = || format!("add `{name} = \"/abs/path\"` under [places] in {}", file.display());
        let Some(p) = self.places.get(name) else {
            return Err(format!("no place '{name}' on {}: {}", self.device, add()));
        };
        let p = PathBuf::from(p);
        if !p.is_absolute() {
            return Err(format!("place '{name}' on {} is not an absolute path: {}", self.device, add()));
        }
        Ok(p)
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
        f.path_map.insert("/Users/me".into(), "/home/user".into());
        f.path_map.insert("/Users/me/code".into(), "/srv/ss".into());
        assert_eq!(f.map_path("/Users/me/code/nerve"), Some(PathBuf::from("/srv/ss/nerve")));
        assert_eq!(f.map_path("/Users/me/x"), Some(PathBuf::from("/home/user/x")));
        assert_eq!(f.map_path("/Users/me"), Some(PathBuf::from("/home/user")));
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
        let mut f = Fleet { device: "laptop".into(), ..Default::default() };
        f.peers.insert("server".into(), "npub1x".into());
        f.relays.push("ws://192.0.2.10:7777".into());
        f.save(dir.path()).unwrap();
        let g = Fleet::load(dir.path()).unwrap();
        assert_eq!(g.device, "laptop");
        assert_eq!(g.peers["server"], "npub1x");
        assert_eq!(g.remote_questions, "away");
        let body = std::fs::read_to_string(Fleet::file(dir.path())).unwrap();
        assert!(!body.contains("[places]") && !body.contains("[run]"), "empty tables stay out of the file");
    }

    #[test]
    fn places_and_run_rules_parse_from_toml() {
        let f: Fleet = toml::from_str(
            r#"
device = "server"
[places]
proj = "/srv/proj"
rel = "not/absolute"
[run]
allow = [{ place = "proj", argv = ["node", "bin/check.mjs", "*"] }, { cwd = "/srv", argv = ["make"], shell = true }]
"#,
        )
        .unwrap();
        let file = Path::new("/c/fleet.toml");
        assert_eq!(f.place("proj", file).unwrap(), PathBuf::from("/srv/proj"));
        assert!(f.place("rel", file).unwrap_err().contains("absolute"));
        let e = f.place("nope", file).unwrap_err();
        assert!(e.contains("[places]") && e.contains("/c/fleet.toml"), "{e}");
        assert_eq!(f.run.allow.len(), 2);
        assert_eq!(f.run.allow[0].argv, ["node", "bin/check.mjs", "*"]);
        assert!(f.run.allow[1].shell && f.run.allow[1].place.is_none());
    }
}
