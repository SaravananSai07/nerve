//! `nerve sync ...` subcommands. Usage is in `USAGE`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::agent::{self, Ctx, Request};
use super::fleet::{self, Fleet};
use super::mailbox::{self, Mailbox};
use super::protocol::{self, Answer, Body, FetchWhat, JobSpec};
use super::transport::Transport;
use crate::paths::Paths;
use crate::util::sanitize::Sanitised;

const USAGE: &str = "\
nerve sync — share agent progress across your devices over Nostr

  nerve setup [<code>]               everything in one step (identity, Claude mod,
                                     background agent, pairing); prints a code for
                                     the next device, or joins with one
  invite                             print a fresh one-time code for another device
  doctor                             check this device's setup end to end

  init [--name N] [--relay URL]...   create this device's key + fleet.toml
  id                                 print this device's npub (give it to peers)
  pair <name> <npub>                 trust a peer device
  relay <add|rm> <url>               edit the relay list
  install-hooks                      write Claude settings for remote answers
  serve-relay [--port 7777]          run a private Nostr relay (e.g. on a Pi)
  agent                              run the sync agent headless (Pi, server)
  status [--json]                    peers: last synced, sessions, pending
  sync                               republish now and ask peers to
  send <device> <session> <text>     type an instruction into a session
  answer <device> <request> allow|deny|<label>[,<label>...]
  fetch <device> <session> diff|log|subagents
  handoff <session> <device> [--then <text>]   continue a session elsewhere
  job [--on D] [--needs cap,cap] [--allow RULE]... [--verify CMD] [--cwd DIR] [--repo URL] [--model M] <title> -- <prompt>
  away on|off                        route Claude's questions to peers
  hook                               (Claude hook entry point)
";

pub(crate) fn run(paths: &Paths, args: &[String]) -> std::io::Result<()> {
    let dir = fleet::sync_dir(paths);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match a.as_slice() {
        ["setup", rest @ ..] => super::setup::run(paths, rest),
        ["invite"] => invite(&dir),
        ["doctor"] => doctor(paths, &dir),
        ["init", rest @ ..] => init(&dir, rest),
        ["id"] => id(&dir),
        ["pair", name, key] => edit(&dir, |f| {
            nostr_sdk::prelude::PublicKey::parse(key).map_err(|e| format!("bad key: {e}"))?;
            f.peers.insert(name.to_string(), key.to_string());
            Ok(format!("paired {name}"))
        }),
        ["relay", "add", url] => edit(&dir, |f| {
            if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                return Err("relay must be ws:// or wss://".into());
            }
            if !f.relays.iter().any(|r| r == url) {
                f.relays.push(url.to_string());
            }
            Ok(format!("relays: {}", f.relays.join(", ")))
        }),
        ["relay", "rm", url] => edit(&dir, |f| {
            f.relays.retain(|r| r != url);
            Ok(format!("relays: {}", f.relays.join(", ")))
        }),
        ["install-hooks"] => install_hooks(&dir),
        ["serve-relay", rest @ ..] => serve_relay(rest),
        ["agent"] => run_agent(paths, &dir),
        ["status", rest @ ..] => status(&dir, rest.contains(&"--json")),
        ["sync"] => oneshot(&dir, |_| vec![Request::SyncNow], 2),
        ["send", device, session, text @ ..] if !text.is_empty() => {
            let body = Body::Send { session: session.to_string(), text: text.join(" ") };
            oneshot(&dir, |_| vec![Request::To { device: device.to_string(), body }], 15)
        }
        ["answer", device, request, choice] => {
            let answer = match *choice {
                "allow" | "yes" => Answer::Allow,
                "deny" | "no" => Answer::Deny { reason: "denied from another device".into() },
                labels => Answer::Choose { labels: labels.split(',').map(String::from).collect() },
            };
            let body = Body::Answer { request: request.to_string(), answer };
            oneshot(&dir, |_| vec![Request::To { device: device.to_string(), body }], 15)
        }
        ["fetch", device, session, what] => {
            let what = match *what {
                "diff" => FetchWhat::Diff,
                "log" => FetchWhat::Log,
                "subagents" => FetchWhat::Subagents,
                _ => return usage(),
            };
            fetch(&dir, device, session, what)
        }
        ["handoff", session, device, rest @ ..] => handoff(paths, &dir, session, device, rest),
        ["job", rest @ ..] => job(&dir, rest),
        ["away", "on"] => oneshot(&dir, |_| vec![Request::SetAway(true)], 2),
        ["away", "off"] => oneshot(&dir, |_| vec![Request::SetAway(false)], 2),
        ["hook"] => hook(&dir),
        _ => return usage(),
    };
    match result {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("nerve sync: {e}");
            std::process::exit(1);
        }
    }
}

