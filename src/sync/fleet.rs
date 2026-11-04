//! Who this device is and who it trusts. Lives in `<config>/sync/`:
//! `device.key` (0600, hex secret) and `fleet.toml` (relays, peers,
//! capabilities). Managed by `nerve connect` and the TUI, editable by hand.

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
    /// Peers that devices joining later are not connected to on their
    /// own (they joined for "the devices there now").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) closed: Vec<String>,
    /// Extra capabilities on top of the detected ones (os, arch, tools).
    pub(crate) caps: Vec<String>,
    /// Folders whose sessions stay on this device, subfolders included
    /// (`nerve sync private`). Peers don't see them, can't type into them,
    /// and their prompts are answered only here. Every other session is
    /// visible to paired devices.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) private: Vec<String>,
    /// Single sessions kept on this device, by session id.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) private_sessions: Vec<String>,
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
    /// Folders kept the same on several devices; each one's path is the
    /// place of the same name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) folders: Vec<FolderCfg>,
    /// Hours a file a sync replaced or deleted is kept, to undo it.
    pub(crate) keep_versions_hours: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct FolderCfg {
    pub(crate) name: String,
    /// The other devices that keep a copy.
    pub(crate) with: Vec<String>,
    #[serde(default)]
    pub(crate) when: super::folders::When,
    /// What those devices may sync next without asking.
    #[serde(default)]
    pub(crate) others: super::folders::Others,
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
            device: crate::util::hostname(),
            relays: Vec::new(),
            peers: BTreeMap::new(),
            closed: Vec::new(),
            caps: Vec::new(),
            private: Vec::new(),
            private_sessions: Vec::new(),
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
            folders: Vec::new(),
            keep_versions_hours: 48,
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

/// This device's fleet.toml, or what to do when there is none.
pub(crate) fn set_up(dir: &Path) -> Result<Fleet, String> {
    Fleet::load(dir).ok_or_else(|| "not set up; run `nerve connect` first".into())
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
        create_private_dir(dir)?;
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

    /// Is the session `id` running in `cwd` kept on this device?
    pub(crate) fn is_private(&self, id: &str, cwd: &Path) -> bool {
        self.private_sessions.iter().any(|s| s == id) || inside(&self.private, cwd)
    }

    /// Keep a folder (absolute; subfolders included) or one session on
    /// this device, or make it visible again. Making a folder visible
    /// drops any private folder inside it, and fails while a parent stays
    /// private.
    pub(crate) fn set_private(&mut self, target: &PrivateTarget, on: bool) -> Result<String, String> {
        let path = match target {
            PrivateTarget::Session(id) => {
                let had = self.private_sessions.iter().any(|s| s == id);
                return Ok(match (on, had) {
                    (true, true) => format!("session {id:.8} is already private"),
                    (true, false) => {
                        self.private_sessions.push(id.clone());
                        format!("session {id:.8} stays on this device now")
                    }
                    (false, true) => {
                        self.private_sessions.retain(|s| s != id);
                        format!("session {id:.8} is visible to your devices again")
                    }
                    (false, false) => format!("session {id:.8} was not private"),
                });
            }
            PrivateTarget::Folder(path) => path,
        };
        if !path.is_absolute() {
            return Err(format!("{} is not an absolute path", path.display()));
        }
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let p = path.to_string_lossy().into_owned();
        if on {
            if inside(&self.private, &path) {
                return Ok(format!("{p} is already private"));
            }
            self.private.push(p.clone());
            return Ok(format!("sessions in {p} stay on this device now"));
        }
        let before = self.private.len();
        self.private.retain(|s| !inside(std::slice::from_ref(&p), Path::new(s)));
        if let Some(parent) = self.private.iter().find(|s| inside(std::slice::from_ref(s), &path)) {
            return Err(format!("{p} is inside private {parent}; make that visible instead"));
        }
        Ok(if self.private.len() < before {
            format!("sessions in {p} are visible to your devices again")
        } else {
            format!("{p} was not private")
        })
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
            .map(|(from, to)| {
                PathBuf::from(format!("{}{}", to.trim_end_matches('/'), &src[from.trim_end_matches('/').len()..]))
            })
    }
}

/// What `nerve sync private` and the TUI's `p` keep on this device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum PrivateTarget {
    Folder(PathBuf),
    Session(String),
}

