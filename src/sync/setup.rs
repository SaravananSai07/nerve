//! `nerve setup [code]`: everything a device needs, in one command.
//!
//! 1. Create this device's identity (once) with working default relays.
//! 2. Join: with an invite code, trust the inviter and ask it to trust
//!    this device back (see `pairing`).
//! 3. Connect Claude: install the nerve mod (Claude Code ≥ 2.1.287), or
//!    fall back to settings hooks on older versions.
//! 4. Keep the agent running: a launchd agent (macOS), a systemd user
//!    service (Linux), or a detached process (Termux, containers).
//! 5. Without a code: print an invite for the next device and wait for
//!    it to join.
//!
//! Re-running it is safe: every step checks what is already in place.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::{ClientNotification, PublicKey, StreamExt};

use super::fleet::{self, Fleet};
use super::pairing::{self, Invite};
use super::protocol::{self, Body, Envelope};
use super::transport::Transport;
use crate::paths::Paths;

/// Relays that delivered for fresh keys in testing (E14).
pub(crate) const DEFAULT_RELAYS: [&str; 2] = ["wss://relay.primal.net", "wss://relay.snort.social"];
/// First Claude Code release with mods.
const MODS_SINCE: (u32, u32, u32) = (2, 1, 287);

const MOD_MARKETPLACE: &str = include_str!("../../mod/.claude-plugin/marketplace.json");
const MOD_PLUGIN: &str = include_str!("../../mod/nerve/.claude-plugin/plugin.json");
const MOD_HOOKS: &str = include_str!("../../mod/nerve/hooks/hooks.json");
const MOD_REGISTER: &str = include_str!("../../mod/nerve/hooks/register.js");

struct Opts {
    code: Option<String>,
    name: Option<String>,
    relays: Vec<String>,
    service: bool,
    claude: bool,
    wait: bool,
}

fn parse(args: &[&str]) -> Result<Opts, String> {
    let mut o = Opts { code: None, name: None, relays: Vec::new(), service: true, claude: true, wait: true };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--name" => o.name = Some(it.next().ok_or("--name needs a value")?.to_string()),
            "--relay" => o.relays.push(it.next().ok_or("--relay needs a URL")?.to_string()),
            "--no-service" => o.service = false,
            "--no-claude" => o.claude = false,
            "--no-wait" => o.wait = false,
            c if c.starts_with("nerve1") => o.code = Some(c.to_string()),
            other => return Err(format!("unknown argument {other}\nusage: nerve setup [<invite code>] [--name N] [--relay URL]... [--no-service] [--no-claude] [--no-wait]")),
        }
    }
    Ok(o)
}

fn ok(msg: impl AsRef<str>) {
    println!("  \u{2713} {}", msg.as_ref());
}

fn warn(msg: impl AsRef<str>) {
    println!("  ! {}", msg.as_ref());
}

pub(crate) fn run(paths: &Paths, args: &[&str]) -> Result<String, String> {
    let o = parse(args)?;
    let dir = fleet::sync_dir(paths);
    let invite = o.code.as_deref().map(Invite::decode).transpose()?;
    println!("nerve setup");

    // 1. Identity.
    let fresh = Fleet::load(&dir).is_none();
    let mut fleet = Fleet::load(&dir).unwrap_or_default();
    if let Some(n) = &o.name {
        fleet.device = n.clone();
    }
    let mut relays: Vec<String> = Vec::new();
    relays.extend(o.relays.iter().cloned());
    if let Some(inv) = &invite {
        relays.extend(inv.r.iter().cloned());
    }
    if fleet.relays.is_empty() && relays.is_empty() {
        relays.extend(DEFAULT_RELAYS.iter().map(|s| s.to_string()));
    }
    for r in relays {
        if !(r.starts_with("ws://") || r.starts_with("wss://")) {
            return Err(format!("relay must be ws:// or wss://: {r}"));
        }
        if !fleet.relays.contains(&r) {
            fleet.relays.push(r);
        }
    }
    let keys = fleet::device_keys(&dir).map_err(|e| e.to_string())?;
    fleet.save(&dir).map_err(|e| e.to_string())?;
    ok(format!(
        "{} '{}' ({}), relays: {}",
        if fresh { "created device" } else { "device" },
        fleet.device,
        short(&fleet::npub(&keys.public_key())),
        fleet.relays.join(" ")
    ));

    // 2. Join.
    if let Some(inv) = &invite {
        join(&dir, &mut fleet, &keys, inv)?;
    }

    // 3. Claude.
    if o.claude {
        connect_claude(&dir);
    }

    // 4. Agent.
    if o.service {
        match install_service(paths, &dir) {
            Ok(how) => ok(how),
            Err(e) => warn(format!("background agent not installed: {e}. Run `nerve sync agent` yourself.")),
        }
    }

    // 5. Invite the next device.
    if invite.is_none() {
        let inv = pairing::create(&dir, &fleet.device, &keys.public_key().to_hex(), &fleet.relays).map_err(|e| e.to_string())?;
        let code = inv.encode();
        println!("\nAdd another device: on it, run\n\n    nerve setup {code}\n");
        if let Some(qr) = qr(&code) {
            println!("{qr}");
        }
        println!("The code works once, for the next hour. Anyone holding it can pair a device, so treat it like a password.");
        if o.wait && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            wait_for_join(&dir, fleet.peers.len());
        }
    }
    Ok(String::new())
}