fn usage() -> std::io::Result<()> {
    eprint!("{USAGE}");
    std::process::exit(2);
}

fn load(dir: &std::path::Path) -> Result<Fleet, String> {
    Fleet::load(dir).ok_or_else(|| "not set up; run `nerve sync init` first".into())
}

fn edit(dir: &std::path::Path, f: impl FnOnce(&mut Fleet) -> Result<String, String>) -> Result<String, String> {
    let mut fleet = load(dir)?;
    let msg = f(&mut fleet)?;
    fleet.save(dir).map_err(|e| e.to_string())?;
    Ok(msg)
}

fn init(dir: &std::path::Path, rest: &[&str]) -> Result<String, String> {
    let mut fleet = Fleet::load(dir).unwrap_or_default();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match *arg {
            "--name" => fleet.device = it.next().ok_or("--name needs a value")?.to_string(),
            "--relay" => {
                let r = it.next().ok_or("--relay needs a URL")?.to_string();
                if !fleet.relays.contains(&r) {
                    fleet.relays.push(r);
                }
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let keys = fleet::device_keys(dir).map_err(|e| e.to_string())?;
    fleet.save(dir).map_err(|e| e.to_string())?;
    Ok(format!(
        "device '{}' ready\n  npub: {}\n  config: {}\nOn each other device: nerve sync pair {} {}",
        fleet.device,
        fleet::npub(&keys.public_key()),
        Fleet::file(dir).display(),
        fleet.device,
        fleet::npub(&keys.public_key())
    ))
}

fn invite(dir: &std::path::Path) -> Result<String, String> {
    let fleet = load(dir)?;
    let keys = fleet::device_keys(dir).map_err(|e| e.to_string())?;
    let inv = super::pairing::create(dir, &fleet.device, &keys.public_key().to_hex(), &fleet.relays).map_err(|e| e.to_string())?;
    if !agent::agent_alive(dir) {
        eprintln!("note: no sync agent is running here, so nobody will answer the joining device. Start one: nerve sync agent");
    }
    Ok(format!("On the other device run:\n\n    nerve setup {}\n\nValid once, for the next hour.", inv.encode()))
}

/// One line per check, with what to do about a failure.
fn doctor(paths: &Paths, dir: &std::path::Path) -> Result<String, String> {
    let mut out = String::new();
    let mut line = |ok: bool, what: String| out.push_str(&format!("  {} {what}\n", if ok { "\u{2713}" } else { "\u{2717}" }));
    let Some(fleet) = Fleet::load(dir) else {
        return Ok("  \u{2717} not set up: run `nerve setup`\n".into());
    };
    line(true, format!("device '{}', {} peer(s), {} relay(s)", fleet.device, fleet.peers.len(), fleet.relays.len()));
    let alive = agent::agent_alive(dir);
    line(alive, if alive { "sync agent running".into() } else { "sync agent not running: `nerve setup` installs it, or run `nerve sync agent`".into() });
    let sock = super::modhub::socket_path(dir);
    line(sock.exists(), format!("mod socket {}", sock.display()));
    let mod_json = dir.join("mod/nerve/nerve.json");
    let mod_ok = std::fs::read_to_string(&mod_json).is_ok_and(|s| s.contains(&*sock.to_string_lossy()));
    line(mod_ok, if mod_ok { "nerve mod installed and pointed at this agent".into() } else { "nerve mod missing or stale: run `nerve setup`".into() });
    let sessions = crate::detect::claude::discover_sessions(Some(paths.statusline_dir()));
    line(true, format!("{} Claude session(s) on this device", sessions.len()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let keys = fleet::device_keys(dir).map_err(|e| e.to_string())?;
    let peers = fleet.peer_keys();
    let (states, health) = rt.block_on(async {
        let t = Transport::connect(keys, &fleet.relays).await?;
        let s = t.fetch_states(&peers.iter().map(|(_, k)| *k).collect::<Vec<_>>()).await;
        let h = t.health().await;
        t.disconnect().await;
        Ok::<_, String>((s, h))
    })?;
    for h in &health {
        line(h.connected, format!("relay {} {}", h.url, if h.connected { "connected" } else { "unreachable" }));
    }
    let now = protocol::now_secs();
    for (name, pk) in &peers {
        let st = states.iter().find(|s| s.sender == *pk).and_then(|s| match &s.envelope.body {
            Body::State(d) => Some(d.generated_at),
            _ => None,
        });
        match st {
            Some(g) => line(now.saturating_sub(g) < 180, format!("{name}: last synced {}", crate::util::text::format_age(now.saturating_sub(g) as f64))),
            None => line(false, format!("{name}: no state seen (is its agent running? same relays?)")),
        }
    }
    Ok(out.trim_end().to_string())
}

fn id(dir: &std::path::Path) -> Result<String, String> {
    let keys = fleet::device_keys(dir).map_err(|e| e.to_string())?;
    Ok(fleet::npub(&keys.public_key()))
}

/// Claude settings that route permission prompts and questions through
/// the mailbox. Used with `claude --settings <file>` or merged into
/// ~/.claude/settings.json by hand.
pub(crate) fn install_hooks(dir: &std::path::Path) -> Result<String, String> {
    let fleet = load(dir)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cmd = format!("'{}' sync hook", exe.display());
    let t = fleet.hook_timeout_secs + 15;
    let settings = serde_json::json!({
        "hooks": {
            "PermissionRequest": [{"matcher": "*", "hooks": [{"type": "command", "command": cmd, "timeout": t}]}],
            "PreToolUse": [{"matcher": "AskUserQuestion", "hooks": [{"type": "command", "command": cmd, "timeout": t}]}]
        }
    });
    let path = dir.join("claude-settings.json");
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap_or_default()).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {p}\nStart sessions with:  claude --settings {p}\nor merge its \"hooks\" into ~/.claude/settings.json to cover every session.",
        p = path.display()
    ))
}

/// Claude hook entry point. Prints a decision when a peer answers; prints
/// nothing (= Claude asks locally as usual) on timeout, when the user
/// answers at the keyboard, or when no sync agent is running.
fn hook(dir: &std::path::Path) -> Result<String, String> {
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw).map_err(|e| e.to_string())?;
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let fleet = Fleet::load(dir).unwrap_or_default();
    if !agent::agent_alive(dir) {
        return Ok(String::new());
    }
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("").to_string();
    let is_question = input.get("hook_event_name").and_then(Value::as_str) == Some("PreToolUse");
    if is_question {
        // The question dialog stays hidden while this hook runs, so only
        // take it remote when nobody is at this keyboard.
        let away = agent::away_flag(dir).exists();
        let allowed = match fleet.remote_questions.as_str() {
            "always" => true,
            "never" => false,
            _ => away,
        };
        if !allowed {
            return Ok(String::new());
        }
    } else if !fleet.remote_permissions {
        return Ok(String::new());
    }
    let transcript = input.get("transcript_path").and_then(Value::as_str).map(PathBuf::from);
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    let answered_locally = || {
        transcript
            .as_deref()
            .is_some_and(|t| mailbox::resolved_in_transcript(t, &tool, &tool_input))
    };
    let mb = Mailbox::new(dir);
    Ok(mb.wait(&input, Duration::from_secs(fleet.hook_timeout_secs), answered_locally).unwrap_or_default())
}

