//! `nerve connect [code]` (also `nerve setup`): everything a device
//! needs, in one command.
//!
//! 1. Create this device's identity (once) with working default relays.
//! 2. Connect Claude, and keep the agent running (see `install`).
//! 3. With a code: ask the device that showed it to let this one in
//!    (see `Invite`). Without one: show a code, and when a device uses
//!    it, ask whether to let it in.
//!
//! Re-running it is safe: every step checks what is already in place.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::{ClientNotification, PublicKey, StreamExt};
use serde::{Deserialize, Serialize};

use super::fleet::{self, Fleet};
use super::protocol::{self, base64_decode, base64_encode, Body, Envelope};
use super::transport::Transport;
use crate::paths::Paths;
use crate::util::sanitize::Sanitised;

/// Relays that delivered for fresh keys in testing (E14).
const DEFAULT_RELAYS: [&str; 2] = ["wss://relay.primal.net", "wss://relay.snort.social"];
/// How `nerve connect` (or `nerve setup`) was asked to run.
pub(crate) struct Opts {
    pub(crate) code: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) relays: Vec<String>,
    /// Showing a code: who the new device joins (`all`, `now`, or names).
    /// Entering one: which of the offered devices to join.
    pub(crate) with: Option<String>,
    pub(crate) service: bool,
    pub(crate) claude: bool,
    pub(crate) wait: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self { code: None, name: None, relays: Vec::new(), with: None, service: true, claude: true, wait: true }
    }
}

fn parse(args: &[&str]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--name" => o.name = Some(it.next().ok_or("--name needs a value")?.to_string()),
            "--relay" => o.relays.push(it.next().ok_or("--relay needs a URL")?.to_string()),
            "--with" => o.with = Some(it.next().ok_or("--with needs all, now or device names")?.to_string()),
            "--no-service" => o.service = false,
            "--no-claude" => o.claude = false,
            "--no-wait" => o.wait = false,
            c if c.starts_with("nerve1") => o.code = Some(c.to_string()),
            other => return Err(format!("unknown argument {other}\nusage: nerve connect [<code>] [--with all|now|<devices>] [--name N] [--relay URL]... [--no-service] [--no-claude] [--no-wait]")),
        }
    }
    Ok(o)
}

/// A progress line: done (✓) or a warning (!).
type Say<'a> = &'a dyn Fn(bool, &str);

fn print(ok: bool, msg: &str) {
    println!("  {} {msg}", if ok { "\u{2713}" } else { "!" });
}

pub(crate) fn names(list: &str) -> Vec<String> {
    list.split(',').map(str::trim).filter(|d| !d.is_empty()).map(String::from).collect()
}

pub(crate) fn run(paths: &Paths, args: &[&str]) -> Result<String, String> {
    let o = parse(args)?;
    let invite = o.code.as_deref().map(Invite::decode).transpose()?;
    println!("nerve connect");
    let (fleet, keys) = prepare(paths, &o, invite.as_ref(), &print)?;
    let dir = fleet::sync_dir(paths);
    match &invite {
        Some(inv) => {
            let want = o.with.as_deref().map(names);
            let words = words(&inv.k, &keys.public_key().to_hex(), &inv.s);
            print(true, &format!("asked {} to let this device in. Check that it shows: {words}", inv.n));
            let joined = join(&dir, &keys, inv, want.as_deref())?;
            Ok(format!("\nConnected to {}. Run `nerve` to see their sessions.", crate::util::text::list_join(&joined)))
        }
        None => show_code(&dir, &fleet, &keys, &o),
    }
}

/// This device's identity, the Claude mod and the background agent: all
/// a device needs before it connects. Safe to repeat.
pub(crate) fn prepare(
    paths: &Paths,
    o: &Opts,
    invite: Option<&Invite>,
    say: Say,
) -> Result<(Fleet, nostr_sdk::prelude::Keys), String> {
    let dir = fleet::sync_dir(paths);
    let fresh = Fleet::load(&dir).is_none();
    let mut fleet = Fleet::load(&dir).unwrap_or_default();
    if let Some(n) = &o.name {
        fleet.device = n.clone();
    }
    let mut relays: Vec<String> = o.relays.clone();
    relays.extend(invite.iter().flat_map(|i| i.r.iter().cloned()));
    if fleet.relays.is_empty() && relays.is_empty() {
        relays.extend(DEFAULT_RELAYS.iter().map(|s| s.to_string()));
    }
    for r in relays {
        fleet::check_relay(&r)?;
        if !fleet.relays.contains(&r) {
            fleet.relays.push(r);
        }
    }
    let keys = fleet::device_keys(&dir).map_err(|e| e.to_string())?;
    if invite.is_some_and(|i| i.k == keys.public_key().to_hex()) {
        return Err("that code came from this device; enter it on the other one".into());
    }
    fleet.save(&dir).map_err(|e| e.to_string())?;
    say(
        true,
        &format!(
            "{} '{}', relays: {}",
            if fresh { "created device" } else { "device" },
            fleet.device,
            fleet.relays.join(" ")
        ),
    );
    if o.claude {
        super::install::claude(&dir, say);
    }
    if o.service {
        match super::install::service(&dir) {
            Ok(how) => say(true, &how),
            Err(e) => say(false, &format!("background agent not installed: {e}. Run `nerve sync agent` yourself.")),
        }
    }
    Ok((fleet, keys))
}

