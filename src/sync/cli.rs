//! `nerve sync ...` subcommands. Usage is in `USAGE`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::agent::{self, Ctx, Request};
use super::fleet::{self, Fleet};
use super::mailbox::{self, Mailbox};
use super::protocol::{self, Answer, Body, FetchWhat, JobSpec, RefMap, RunSpec, XferOffer, XferWhat};
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
  handoff <session> <device> [--to <place>] [--force] [--then <text>]
                                     continue a session elsewhere
  job [--on D] [--needs cap,cap] [--allow RULE]... [--verify CMD] [--cwd DIR] [--repo URL] [--model M] <title> -- <prompt>
  run <device> [--place P | --cwd DIR] [--timeout S] [--wait] -- <cmd> [args...]
                                     run an allowlisted command; --wait exits with its code
  put <device> <place> <path>... [--delete] [--wait]
                                     send files/dirs (relative to the place) into the peer's place
  get <device> <place> <path>... [--delete] [--wait]
                                     the same, from the peer into this device's place
  refs <device> <place> [--pull] [--wait] <src-ref>:<dst-ref>...
                                     send git refs into exactly <dst-ref> (never checks out)
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
        ["run", device, rest @ ..] => run_cmd(&dir, device, rest),
        ["put", device, place, rest @ ..] => xfer_files(&dir, device, place, rest, false),
        ["get", device, place, rest @ ..] => xfer_files(&dir, device, place, rest, true),
        ["refs", device, place, rest @ ..] => xfer_refs(&dir, device, place, rest),
        ["away", "on"] => oneshot(&dir, |_| vec![Request::SetAway(true)], 2),
        ["away", "off"] => oneshot(&dir, |_| vec![Request::SetAway(false)], 2),
        ["hook"] => hook(&dir),
        _ => return usage(),
    };
    match result {
        Ok(msg) => {
            if let Some(code) = msg.strip_prefix(EXIT_PREFIX) {
                std::process::exit(code.parse().unwrap_or(1));
            }
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

/// A command result that is an exit code to leave with, not a message.
const EXIT_PREFIX: &str = "\0exit:";
/// `run --wait` exit code when the command never ran (refused, unknown
/// device, no answer), as `docker run` and `env` use it.
const NOT_RUN: i32 = 125;

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
        // The library's default of 60 events a minute caps a transfer at
        // ~2 MB a minute (32 KiB chunks); this relay only serves the fleet.
        let relay = LocalRelay::builder()
            .rate_limit(RateLimit { max_reqs: 500, notes_per_minute: 6_000 })
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

/// What following the activity log should stop at.
enum Until {
    /// The first outcome line of any request (legacy commands).
    Any,
    /// The first outcome line for this id.
    Id(String),
    /// The end of run `id`: its `run finished` line, or a refusal.
    RunEnd(String),
}

/// How a followed request ended.
struct Ended {
    ok: bool,
    /// For runs: the remote exit code.
    code: Option<i32>,
}

impl Until {
    /// Print what matters about `line`; Some once it is the end.
    fn feed(&self, line: &str) -> Option<Ended> {
        match self {
            Self::Any => {
                println!("{line}");
                agent::outcome(line).map(|(_, ok, _)| Ended { ok, code: None })
            }
            Self::Id(id) => {
                let o = agent::outcome(line).filter(|(i, _, _)| i == id);
                if o.is_some() || line.contains(id.as_str()) {
                    eprintln!("{line}");
                }
                o.map(|(_, ok, _)| Ended { ok, code: None })
            }
            Self::RunEnd(id) => {
                // Command output goes to stdout as plain lines, so a
                // waited-on run pipes like a local one.
                let mark = format!(" \u{2502} {id} ");
                if let Some(at) = line.find(&mark) {
                    println!("{}", &line[at + mark.len()..]);
                    return None;
                }
                let (_, ok, msg) = agent::outcome(line).filter(|(i, _, _)| i == id)?;
                eprintln!("{line}");
                if let Some(code) = msg.strip_prefix("run finished: exit ") {
                    let code = code.split_whitespace().next().and_then(|c| c.parse().ok()).unwrap_or(NOT_RUN);
                    return Some(Ended { ok, code: Some(code) });
                }
                // Accepted: keep following. Refused: it never ran.
                (!ok).then_some(Ended { ok, code: Some(NOT_RUN) })
            }
        }
    }
}

/// Run a short-lived agent, issue requests, stay up `secs` for acks.
fn oneshot(dir: &std::path::Path, reqs: impl FnOnce(&Fleet) -> Vec<Request>, secs: u64) -> Result<String, String> {
    match drive(dir, reqs, Until::Any, Duration::from_secs(secs), false)? {
        None if agent::agent_alive(dir) => Ok("handed to the running sync agent; no answer yet (see `nerve sync status`)".into()),
        _ => Ok(String::new()),
    }
}

/// Issue `reqs` through the running agent (spool) or a short-lived one,
/// then follow the activity until `until` says it ended or `max` passes.
/// With `detach` and a running agent, return as soon as it is queued.
fn drive(dir: &std::path::Path, reqs: impl FnOnce(&Fleet) -> Vec<Request>, until: Until, max: Duration, detach: bool) -> Result<Option<Ended>, String> {
    let fleet = load(dir)?;
    let deadline = Instant::now() + max;
    if agent::agent_alive(dir) {
        // A long-running agent already holds this device's identity;
        // queue through it instead of racing it for the mailbox.
        let log = agent::activity_file(dir);
        let mut at = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        queue_via_file(dir, reqs(&fleet))?;
        if detach {
            return Ok(None);
        }
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            let Ok(bytes) = std::fs::read(&log) else { continue };
            if (bytes.len() as u64) < at {
                at = 0; // rotated
            }
            let new = bytes.get(at as usize..).unwrap_or_default();
            // Only whole lines: the agent may be mid-write.
            let Some(end) = new.iter().rposition(|b| *b == b'\n') else { continue };
            at += end as u64 + 1;
            for line in String::from_utf8_lossy(&new[..end]).lines() {
                if let Some(e) = until.feed(line) {
                    return Ok(Some(e));
                }
            }
        }
        return Ok(None);
    }
    if detach {
        eprintln!("no sync agent runs here, so this command stays until the transfer ends");
    }
    let handle = agent::spawn(Ctx { sync_dir: dir.to_path_buf(), fleet: fleet.clone(), statusline_dir: None, long_lived: false })
        .map_err(|e| e.to_string())?;
    wait_running(&handle)?;
    for r in reqs(&fleet) {
        handle.request(r);
    }
    let mut printed = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        let v = handle.snapshot();
        // The view keeps the last 200 lines; count from its end.
        let skip = printed.min(v.log.len());
        for line in v.log.iter().skip(skip) {
            if let Some(e) = until.feed(line) {
                return Ok(Some(e));
            }
        }
        printed = v.log.len();
    }
    Ok(None)
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

/// Hand requests to the running agent through the spool directory.
fn queue_via_file(dir: &std::path::Path, reqs: Vec<Request>) -> Result<(), String> {
    let spool = dir.join("spool");
    std::fs::create_dir_all(&spool).map_err(|e| e.to_string())?;
    for r in reqs {
        let v = super::spool::encode(&r);
        let id = protocol::new_id();
        let tmp = spool.join(format!(".{id}"));
        std::fs::write(&tmp, v.to_string()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, spool.join(format!("{id}.json"))).map_err(|e| e.to_string())?;
    }
    Ok(())
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
                    let title = Sanitised::new(j.title.clone());
                    if j.kind == "run" {
                        let code = j.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default();
                        out.push_str(&format!("  run {} '{title}': {}{code} · {} line(s)\n", j.id, j.phase, j.lines));
                        if let Some(last) = j.tail.last() {
                            out.push_str(&format!("      {}\n", Sanitised::new(last.clone())));
                        }
                    } else {
                        out.push_str(&format!("  job {} '{title}': {}\n", j.id, j.phase));
                    }
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
    let (mut place, mut force, mut continue_with) = (None, false, None);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match *a {
            "--to" => place = Some(it.next().ok_or("--to needs a place name")?.to_string()),
            "--force" => force = true,
            "--then" => {
                continue_with = Some(it.by_ref().copied().collect::<Vec<_>>().join(" "));
            }
            _ => return Err("usage: handoff <session> <device> [--to <place>] [--force] [--then <text>]".into()),
        }
    }
    let snaps = crate::detect::claude::discover_sessions(Some(paths.statusline_dir()));
    let snap = snaps
        .iter()
        .find(|s| s.id.as_str().starts_with(session))
        .ok_or_else(|| format!("no running session matches {session}; see `nerve --list`"))?;
    let (sid, cwd) = (snap.id.as_str().to_string(), snap.cwd.clone());
    let device = device.to_string();
    oneshot(dir, move |_| vec![Request::Handoff { session_id: sid, cwd, device, continue_with, place, force }], 120)
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

/// `nerve sync run <device> [--place P | --cwd DIR] [--timeout S] [--wait] -- <argv>`
fn run_cmd(dir: &std::path::Path, device: &str, rest: &[&str]) -> Result<String, String> {
    let split = rest.iter().position(|a| *a == "--").ok_or("run needs `-- <command> [args...]`")?;
    let (flags, argv) = (&rest[..split], &rest[split + 1..]);
    if argv.is_empty() {
        return Err("run needs a command after `--`".into());
    }
    let mut spec = RunSpec { id: protocol::new_id(), argv: argv.iter().map(|s| s.to_string()).collect(), ..Default::default() };
    let mut wait = false;
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        let mut val = || it.next().map(|s| s.to_string()).ok_or(format!("{f} needs a value"));
        match *f {
            "--place" => spec.place = Some(val()?),
            "--cwd" => spec.cwd = Some(val()?),
            "--timeout" => spec.timeout_secs = Some(val()?.parse().map_err(|_| "--timeout takes seconds")?),
            "--wait" => wait = true,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if spec.place.is_some() && spec.cwd.is_some() {
        return Err("give --place or --cwd, not both".into());
    }
    let id = spec.id.clone();
    let device = device.to_string();
    let req = move |_: &Fleet| vec![Request::Run { spec, device }];
    if !wait {
        return match drive(dir, req, Until::Id(id.clone()), Duration::from_secs(30), false)? {
            Some(Ended { ok: true, .. }) => Ok(format!("run {id} started; follow it with `nerve sync status`, or use --wait")),
            Some(_) => Err(format!("run {id} was not started")),
            None => Err(format!("run {id}: no answer within 30s (is the agent running on that device?)")),
        };
    }
    let max = Duration::from_secs(rest_timeout(flags).map(|t| t + 330).unwrap_or(24 * 3600));
    match drive(dir, req, Until::RunEnd(id.clone()), max, false)? {
        Some(Ended { code: Some(c), .. }) => Ok(format!("{EXIT_PREFIX}{c}")),
        _ => {
            eprintln!("nerve sync: run {id}: no result before the wait ran out");
            Ok(format!("{EXIT_PREFIX}{NOT_RUN}"))
        }
    }
}

fn rest_timeout(flags: &[&str]) -> Option<u64> {
    flags.iter().position(|f| *f == "--timeout").and_then(|i| flags.get(i + 1)).and_then(|v| v.parse().ok())
}

/// How long a waited-on transfer may take before the CLI gives up.
const XFER_WAIT: Duration = Duration::from_secs(30 * 60);

fn follow_xfer(dir: &std::path::Path, id: String, req: Request, wait: bool) -> Result<String, String> {
    let until = Until::Id(id.clone());
    match drive(dir, move |_| vec![req], until, XFER_WAIT, !wait)? {
        Some(Ended { ok: true, .. }) => Ok(String::new()),
        Some(_) => Err(format!("transfer {id} failed")),
        None if !wait => Ok(format!("queued as {id} with the running agent; its outcome goes to {}", agent::activity_file(dir).display())),
        None => Err(format!("transfer {id}: no outcome within {} minutes", XFER_WAIT.as_secs() / 60)),
    }
}

/// `put|get <device> <place> <path>... [--delete] [--wait]`
fn xfer_files(dir: &std::path::Path, device: &str, place: &str, rest: &[&str], pull: bool) -> Result<String, String> {
    let (mut delete, mut wait, mut roots) = (false, false, Vec::new());
    for a in rest {
        match *a {
            "--delete" => delete = true,
            "--wait" => wait = true,
            f if f.starts_with("--") => return Err(format!("unknown flag {f}")),
            p => roots.push(super::places::norm_root(p).map(|_| p.to_string())?),
        }
    }
    if roots.is_empty() {
        return Err(format!("{} needs at least one path relative to the place (`.` for all of it)", if pull { "get" } else { "put" }));
    }
    let id = protocol::new_id();
    let offer = XferOffer { place: place.to_string(), what: XferWhat::Files { roots, delete }, for_req: None };
    follow_xfer(dir, id.clone(), Request::Xfer { id, device: device.to_string(), offer, pull }, wait)
}

/// `refs <device> <place> [--pull] [--wait] <src>:<dst>...`
fn xfer_refs(dir: &std::path::Path, device: &str, place: &str, rest: &[&str]) -> Result<String, String> {
    let (mut pull, mut wait, mut map) = (false, false, Vec::new());
    for a in rest {
        match *a {
            "--pull" => pull = true,
            "--wait" => wait = true,
            f if f.starts_with("--") => return Err(format!("unknown flag {f}")),
            pair => {
                let (src, dst) = pair.split_once(':').ok_or_else(|| format!("{pair}: give <src-ref>:<dst-ref>"))?;
                if src.is_empty() || !dst.starts_with("refs/") {
                    return Err(format!("{pair}: the destination must be a full ref, e.g. main:refs/remotes/mac/main"));
                }
                map.push(RefMap { src: src.to_string(), dst: dst.to_string() });
            }
        }
    }
    if map.is_empty() {
        return Err("refs needs at least one <src-ref>:<dst-ref>".into());
    }
    let id = protocol::new_id();
    let offer = XferOffer { place: place.to_string(), what: XferWhat::Refs { map, repo_id: String::new() }, for_req: None };
    follow_xfer(dir, id.clone(), Request::Xfer { id, device: device.to_string(), offer, pull }, wait)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_end_is_read_from_outcome_lines_not_output() {
        let u = Until::RunEnd("ab12".into());
        assert!(u.feed("10:00:00Z pi \u{2502} ab12 \u{2713} looks like an outcome").is_none());
        assert!(u.feed("10:00:00Z pi \u{2713} ab12: run accepted: node x in /h").is_none(), "accepted is not the end");
        let e = u.feed("10:00:01Z pi \u{2717} ab12: run finished: exit 3 (exit 3 after 0.1s)").unwrap();
        assert_eq!((e.ok, e.code), (false, Some(3)));
        let r = u.feed("10:00:00Z pi \u{2717} ab12: not allowed on pi: …").unwrap();
        assert_eq!(r.code, Some(NOT_RUN));
        assert!(u.feed("10:00:00Z pi \u{2717} other: run finished: exit 0 (x)").is_none(), "other ids are ignored");
    }

    #[test]
    fn transfers_end_on_their_own_id() {
        let u = Until::Id("x1".into());
        assert!(u.feed("10:00:00Z mac \u{2713} y2: something else").is_none());
        assert!(u.feed("10:00:00Z pi \u{2713} x1: pi place h: 1 file(s) written").unwrap().ok);
    }
}