fn serve_relay(rest: &[&str]) -> Result<String, String> {
    let port: u16 = match rest {
        ["--port", p] => p.parse().map_err(|_| "bad port")?,
        [] => 7777,
        _ => return Err("usage: serve-relay [--port N]".into()),
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        use nostr_sdk::prelude::*;
        // Events up to 256 KiB; NIP-40 expiry keeps the in-memory store small.
        let relay = LocalRelay::builder()
            .addr("0.0.0.0".parse().map_err(|e: std::net::AddrParseError| e.to_string())?)
            .port(port)
            .max_event_size(256 * 1024)
            .max_websocket_message_size(512 * 1024)
            .build();
        relay.run().await.map_err(|e| e.to_string())?;
        eprintln!("nerve relay listening on ws://0.0.0.0:{port} (in-memory)");
        let _ = tokio::signal::ctrl_c().await;
        Ok::<_, String>(String::new())
    })
}

fn run_agent(paths: &Paths, dir: &std::path::Path) -> Result<String, String> {
    let fleet = load(dir)?;
    let handle = agent::spawn(Ctx { sync_dir: dir.to_path_buf(), fleet, statusline_dir: Some(paths.statusline_dir()), long_lived: true })
        .map_err(|e| e.to_string())?;
    let mut printed = 0;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let v = handle.snapshot();
        if let Some(e) = &v.error {
            return Err(e.clone());
        }
        let total = v.log.len();
        for line in v.log.iter().skip(printed.min(total)) {
            println!("{line}");
        }
        printed = total;
    }
}