/// Who a new device joins: `all` (also devices added later, the
/// default), `now` (only the devices there now), or named devices.
pub(crate) fn offer(fleet: &Fleet, with: Option<&str>) -> Result<(Vec<String>, bool), String> {
    let open = || fleet.peers.keys().filter(|d| !fleet.closed.contains(d)).cloned().collect();
    match with.unwrap_or("all") {
        "all" => Ok((open(), true)),
        "now" => Ok((open(), false)),
        list => {
            let picked = names(list);
            match picked.iter().find(|d| !fleet.peers.contains_key(*d)) {
                Some(d) => Err(format!("unknown device {d}")),
                None => Ok((picked, false)),
            }
        }
    }
}

fn show_code(dir: &Path, fleet: &Fleet, keys: &nostr_sdk::prelude::Keys, o: &Opts) -> Result<String, String> {
    let (offer, later) = offer(fleet, o.with.as_deref())?;
    let inv = create(dir, &fleet.device, &keys.public_key().to_hex(), &fleet.relays, offer.clone(), later)
        .map_err(|e| e.to_string())?;
    let code = inv.encode();
    println!("\nOn the other device, open nerve and press c in Settings, or run:\n\n    nerve connect {code}\n");
    if let Some(qr) = qr(&code) {
        println!("{qr}");
    }
    let joins = match (offer.is_empty(), later) {
        (true, _) => "this device".to_string(),
        (false, true) => "all your devices, and ones you add later".into(),
        (false, false) => format!("this device and {}", crate::util::text::list_join(&offer)),
    };
    println!("The code works once, for 10 minutes. The new device joins {joins}, once you allow it here.");
    if !super::agent::agent_alive(dir) {
        print(false, "no background agent runs here to hear the other device: run `nerve sync agent`");
    }
    if !(o.wait && std::io::IsTerminal::is_terminal(&std::io::stdin())) {
        return Ok("When it asks, allow it with `nerve allow <id>` (see `nerve requests`).".into());
    }
    allow_join(dir)
}

/// Wait for a device to use the code, then ask whether to let it in.
fn allow_join(dir: &Path) -> Result<String, String> {
    println!("\nWaiting for the other device (Ctrl-C stops waiting; the code stays valid)…");
    let start = crate::util::now_secs();
    let deadline = Instant::now() + Duration::from_secs(INVITE_TTL_SECS);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        let local: Option<protocol::DeviceState> =
            std::fs::read_to_string(super::agent::local_file(dir)).ok().and_then(|s| serde_json::from_str(&s).ok());
        let asked = local
            .into_iter()
            .flat_map(|l| l.approvals)
            .find(|a| a.at >= start && matches!(a.what, protocol::Asked::Join { .. }));
        let Some(a) = asked else { continue };
        let protocol::Asked::Join { name, words, .. } = &a.what else { continue };
        print!(
            "\n{} wants to connect. Check that it shows: {words}\nConnect it? [y/N] ",
            Sanitised::new(name).as_str()
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let yes = line.trim().eq_ignore_ascii_case("y");
        let d = protocol::Decision { id: a.id.clone(), yes, ..Default::default() };
        return super::ipc::ask_agent(dir, super::agent::Request::Fleet(super::agent::FleetReq::Decide(d)));
    }
    Err("nobody used the code within 10 minutes".into())
}