fn short(npub: &str) -> String {
    format!("{}…{}", &npub[..12.min(npub.len())], &npub[npub.len().saturating_sub(6)..])
}

/// Trust the inviter, then prove the invite to it and wait for its ack.
fn join(dir: &Path, fleet: &mut Fleet, keys: &nostr_sdk::prelude::Keys, inv: &Invite) -> Result<(), String> {
    let inviter = PublicKey::from_hex(&inv.k).map_err(|e| e.to_string())?;
    if inviter == keys.public_key() {
        return Err("that invite came from this device; run it on the other one".into());
    }
    let existing = fleet.peer_keys().into_iter().find(|(_, k)| *k == inviter).map(|(n, _)| n);
    let name = existing.unwrap_or_else(|| {
        let f = fleet.clone();
        pairing::unique_name(&inv.n, |n| f.peers.contains_key(n) || n == f.device)
    });
    fleet.peers.insert(name.clone(), fleet::npub(&inviter));
    fleet.save(dir).map_err(|e| e.to_string())?;
    print!("  … asking {name} to pair back");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let me = fleet.device.clone();
    let relays = fleet.relays.clone();
    let res = rt.block_on(async {
        let t = Transport::connect(keys.clone(), &relays).await?;
        t.subscribe(&[inviter], protocol::now_secs().saturating_sub(5)).await?;
        let mut notes = t.client.notifications();
        let env = Envelope::new(&me, Body::PairRequest { secret: inv.s.clone(), name: me.clone(), relays: relays.clone() });
        t.send(&inviter, &env).await?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let out = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err("no answer within 60 s: is nerve set up and running on the inviting device?".to_string());
            }
            let Ok(Some(ClientNotification::Event { event, .. })) = tokio::time::timeout(left, notes.next()).await else {
                continue;
            };
            match t.open(&event).map(|i| i.envelope.body) {
                Some(Body::Ack { re, ok: true, msg }) if re == env.id => break Ok(msg),
                Some(Body::Ack { re, ok: false, msg }) if re == env.id => break Err(msg),
                _ => {}
            }
        };
        t.disconnect().await;
        out
    });
    println!();
    match res {
        Ok(msg) => {
            ok(format!("paired with {name}: {msg}"));
            Ok(())
        }
        Err(e) => {
            // Keep the half-pairing out of the way: they don't trust us.
            fleet.peers.remove(&name);
            let _ = fleet.save(dir);
            Err(format!("pairing failed: {e}"))
        }
    }
}

fn wait_for_join(dir: &Path, before: usize) {
    println!("\nWaiting for a device to join (Ctrl-C to stop waiting; the code stays valid)…");
    let deadline = Instant::now() + Duration::from_secs(pairing::INVITE_TTL_SECS);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(f) = Fleet::load(dir) {
            if f.peers.len() > before {
                let names: Vec<&String> = f.peers.keys().collect();
                ok(format!("a device joined. Paired devices: {}", names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
                println!("\nRun `nerve` and press D to see them.");
                return;
            }
        }
    }
}

/// "2.1.295 (Claude Code)" → (2, 1, 295).
pub(crate) fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let v = s.split_whitespace().next()?;
    let mut p = v.split('.').map(|x| x.parse::<u32>().ok());
    Some((p.next()??, p.next()??, p.next()??))
}