/// Run a short-lived agent, issue requests, stay up `secs` for acks.
fn oneshot(dir: &std::path::Path, reqs: impl FnOnce(&Fleet) -> Vec<Request>, secs: u64) -> Result<String, String> {
    let fleet = load(dir)?;
    if agent::agent_alive(dir) {
        // A long-running agent already holds this device's identity;
        // queue through it instead of racing it for the mailbox.
        return queue_via_file(dir, reqs(&fleet), secs);
    }
    let handle = agent::spawn(Ctx { sync_dir: dir.to_path_buf(), fleet: fleet.clone(), statusline_dir: None, long_lived: false })
        .map_err(|e| e.to_string())?;
    wait_running(&handle)?;
    for r in reqs(&fleet) {
        handle.request(r);
    }
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut printed = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        let v = handle.snapshot();
        for line in v.log.iter().skip(printed) {
            println!("{line}");
            if line.contains(" ✓ ") || line.contains(" ✗ ") {
                return Ok(String::new());
            }
        }
        printed = v.log.len();
    }
    Ok(String::new())
}

fn wait_running(handle: &agent::AgentHandle) -> Result<(), String> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        let v = handle.snapshot();
        if let Some(e) = v.error {
            return Err(e);
        }
        if v.running {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("timed out connecting to relays".into())
}

/// Hand requests to the running agent through the spool directory, then
/// follow its activity log until the outcome (an ack) shows up.
fn queue_via_file(dir: &std::path::Path, reqs: Vec<Request>, secs: u64) -> Result<String, String> {
    let spool = dir.join("spool");
    std::fs::create_dir_all(&spool).map_err(|e| e.to_string())?;
    let log = agent::activity_file(dir);
    let start = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    for r in reqs {
        let v = super::spool::encode(&r);
        let id = protocol::new_id();
        let tmp = spool.join(format!(".{id}"));
        std::fs::write(&tmp, v.to_string()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, spool.join(format!("{id}.json"))).map_err(|e| e.to_string())?;
    }
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut shown = 0usize;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let Ok(bytes) = std::fs::read(&log) else { continue };
        let new = String::from_utf8_lossy(bytes.get(start as usize..).unwrap_or_default()).into_owned();
        for line in new.lines().skip(shown) {
            println!("{line}");
            if line.contains(" ✓ ") || line.contains(" ✗ ") {
                return Ok(String::new());
            }
        }
        shown = new.lines().count();
    }
    Ok("handed to the running sync agent; no answer yet (see `nerve sync status`)".into())
}

fn status(dir: &std::path::Path, json: bool) -> Result<String, String> {
    let fleet = load(dir)?;
    let keys = fleet::device_keys(dir).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let peers = fleet.peer_keys();
    let states = rt.block_on(async {
        let t = Transport::connect(keys, &fleet.relays).await?;
        let s = t.fetch_states(&peers.iter().map(|(_, k)| *k).collect::<Vec<_>>()).await;
        let h = t.health().await;
        t.disconnect().await;
        Ok::<_, String>((s, h))
    })?;
    let (states, health) = states;
    if json {
        let out: Vec<Value> = states
            .iter()
            .filter_map(|s| match &s.envelope.body {
                Body::State(d) => serde_json::to_value(d).ok(),
                _ => None,
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&out).unwrap_or_default());
    }
    let now = protocol::now_secs();
    let mut out = format!("this device: {}{}\n", fleet.device, if agent::agent_alive(dir) { " (agent running)" } else { "" });
    for h in health {
        out.push_str(&format!("  relay {} {}\n", h.url, if h.connected { "connected" } else { "DOWN" }));
    }
    for (name, pk) in &peers {
        let st = states.iter().find(|s| s.sender == *pk);
        match st.map(|s| &s.envelope.body) {
            Some(Body::State(d)) => {
                out.push_str(&format!(
                    "\n{name}: last synced {} ({}){}\n",
                    crate::util::text::format_age(now.saturating_sub(d.generated_at) as f64),
                    agent::clock(d.generated_at),
                    if d.away { " · away" } else { "" }
                ));
                out.push_str(&format!("  {} {} · caps {}\n", d.os, d.arch, d.caps.join(",")));
                for s in &d.sessions {
                    out.push_str(&format!(
                        "  [{}] {} — {}{} · {}{}\n",
                        &s.id[..8.min(s.id.len())],
                        Sanitised::new(s.name.clone()),
                        Sanitised::new(s.state.clone()),
                        s.waiting_for.as_ref().map(|w| format!(" ({})", Sanitised::new(w.clone()))).unwrap_or_default(),
                        Sanitised::new(s.cwd.clone()),
                        if s.subagents > 0 { format!(" · {} subagents", s.subagents) } else { String::new() }
                    ));
                }
                for r in &d.requests {
                    out.push_str(&format!(
                        "  ? {} [{}] {}: {}{}\n",
                        r.id,
                        r.kind,
                        Sanitised::new(r.tool.clone()),
                        Sanitised::new(r.summary.clone()),
                        r.questions
                            .first()
                            .map(|q| format!("  options: {}", q.options.join(" | ")))
                            .unwrap_or_default()
                    ));
                }
                for j in &d.jobs {
                    out.push_str(&format!("  job {} '{}': {}\n", j.id, Sanitised::new(j.title.clone()), j.phase));
                }
            }
            _ => out.push_str(&format!("\n{name}: no state yet (never synced, or expired)\n")),
        }
    }
    Ok(out)
}