/// Ask the inviter to let this device in, and wait for its answer: the
/// devices it joins. Trusts the inviter first, so the answer gets through.
pub(crate) fn join(
    dir: &Path,
    keys: &nostr_sdk::prelude::Keys,
    inv: &Invite,
    want: Option<&[String]>,
) -> Result<Vec<String>, String> {
    let inviter = PublicKey::from_hex(&inv.k).map_err(|e| e.to_string())?;
    if let Some(d) = want.unwrap_or_default().iter().find(|d| !inv.d.contains(d)) {
        return Err(format!("{} doesn't offer {d}; it offers {}", inv.n, crate::util::text::list_join(&inv.d)));
    }
    let mut fleet = Fleet::load(dir).ok_or("not set up")?;
    let known = fleet.peer_keys().into_iter().find(|(_, k)| *k == inviter).map(|(n, _)| n);
    let name =
        known.clone().unwrap_or_else(|| unique_name(&inv.n, |n| fleet.peers.contains_key(n) || n == fleet.device));
    fleet.peers.insert(name.clone(), fleet::npub(&inviter));
    fleet.save(dir).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let (me, relays) = (fleet.device.clone(), fleet.relays.clone());
    let res = rt.block_on(async {
        let t = Transport::connect(keys.clone(), &relays).await?;
        t.subscribe(&[inviter], crate::util::now_secs().saturating_sub(5)).await?;
        let mut notes = t.client.notifications();
        let env = Envelope::new(
            &me,
            Body::PairRequest {
                secret: inv.s.clone(),
                name: me.clone(),
                relays: relays.clone(),
                want: want.map(<[String]>::to_vec),
            },
        );
        t.send(&inviter, &env).await?;
        let deadline = Instant::now() + Duration::from_secs(INVITE_TTL_SECS);
        // A yes brings the devices joined and an ack, in either order; a
        // no only the ack.
        let (mut welcome, mut acked) = (None, false);
        let out = loop {
            if let (Some(devices), true) = (&mut welcome, acked) {
                break Ok(std::mem::take(devices));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err("no answer within 10 minutes".to_string());
            }
            let Ok(Some(ClientNotification::Event { event, .. })) = tokio::time::timeout(left, notes.next()).await
            else {
                continue;
            };
            match t.open(&event).map(|i| i.envelope.body) {
                Some(Body::Welcome { re, devices }) if re == env.id => welcome = Some(devices),
                Some(Body::Ack { re, ok: true, .. }) if re == env.id => acked = true,
                Some(Body::Ack { re, ok: false, msg, .. }) if re == env.id => break Err(msg),
                _ => {}
            }
        };
        t.disconnect().await;
        out
    });
    let mut fleet = Fleet::load(dir).ok_or("not set up")?;
    let devices = match res {
        Ok(d) => d,
        Err(e) => {
            // Keep the half-connection out of the way: it doesn't trust us.
            if known.is_none() {
                fleet.peers.remove(&name);
                let _ = fleet.save(dir);
            }
            return Err(format!("not connected: {e}"));
        }
    };
    let mut joined = vec![name];
    for m in devices {
        let Ok(pk) = PublicKey::parse(&m.key) else { continue };
        if pk == keys.public_key() || fleet.peer_keys().iter().any(|(_, k)| *k == pk) {
            continue;
        }
        let n = unique_name(&m.name, |n| fleet.peers.contains_key(n) || n == fleet.device);
        fleet.peers.insert(n.clone(), fleet::npub(&pk));
        if !m.later {
            fleet.closed.push(n.clone());
        }
        joined.push(n);
    }
    fleet.save(dir).map_err(|e| e.to_string())?;
    Ok(joined)
}

/// The invite as a terminal QR code, for phones with a camera app that
/// copies text. None when the code is too long for a small QR.
fn qr(code: &str) -> Option<String> {
    let q = qrcode::QrCode::with_error_correction_level(code.as_bytes(), qrcode::EcLevel::L).ok()?;
    Some(q.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build())
}

// ---- invite codes ----
//
// `nerve connect` (or `c` in Settings) prints a code: `nerve1` +
// base64url(JSON {v, n: name, k: pubkey hex, s: secret, r: relays,
// d: the devices offered}). `nerve connect <code>` on the new device
// trusts the inviter's key, adopts its relays, and sends a `PairRequest`
// carrying the secret and which offered devices it wants. The inviter's
// agent accepts that one message from an unknown key, uses the secret
// up (once, within 10 minutes) and holds the join for a person's yes,
// with three words both screens show. On a yes it trusts the new device
// and introduces it to the devices it joins.
//
// On its own a leaked code connects nothing: someone has to allow the
// join, by name, while it waits.

pub(crate) const INVITE_TTL_SECS: u64 = 600;
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
    /// The inviter's devices the joiner may join, by the inviter's names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) d: Vec<String>,
}

