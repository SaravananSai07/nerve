//! Pairing by invite code, so a new device joins with one command.
//!
//! `nerve setup` on a device that is already set up prints a code:
//! `nerve1` + base64url(JSON {v, n: name, k: pubkey hex, s: secret,
//! r: relays}). `nerve setup <code>` on the new device trusts the
//! inviter's key, adopts its relays, and sends a `PairRequest` carrying
//! the secret. The inviter's agent accepts that one message from an
//! unknown key, checks the secret (single use, valid for an hour) and
//! pairs back. Both ends then trust each other's key, exactly as two
//! manual `nerve sync pair` calls would leave them.
//!
//! The code is a credential for the next hour: anyone holding it can
//! pair a device that may then type into your sessions. It is shown
//! once, on the inviting device's own terminal.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::protocol::{base64_decode, base64_encode, new_id, now_secs};

pub(crate) const INVITE_TTL_SECS: u64 = 3600;
const PREFIX: &str = "nerve1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Invite {
    pub(crate) v: u32,
    /// Inviter's device name.
    pub(crate) n: String,
    /// Inviter's public key, hex.
    pub(crate) k: String,
    /// One-time secret.
    pub(crate) s: String,
    /// Inviter's relays: the joiner uses the same ones.
    pub(crate) r: Vec<String>,
}

impl Invite {
    pub(crate) fn encode(&self) -> String {
        let json = serde_json::to_string(self).expect("invite serialises");
        let b64 = base64_encode(json.as_bytes()).replace('+', "-").replace('/', "_");
        format!("{PREFIX}{}", b64.trim_end_matches('='))
    }

    pub(crate) fn decode(code: &str) -> Result<Self, String> {
        let body = code.trim().strip_prefix(PREFIX).ok_or("not a nerve invite code (it starts with nerve1)")?;
        let mut b64 = body.replace('-', "+").replace('_', "/");
        while b64.len() % 4 != 0 {
            b64.push('=');
        }
        let raw = base64_decode(&b64).map_err(|_| "invite code is damaged (copy it again)")?;
        let inv: Self = serde_json::from_slice(&raw).map_err(|_| "invite code is damaged (copy it again)")?;
        if inv.v != 1 {
            return Err(format!("invite is for a newer nerve (v{}); update this device", inv.v));
        }
        if inv.k.len() != 64 || !inv.k.bytes().all(|b| b.is_ascii_hexdigit()) || !valid_secret(&inv.s) {
            return Err("invite code is damaged (copy it again)".into());
        }
        if inv.r.iter().any(|r| !(r.starts_with("ws://") || r.starts_with("wss://"))) {
            return Err("invite lists a relay that isn't ws:// or wss://".into());
        }
        Ok(inv)
    }
}

fn valid_secret(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn dir(sync_dir: &Path) -> PathBuf {
    sync_dir.join("invites")
}

/// A fresh single-use invite for this device.
pub(crate) fn create(sync_dir: &Path, name: &str, pubkey_hex: &str, relays: &[String]) -> std::io::Result<Invite> {
    let d = dir(sync_dir);
    std::fs::create_dir_all(&d)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    }
    let secret = format!("{}{}", new_id(), new_id());
    std::fs::write(d.join(&secret), (now_secs() + INVITE_TTL_SECS).to_string())?;
    Ok(Invite { v: 1, n: name.to_string(), k: pubkey_hex.to_string(), s: secret, r: relays.to_vec() })
}

/// Use up an invite. True only once, and only before it expires.
pub(crate) fn consume(sync_dir: &Path, secret: &str) -> bool {
    if !valid_secret(secret) {
        return false;
    }
    let p = dir(sync_dir).join(secret);
    let expires = std::fs::read_to_string(&p).ok().and_then(|s| s.trim().parse::<u64>().ok());
    // Remove first: two racing requests can't both win.
    std::fs::remove_file(&p).is_ok() && expires.is_some_and(|t| now_secs() <= t)
}

/// Any unexpired invite outstanding? The agent only listens to unknown
/// keys while one is. Expired ones are deleted on the way.
pub(crate) fn any_open(sync_dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir(sync_dir)) else { return false };
    let now = now_secs();
    let mut open = false;
    for e in rd.flatten() {
        let alive = std::fs::read_to_string(e.path()).ok().and_then(|s| s.trim().parse::<u64>().ok()).is_some_and(|t| now <= t);
        if alive {
            open = true;
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
    open
}

/// A name for the joining device that doesn't clash with existing peers.
pub(crate) fn unique_name(wanted: &str, taken: impl Fn(&str) -> bool) -> String {
    let base: String = wanted
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(32)
        .collect();
    let base = if base.is_empty() { "device".to_string() } else { base };
    if !taken(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| !taken(n)).unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_round_trips_and_rejects_damage() {
        let inv = Invite { v: 1, n: "mac".into(), k: "ab".repeat(32), s: "0f".repeat(16), r: vec!["wss://relay.primal.net".into()] };
        let code = inv.encode();
        assert!(code.starts_with("nerve1") && !code.contains('=') && !code.contains('/'));
        assert_eq!(Invite::decode(&format!("  {code}\n")).unwrap(), inv);
        assert!(Invite::decode("npub1abc").is_err());
        assert!(Invite::decode(&code[..code.len() - 5]).is_err());
        let bad = Invite { r: vec!["http://evil".into()], ..inv };
        assert!(Invite::decode(&bad.encode()).is_err());
    }

    #[test]
    fn invites_are_single_use_and_expire() {
        let d = tempfile::tempdir().unwrap();
        let inv = create(d.path(), "mac", &"ab".repeat(32), &[]).unwrap();
        assert!(any_open(d.path()));
        assert!(!consume(d.path(), &"0".repeat(32)), "unknown secret");
        assert!(consume(d.path(), &inv.s));
        assert!(!consume(d.path(), &inv.s), "second use");
        assert!(!any_open(d.path()));

        let old = create(d.path(), "mac", &"ab".repeat(32), &[]).unwrap();
        std::fs::write(d.path().join("invites").join(&old.s), "1").unwrap();
        assert!(!any_open(d.path()), "expired invites don't count and are removed");
        assert!(!consume(d.path(), &old.s));
        assert!(!consume(d.path(), "../fleet.toml"));
    }

    #[test]
    fn joining_names_never_clash() {
        let taken = |n: &str| n == "pixel" || n == "pixel-2";
        assert_eq!(unique_name("pixel", taken), "pixel-3");
        assert_eq!(unique_name("my phone!", |_| false), "myphone");
        assert_eq!(unique_name("", |_| false), "device");
    }
}