fn claude_version() -> Option<(u32, u32, u32)> {
    let out = Command::new("claude").arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

fn connect_claude(dir: &Path) {
    let Some(v) = claude_version() else {
        warn("Claude Code not found on PATH: sessions can be watched once it's installed; re-run `nerve setup` after.");
        return;
    };
    if v < MODS_SINCE {
        let msg = super::cli::install_hooks(dir).unwrap_or_else(|e| e);
        warn(format!("Claude Code {}.{}.{} predates mods (2.1.287); using settings hooks instead.", v.0, v.1, v.2));
        println!("    {}", msg.replace('\n', "\n    "));
        return;
    }
    match install_mod(dir) {
        Ok(()) => ok(format!("nerve mod installed for Claude Code {}.{}.{} (new sessions load it)", v.0, v.1, v.2)),
        Err(e) => warn(format!("couldn't install the nerve mod: {e}")),
    }
}

/// Write the mod into `<sync>/mod` and register it as a local plugin
/// marketplace. The plugin reads its socket path from `nerve.json`.
fn install_mod(dir: &Path) -> Result<(), String> {
    let root = dir.join("mod");
    let files: [(&str, &str); 4] = [
        (".claude-plugin/marketplace.json", MOD_MARKETPLACE),
        ("nerve/.claude-plugin/plugin.json", MOD_PLUGIN),
        ("nerve/hooks/hooks.json", MOD_HOOKS),
        ("nerve/hooks/register.js", MOD_REGISTER),
    ];
    for (rel, body) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap_or(&root)).map_err(|e| e.to_string())?;
        std::fs::write(&p, body).map_err(|e| e.to_string())?;
    }
    let sock = super::modhub::socket_path(dir);
    let cfg = serde_json::json!({ "socket": sock.to_string_lossy() });
    std::fs::write(root.join("nerve/nerve.json"), cfg.to_string()).map_err(|e| e.to_string())?;
    let claude = |args: &[&str]| -> Result<String, String> {
        let out = Command::new("claude").args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if out.status.success() { Ok(text) } else { Err(text.trim().to_string()) }
    };
    let root_s = root.to_string_lossy().into_owned();
    if let Err(e) = claude(&["plugin", "marketplace", "add", &root_s]) {
        if !e.contains("already") {
            return Err(e);
        }
        let _ = claude(&["plugin", "marketplace", "update", "nerve-local"]);
    }
    match claude(&["plugin", "install", "nerve@nerve-local"]) {
        Ok(_) => Ok(()),
        Err(e) if e.contains("already") => claude(&["plugin", "enable", "nerve@nerve-local"]).map(|_| ()).or(Ok(())),
        Err(e) => Err(e),
    }
}

fn exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| e.to_string())
}

/// A per-config suffix, so a second nerve config (the demo, a test)
/// gets its own service instead of replacing the real one.
fn label_suffix() -> String {
    match std::env::var("NERVE_CONFIG_DIR") {
        Ok(d) if !d.is_empty() => format!(".{}", &super::chunks::id_of(d.as_bytes())[..8]),
        _ => String::new(),
    }
}

fn is_termux() -> bool {
    std::env::var("PREFIX").is_ok_and(|p| p.contains("com.termux"))
}