/// What the inviter decided the new device may join.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Ticket {
    pub(crate) expires: u64,
    /// The devices offered.
    pub(crate) offer: Vec<String>,
    /// Devices that join later are connected to it too.
    pub(crate) later: bool,
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
        if inv.r.iter().any(|r| super::fleet::check_relay(r).is_err()) {
            return Err("invite lists a relay that isn't ws:// or wss://".into());
        }
        Ok(inv)
    }
}

fn valid_secret(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn invites_dir(sync_dir: &Path) -> PathBuf {
    sync_dir.join("invites")
}

fn ticket(p: &Path) -> Option<Ticket> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// A fresh single-use invite for this device, offering `offer`.
pub(crate) fn create(
    sync_dir: &Path,
    name: &str,
    pubkey_hex: &str,
    relays: &[String],
    offer: Vec<String>,
    later: bool,
) -> std::io::Result<Invite> {
    let d = invites_dir(sync_dir);
    std::fs::create_dir_all(&d)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    }
    // From the library's RNG, not `new_id`, which falls back to the clock.
    let secret: String = nostr_sdk::prelude::Keys::generate().secret_key().to_secret_hex().chars().take(32).collect();
    let t = Ticket { expires: crate::util::now_secs() + INVITE_TTL_SECS, offer: offer.clone(), later };
    std::fs::write(d.join(&secret), serde_json::to_string(&t).map_err(std::io::Error::other)?)?;
    Ok(Invite { v: 1, n: name.to_string(), k: pubkey_hex.to_string(), s: secret, r: relays.to_vec(), d: offer })
}

/// Use up an invite: its ticket only once, and only before it expires.
pub(crate) fn consume(sync_dir: &Path, secret: &str) -> Option<Ticket> {
    if !valid_secret(secret) {
        return None;
    }
    let p = invites_dir(sync_dir).join(secret);
    let t = ticket(&p);
    // Remove first: two racing requests can't both win.
    std::fs::remove_file(&p).ok()?;
    t.filter(|t| crate::util::now_secs() <= t.expires)
}

/// Withdraw an invite before anyone uses it.
pub(crate) fn cancel(sync_dir: &Path, secret: &str) {
    if valid_secret(secret) {
        let _ = std::fs::remove_file(invites_dir(sync_dir).join(secret));
    }
}