/// Is `cwd` inside one of the folders in `list`? A leading `~/` means $HOME.
pub(crate) fn inside(list: &[String], cwd: &Path) -> bool {
    let cwd = cwd.to_string_lossy();
    list.iter().any(|s| {
        let root = crate::paths::untilde(s).to_string_lossy().into_owned();
        if !root.starts_with('/') {
            return false;
        }
        let root = root.trim_end_matches('/');
        root.is_empty() || cwd == root || cwd.starts_with(&format!("{root}/"))
    })
}

/// The sync dir holds the device key and the mod's socket: owner only.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// `ws://` or `wss://`: the only relay URLs nerve speaks.
pub(crate) fn check_relay(url: &str) -> Result<(), String> {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        Ok(())
    } else {
        Err(format!("relay must be ws:// or wss://: {url}"))
    }
}

/// Load the device key, creating it (mode 0600) on first use.
pub(crate) fn device_keys(dir: &Path) -> std::io::Result<Keys> {
    let path = dir.join("device.key");
    if let Ok(s) = std::fs::read_to_string(&path) {
        return Keys::parse(s.trim()).map_err(std::io::Error::other);
    }
    create_private_dir(dir)?;
    let keys = Keys::generate();
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    f.write_all(keys.secret_key().to_secret_hex().as_bytes())?;
    Ok(keys)
}

pub(crate) fn npub(pk: &PublicKey) -> String {
    pk.to_bech32().unwrap_or_else(|_| pk.to_hex())
}

/// What this device advertises (`nerve sync status`): OS, arch, and tools on PATH.
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
    fn sessions_are_visible_unless_kept_private() {
        let mut f = Fleet::default();
        assert!(!f.is_private("s1", Path::new("/srv/proj")), "everything is visible by default");
        f.private = vec!["/srv/proj/".into(), "relative/dir".into()];
        assert!(f.is_private("s1", Path::new("/srv/proj")));
        assert!(f.is_private("s1", Path::new("/srv/proj/sub/dir")));
        assert!(!f.is_private("s1", Path::new("/srv/project")), "prefix must end at path boundary");
        assert!(!f.is_private("s1", Path::new("/srv")));
        assert!(!f.is_private("s1", Path::new("relative/dir")), "relative entries match nothing");
        let home = std::env::var("HOME").unwrap();
        f.private = vec!["~/code".into()];
        assert!(f.is_private("s1", &Path::new(&home).join("code/x")));
        f.private_sessions = vec!["s2".into()];
        assert!(f.is_private("s2", Path::new("/anywhere")), "one session alone");
    }

    #[test]
    fn keeping_folders_and_sessions_private() {
        let mut f = Fleet::default();
        let dir = |p: &str| PrivateTarget::Folder(PathBuf::from(p));
        assert!(f.set_private(&dir("rel"), true).is_err());
        f.set_private(&dir("/nonexistent/a"), true).unwrap();
        f.set_private(&dir("/nonexistent/a/b"), true).unwrap();
        assert_eq!(f.private, ["/nonexistent/a"], "already covered by its parent");
        assert!(f.set_private(&dir("/nonexistent/a/b"), false).is_err(), "parent keeps it private");
        f.private.push("/nonexistent/a/c".into());
        f.set_private(&dir("/nonexistent/a"), false).unwrap();
        assert!(f.private.is_empty(), "making a folder visible drops folders inside it");
        let one = PrivateTarget::Session("abc".into());
        f.set_private(&one, true).unwrap();
        f.set_private(&one, true).unwrap();
        assert_eq!(f.private_sessions, ["abc"]);
        f.set_private(&one, false).unwrap();
        assert!(f.private_sessions.is_empty());
    }

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
        assert!(
            !body.contains("[places]") && !body.contains("[run]") && !body.contains("folders"),
            "empty tables stay out of the file"
        );
        let mut g = g;
        g.places.insert("site".into(), "/srv/site".into());
        g.folders.push(FolderCfg {
            name: "site".into(),
            with: vec!["server".into()],
            when: super::super::folders::When::Watch,
            others: super::super::folders::Others::Inside,
        });
        g.closed.push("pi".into());
        g.save(dir.path()).unwrap();
        let h = Fleet::load(dir.path()).unwrap();
        assert_eq!((h.folders, h.closed, h.keep_versions_hours), (g.folders, vec!["pi".to_string()], 48));
        assert!(std::fs::read_to_string(Fleet::file(dir.path())).unwrap().contains("others = \"inside\""));
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