fn fetch(dir: &std::path::Path, device: &str, session: &str, what: FetchWhat) -> Result<String, String> {
    let fleet = load(dir)?;
    let handle = agent::spawn(Ctx { sync_dir: dir.to_path_buf(), fleet, statusline_dir: None, long_lived: false }).map_err(|e| e.to_string())?;
    wait_running(&handle)?;
    handle.request(Request::To { device: device.into(), body: Body::Fetch { session: session.into(), what } });
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(250));
        let v = handle.snapshot();
        if let Some((title, text)) = v.blobs.back() {
            return Ok(format!("=== {title}\n{text}"));
        }
        if let Some(l) = v.log.iter().find(|l| l.contains(" ✗ ")) {
            return Err(l.clone());
        }
    }
    Err("no reply within 60s (is the agent running on that device?)".into())
}

fn handoff(paths: &Paths, dir: &std::path::Path, session: &str, device: &str, rest: &[&str]) -> Result<String, String> {
    let continue_with = match rest {
        ["--then", text @ ..] => Some(text.join(" ")),
        [] => None,
        _ => return Err("usage: handoff <session> <device> [--then <text>]".into()),
    };
    let snaps = crate::detect::claude::discover_sessions(Some(paths.statusline_dir()));
    let snap = snaps
        .iter()
        .find(|s| s.id.as_str().starts_with(session))
        .ok_or_else(|| format!("no running session matches {session}; see `nerve --list`"))?;
    let (sid, cwd) = (snap.id.as_str().to_string(), snap.cwd.clone());
    let device = device.to_string();
    oneshot(dir, move |_| vec![Request::Handoff { session_id: sid, cwd, device, continue_with }], 120)
}

fn job(dir: &std::path::Path, rest: &[&str]) -> Result<String, String> {
    let split = rest.iter().position(|a| *a == "--").ok_or("job needs `-- <prompt>`")?;
    let (flags, prompt) = (&rest[..split], rest[split + 1..].join(" "));
    let mut spec = JobSpec { permission_mode: "acceptEdits".into(), ..Default::default() };
    let mut device = None;
    let mut title = Vec::new();
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        let mut val = || it.next().map(|s| s.to_string()).ok_or(format!("{f} needs a value"));
        match *f {
            "--on" => device = Some(val()?),
            "--needs" => spec.requires = val()?.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
            "--verify" => spec.verify = Some(val()?),
            "--cwd" => spec.cwd = Some(val()?),
            "--repo" => spec.repo = Some(val()?),
            "--model" => spec.model = Some(val()?),
            "--allow" => spec.allowed_tools.push(val()?),
            "--mode" => spec.permission_mode = val()?,
            other => title.push(other.to_string()),
        }
    }
    if prompt.trim().is_empty() {
        return Err("empty prompt".into());
    }
    spec.title = if title.is_empty() { prompt.chars().take(40).collect() } else { title.join(" ") };
    spec.prompt = prompt;
    oneshot(dir, move |_| vec![Request::Job { spec, device }], 20)
}