/// Any unexpired invite outstanding? The agent only listens to unknown
/// keys while one is. Expired ones are deleted on the way.
pub(crate) fn any_open(sync_dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(invites_dir(sync_dir)) else { return false };
    let now = crate::util::now_secs();
    let mut open = false;
    for e in rd.flatten() {
        let alive = ticket(&e.path()).is_some_and(|t| now <= t.expires);
        if alive {
            open = true;
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
    open
}

/// Three words from both keys and the secret. Both screens show them, so
/// a person can see the join they allow is the one they started.
pub(crate) fn words(inviter_hex: &str, joiner_hex: &str, secret: &str) -> String {
    let h = super::chunks::id_of(format!("{inviter_hex}:{joiner_hex}:{secret}").as_bytes());
    let w: Vec<&str> = WORDS.split_whitespace().collect();
    (0..3)
        .map(|i| w[usize::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0) % w.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

/// 256 short words that are hard to mix up when read aloud.
const WORDS: &str = "\
acorn actor adult agent alarm album alert alley amber anchor angle ankle apple april arena armor arrow atlas attic \
audio autumn avenue badge bagel baker bamboo banjo barn basket beach beard beaver bench berry bicycle bison blade \
blanket blossom bonus border bottle bounce branch bread brick bridge bronze brush bubble bucket buffalo bundle butter \
button cabin cactus camel camera candle canoe canvas canyon carbon carpet carrot castle cello chalk cheese cherry \
chess cider circle citrus clover cobalt coconut comet copper coral cotton cougar crayon cricket crystal dagger daisy \
dancer delta denim desert diamond dolphin donkey dragon drawer dream drum eagle easel echo eclipse elbow ember engine \
falcon feather fence ferry fig finch flame flute forest fossil fountain fox galaxy garden garlic gecko ginger glacier \
globe goblet gorilla grape gravel guitar hammer harbor harvest hazel helmet heron hollow honey hornet igloo island \
ivory jacket jaguar jasmine jelly jungle kayak kettle kiwi koala ladder lagoon lantern lemon lily linen lizard lobster \
locket lotus magnet mango maple marble meadow melon mirror mitten monkey mosaic moss muffin needle nectar noodle \
nutmeg oasis ocean olive onion opal orbit orchid otter oyster paddle palace panda paper parrot pasta peach pebble \
pelican pepper piano pickle pigeon pillow pine planet plum pocket pony poppy potato prism pumpkin puzzle quartz quill \
rabbit radar radish raven ribbon river robin rocket saddle salmon sandal satin scarf shell silver sketch sled spider \
spruce squid stable statue summit sunset swan table tiger timber toast tomato torch tulip tunnel turtle velvet violin \
walnut walrus whale willow window yogurt zebra zipper";

/// A name for the joining device that doesn't clash with existing peers.
pub(crate) fn unique_name(wanted: &str, taken: impl Fn(&str) -> bool) -> String {
    let base: String =
        wanted.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(32).collect();
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
    fn invite_codes_fit_a_terminal_qr() {
        let d = vec!["studio".into(), "mini".into(), "server".into(), "pi".into()];
        let inv = Invite {
            v: 1,
            n: "laptop".into(),
            k: "ab".repeat(32),
            s: "0f".repeat(16),
            r: vec!["ws://192.0.2.10:7777".into(), DEFAULT_RELAYS[0].into(), DEFAULT_RELAYS[1].into()],
            d,
        };
        let q = qr(&inv.encode()).expect("fits");
        assert!(q.lines().count() < 45, "{} rows", q.lines().count());
    }

    #[test]
    fn a_new_device_joins_who_the_inviter_says() {
        let mut f = Fleet { device: "laptop".into(), ..Default::default() };
        for d in ["studio", "pi", "mini"] {
            f.peers.insert(d.into(), "npub1x".into());
        }
        f.closed.push("pi".into());
        assert_eq!(
            offer(&f, None).unwrap(),
            (vec!["mini".into(), "studio".into()], true),
            "all: every device that meets new ones, and later ones"
        );
        assert_eq!(offer(&f, Some("now")).unwrap(), (vec!["mini".into(), "studio".into()], false));
        assert_eq!(
            offer(&f, Some("pi, studio")).unwrap(),
            (vec!["pi".into(), "studio".into()], false),
            "named devices, closed ones too"
        );
        assert!(offer(&f, Some("air")).unwrap_err().contains("unknown device air"));
    }

    #[test]
    fn code_round_trips_and_rejects_damage() {
        let inv = Invite {
            v: 1,
            n: "laptop".into(),
            k: "ab".repeat(32),
            s: "0f".repeat(16),
            r: vec!["wss://relay.primal.net".into()],
            d: vec!["studio".into()],
        };
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
        let inv = create(d.path(), "laptop", &"ab".repeat(32), &[], vec!["studio".into()], true).unwrap();
        assert_eq!(inv.d, ["studio"]);
        assert!(any_open(d.path()));
        assert!(consume(d.path(), &"0".repeat(32)).is_none(), "unknown secret");
        let t = consume(d.path(), &inv.s).unwrap();
        assert_eq!((t.offer, t.later), (vec!["studio".to_string()], true));
        assert!(consume(d.path(), &inv.s).is_none(), "second use");
        assert!(!any_open(d.path()));

        let old = create(d.path(), "laptop", &"ab".repeat(32), &[], vec![], false).unwrap();
        std::fs::write(d.path().join("invites").join(&old.s), r#"{"expires":1,"offer":[],"later":false}"#).unwrap();
        assert!(!any_open(d.path()), "expired invites don't count and are removed");
        assert!(consume(d.path(), &old.s).is_none());
        assert!(consume(d.path(), "../fleet.toml").is_none());

        let gone = create(d.path(), "laptop", &"ab".repeat(32), &[], vec![], false).unwrap();
        cancel(d.path(), &gone.s);
        assert!(consume(d.path(), &gone.s).is_none(), "a cancelled code is dead");
    }

    #[test]
    fn both_sides_get_the_same_words_and_others_different_ones() {
        let (a, b, s) = ("ab".repeat(32), "cd".repeat(32), "0f".repeat(16));
        let w = words(&a, &b, &s);
        assert_eq!(w.split(' ').count(), 3);
        assert_eq!(w, words(&a, &b, &s));
        assert_ne!(w, words(&a, &"ce".repeat(32), &s), "another joiner's key gives other words");
        assert_eq!(WORDS.split_whitespace().count(), 256);
    }

    #[test]
    fn joining_names_never_clash() {
        let taken = |n: &str| n == "pixel" || n == "pixel-2";
        assert_eq!(unique_name("pixel", taken), "pixel-3");
        assert_eq!(unique_name("my phone!", |_| false), "myphone");
        assert_eq!(unique_name("", |_| false), "device");
    }
}