fn install_service(paths: &Paths, dir: &Path) -> Result<String, String> {
    let exe = exe()?;
    let log = dir.join("agent.log");
    let path_env = std::env::var("PATH").unwrap_or_default();
    let cfg_env = std::env::var("NERVE_CONFIG_DIR").ok().filter(|s| !s.is_empty());
    let _ = paths;
    if cfg!(target_os = "macos") {
        let label = format!("dev.nerve.sync{}", label_suffix());
        let plist_dir = dirs::home_dir().ok_or("no home dir")?.join("Library/LaunchAgents");
        std::fs::create_dir_all(&plist_dir).map_err(|e| e.to_string())?;
        let plist = plist_dir.join(format!("{label}.plist"));
        let mut env = format!("<key>PATH</key><string>{}</string>", xml(&path_env));
        if let Some(c) = &cfg_env {
            env.push_str(&format!("<key>NERVE_CONFIG_DIR</key><string>{}</string>", xml(c)));
        }
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array><string>{exe}</string><string>sync</string><string>agent</string></array>
<key>EnvironmentVariables</key><dict>{env}</dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>StandardOutPath</key><string>{log}</string>
<key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
            exe = xml(&exe.to_string_lossy()),
            log = xml(&log.to_string_lossy()),
        );
        std::fs::write(&plist, body).map_err(|e| e.to_string())?;
        let domain = format!("gui/{}", nix::unistd::getuid());
        let _ = Command::new("launchctl").args(["bootout", &domain, &plist.to_string_lossy()]).stderr(Stdio::null()).status();
        let st = Command::new("launchctl").args(["bootstrap", &domain, &plist.to_string_lossy()]).status().map_err(|e| e.to_string())?;
        if !st.success() {
            return Err("launchctl bootstrap failed".into());
        }
        return Ok(format!("background agent runs at login (launchd {label}; log {})", log.display()));
    }
    let systemd = !is_termux()
        && Command::new("systemctl").args(["--user", "show-environment"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if systemd {
        let name = format!("nerve-sync{}", label_suffix());
        let unit_dir = dirs::config_dir().ok_or("no config dir")?.join("systemd/user");
        std::fs::create_dir_all(&unit_dir).map_err(|e| e.to_string())?;
        let mut env = format!("Environment=PATH={path_env}\n");
        if let Some(c) = &cfg_env {
            env.push_str(&format!("Environment=NERVE_CONFIG_DIR={c}\n"));
        }
        let unit = format!(
            "[Unit]\nDescription=nerve sync agent\nAfter=network-online.target\n\n[Service]\nExecStart={} sync agent\n{env}Restart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
            exe.display()
        );
        std::fs::write(unit_dir.join(format!("{name}.service")), unit).map_err(|e| e.to_string())?;
        let sc = |a: &[&str]| Command::new("systemctl").arg("--user").args(a).status().is_ok_and(|s| s.success());
        if !(sc(&["daemon-reload"]) && sc(&["enable", &format!("{name}.service")]) && sc(&["restart", &format!("{name}.service")])) {
            return Err("systemctl --user failed".into());
        }
        // Without lingering, user services stop at logout: a headless Pi
        // would lose its agent when the SSH session ends.
        let user = std::env::var("USER").unwrap_or_default();
        let linger = Command::new("loginctl").args(["enable-linger", &user]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        return Ok(format!(
            "background agent runs as systemd user service {name}{}",
            if linger { " (also while logged out)" } else { "; run `sudo loginctl enable-linger $USER` to keep it up while logged out" }
        ));
    }
    // No service manager (Termux, containers): start it detached now.
    if super::agent::agent_alive(dir) {
        return Ok("background agent already running".into());
    }
    let logf = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.args(["sync", "agent"]).stdin(Stdio::null()).stdout(logf.try_clone().map_err(|e| e.to_string())?).stderr(logf);
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().map_err(|e| e.to_string())?;
    let mut how = format!("background agent started (log {})", log.display());
    if is_termux() {
        let _ = Command::new("termux-wake-lock").status();
        if let Some(boot) = dirs::home_dir().map(|h| h.join(".termux/boot")) {
            if std::fs::create_dir_all(&boot).is_ok() {
                let script = format!("#!/data/data/com.termux/files/usr/bin/sh\ntermux-wake-lock\nexec {} sync agent >> {} 2>&1\n", exe.display(), log.display());
                let p = boot.join("nerve-sync");
                if std::fs::write(&p, script).is_ok() {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
                    how.push_str("; with the Termux:Boot app it also starts at boot");
                }
            }
        }
    }
    Ok(how)
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The invite as a terminal QR code, for phones with a camera app that
/// copies text. None when the code is too long for a small QR.
fn qr(code: &str) -> Option<String> {
    let q = qrcode::QrCode::with_error_correction_level(code.as_bytes(), qrcode::EcLevel::L).ok()?;
    Some(q.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_versions_compare() {
        assert_eq!(parse_version("2.1.295 (Claude Code)"), Some((2, 1, 295)));
        assert!(parse_version("2.1.286 (Claude Code)").unwrap() < MODS_SINCE);
        assert!(parse_version("2.2.0").unwrap() >= MODS_SINCE);
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn embedded_mod_is_consistent() {
        let m: serde_json::Value = serde_json::from_str(MOD_MARKETPLACE).unwrap();
        assert_eq!(m["name"], "nerve-local");
        assert_eq!(m["plugins"][0]["name"], "nerve");
        let h: serde_json::Value = serde_json::from_str(MOD_HOOKS).unwrap();
        assert_eq!(h["modules"][0], "./register.js");
        assert!(MOD_REGISTER.contains("/v1/poll") && MOD_REGISTER.contains("/v1/request"));
    }

    #[test]
    fn invite_codes_fit_a_terminal_qr() {
        let inv = Invite { v: 1, n: "macbook".into(), k: "ab".repeat(32), s: "0f".repeat(16), r: vec!["ws://100.73.26.46:7777".into(), DEFAULT_RELAYS[0].into(), DEFAULT_RELAYS[1].into()] };
        let q = qr(&inv.encode()).expect("fits");
        assert!(q.lines().count() < 45, "{} rows", q.lines().count());
    }
}
