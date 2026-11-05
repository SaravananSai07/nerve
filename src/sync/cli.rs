//! `nerve sync ...` subcommands (usage is in `USAGE`), and the everyday
//! commands (`nerve devices`, `allow`, `sync <dir>`, ...) in `verb`, which
//! each do what a key in the TUI does.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::agent::{self, Ctx, FleetReq, FolderReq, Request};
use super::connect::{self, names};
use super::fleet::{self, set_up, Fleet};
use super::folders::{Others, When};
use super::ipc::{self, ask_agent, drive, oneshot, Ended, Until, NOT_RUN, STALL};
use super::protocol::{
    self, Answer, Approval, Body, Decision, DeviceState, FetchWhat, RefMap, RemoteSession, RunSpec, StartSpec,
    XferOffer, XferWhat,
};
use super::transport::Transport;
use super::versions::Versions;
use crate::lockfile::LockError;
use crate::paths::Paths;
use crate::util::sanitize::Sanitised;
use crate::util::text::format_age;

const USAGE: &str = "\
nerve sync — the lower-level sync commands (everyday ones: `nerve help`)

  <folder> | <dir> --with D,…        sync a folder now, or start syncing one
  doctor                             check this device's setup end to end

  init [--name N] [--relay URL]...   create this device's key + fleet.toml
  id                                 print this device's npub (give it to peers)
  pair <name> <npub>                 trust a peer device
  relay <add|rm> <url>               edit the relay list
  private [<dir>] | public [<dir>]   keep a folder's sessions on this device, or let
                                     peers see them again (default: here); bare
                                     `private` lists what is kept here
  install-hooks                      write Claude settings for remote answers
  serve-relay [--port 7777]          run a private Nostr relay (on an always-on device)
  agent                              run the sync agent headless (a server)
  status [--json]                    peers: last synced, sessions, pending
  sync                               republish now and ask peers to
  send <device> <session> <text>     type an instruction into a session
  answer <device> <request> allow|deny|<label>[,<label>...]
  fetch <device> <session> diff|log|subagents
  handoff <session> <device> [--to <place>] [--force] [--then <text>]
                                     continue a session elsewhere
  start <device> <dir> <prompt> [--wait] [--timeout S]
                                     start `claude <prompt>` in tmux in <dir> there (an absolute
                                     path, '~/…' or a place); prints the session id, then the
                                     tmux session; --wait also waits for its first turn to end
                                     and prints its reply (--timeout: give up after S, 3600)
  run <device> [--place P | --cwd DIR] [--timeout S] [--wait] -- <cmd> [args...]
                                     run an allowlisted command; --wait exits with its code
  put <device> <place> <path>... [--delete] [--wait]
                                     send files/dirs (relative to the place) into the peer's place
  get <device> <place> <path>... [--delete] [--wait]
                                     the same, from the peer into this device's place
  refs <device> <place> [--pull] [--wait] <src-ref>:<dst-ref>...
                                     send git refs into exactly <dst-ref> (never checks out)
                                     files at or over [xfer] bulk_over (64 MiB), or that the
                                     relay fails to carry, go over rsync on [ssh] <peer> = host
  place <name> --src|--dst <path>... (run over ssh by a peer's rsync) this place's path, checked
  away on|off                        route Claude's questions to peers
  hook                               (Claude hook entry point)
";

/// What a command leaves with.
enum Out {
    /// Printed, then exit 0.
    Msg(String),
    /// An exit code to leave with, printing nothing more (`run --wait`).
    Exit(i32),
}

pub(crate) fn run(paths: &Paths, args: &[String]) -> std::io::Result<()> {
    let dir = fleet::sync_dir(paths);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match a.as_slice() {
        ["run", device, rest @ ..] => run_cmd(&dir, device, rest),
        ["start", rest @ ..] => start(&dir, rest),
        a => command(paths, &dir, a).map(Out::Msg),
    };
    match result {
        Ok(Out::Exit(code)) => std::process::exit(code),
        Ok(Out::Msg(msg)) => {
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

/// Every subcommand but `run` and `start`, which print as they go.
fn command(paths: &Paths, dir: &Path, a: &[&str]) -> Result<String, String> {
    match a {
        ["doctor"] => doctor(paths, dir),
        ["init", rest @ ..] => init(dir, rest),
        ["id"] => id(dir),
        ["pair", name, key] => edit(dir, |f| {
            nostr_sdk::prelude::PublicKey::parse(key).map_err(|e| format!("bad key: {e}"))?;
            f.peers.insert(name.to_string(), key.to_string());
            Ok(format!("paired {name}"))
        }),
        ["relay", "add", url] => edit(dir, |f| {
            fleet::check_relay(url)?;
            if !f.relays.iter().any(|r| r == url) {
                f.relays.push(url.to_string());
            }
            Ok(format!("relays: {}", f.relays.join(", ")))
        }),
        ["relay", "rm", url] => edit(dir, |f| {
            f.relays.retain(|r| r != url);
            Ok(format!("relays: {}", f.relays.join(", ")))
        }),
        ["private"] => set_up(dir).map(|f| {
            let all: Vec<String> =
                f.private.iter().cloned().chain(f.private_sessions.iter().map(|id| format!("session {id}"))).collect();
            if all.is_empty() {
                "nothing is private: your devices see every session here".to_string()
            } else {
                all.join("\n")
            }
        }),

        [verb @ ("private" | "public"), path @ ..] if path.len() <= 1 => {
            let path = match path.first() {
                Some(p) => std::path::absolute(p).map_err(|e| e.to_string()),
                None => std::env::current_dir().map_err(|e| e.to_string()),
            };
            path.and_then(|p| edit(dir, |f| f.set_private(&super::fleet::PrivateTarget::Folder(p), *verb == "private")))
        }

        ["install-hooks"] => super::install::hooks(dir),
        ["serve-relay", rest @ ..] => super::transport::serve_relay(rest),
        ["agent"] => run_agent(paths, dir),
        ["status", rest @ ..] => status(dir, rest.contains(&"--json")),
        ["sync"] => oneshot(dir, Request::SyncNow, 2),
        ["send", device, session, text @ ..] if !text.is_empty() => {
            let body = Body::Send { session: session.to_string(), text: text.join(" ") };
            oneshot(dir, Request::To { device: device.to_string(), body }, 15)
        }
        ["answer", device, request, choice] => {
            let answer = match *choice {
                "allow" | "yes" => Answer::Allow,
                "deny" | "no" => Answer::Deny { reason: "denied from another device".into() },
                labels => Answer::Choose { labels: labels.split(',').map(String::from).collect() },
            };
            let body = Body::Answer { request: request.to_string(), answer };
            oneshot(dir, Request::To { device: device.to_string(), body }, 15)
        }
        ["fetch", device, session, what] => {
            let what = match *what {
                "diff" => FetchWhat::Diff,
                "log" => FetchWhat::Log,
                "subagents" => FetchWhat::Subagents,
                _ => usage(),
            };
            fetch(dir, device, session, what)
        }
        ["handoff", session, device, rest @ ..] => handoff(paths, dir, session, device, rest),
        ["put", device, place, rest @ ..] => xfer_files(dir, device, place, rest, false),
        ["get", device, place, rest @ ..] => xfer_files(dir, device, place, rest, true),
        ["refs", device, place, rest @ ..] => xfer_refs(dir, device, place, rest),
        ["place", name, role, roots @ ..] if !roots.is_empty() && (*role == "--src" || *role == "--dst") => {
            set_up(dir).and_then(|f| f.place(name, &Fleet::file(dir))).and_then(|base| {
                let roots: Vec<String> = roots.iter().map(|r| r.to_string()).collect();
                let info = super::places::place_info(&base, *role == "--src", &roots)?;
                serde_json::to_string(&info).map_err(|e| e.to_string())
            })
        }
        [folder, rest @ ..] if !SUBCOMMANDS.contains(folder) => sync(dir, folder, rest),
        ["away", "on"] => oneshot(dir, Request::SetAway(true), 2),
        ["away", "off"] => oneshot(dir, Request::SetAway(false), 2),
        ["hook"] => super::mailbox::hook(dir),
        _ => usage(),
    }
}

/// `nerve sync <word>`: these are commands; any other word is a folder.
const SUBCOMMANDS: &[&str] = &[
    "doctor",
    "init",
    "id",
    "pair",
    "relay",
    "private",
    "public",
    "install-hooks",
    "serve-relay",
    "agent",
    "status",
    "sync",
    "send",
    "answer",
    "fetch",
    "handoff",
    "start",
    "run",
    "put",
    "get",
    "refs",
    "place",
    "away",
    "hook",
];

fn usage() -> ! {
    eprint!("{USAGE}");
    std::process::exit(2);
}

fn edit(dir: &std::path::Path, f: impl FnOnce(&mut Fleet) -> Result<String, String>) -> Result<String, String> {
    let mut fleet = set_up(dir)?;
    let msg = f(&mut fleet)?;
    fleet.save(dir).map_err(|e| e.to_string())?;
    Ok(msg)
}

/// Each peer with its last published state (None: none on the relays).
pub(super) type PeerStates = Vec<(String, Option<protocol::DeviceState>)>;

fn peer_states(dir: &std::path::Path) -> Result<(Fleet, PeerStates, Vec<super::transport::RelayHealth>), String> {
    let fleet = set_up(dir)?;
    let keys = fleet::device_keys(dir).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let peers = fleet.peer_keys();
    let (states, health) = rt.block_on(async {
        let t = Transport::connect(keys, &fleet.relays).await?;
        let s = t.fetch_states(&peers.iter().map(|(_, k)| *k).collect::<Vec<_>>()).await;
        let h = t.health().await;
        t.disconnect().await;
        Ok::<_, String>((s, h))
    })?;
    let out = peers
        .into_iter()
        .map(|(name, pk)| {
            let st = states.iter().find(|s| s.sender == pk).and_then(|s| match &s.envelope.body {
                Body::State(d) => Some((**d).clone()),
                _ => None,
            });
            (name, st)
        })
        .collect();
    Ok((fleet, out, health))
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

/// One line per check, with what to do about a failure.
fn doctor(paths: &Paths, dir: &std::path::Path) -> Result<String, String> {
    let mut out = String::new();
    let mut line =
        |ok: bool, what: String| out.push_str(&format!("  {} {what}\n", if ok { "\u{2713}" } else { "\u{2717}" }));
    let Some(fleet) = Fleet::load(dir) else {
        return Ok("  \u{2717} not set up: run `nerve connect`\n".into());
    };
    line(true, format!("device '{}', {} peer(s), {} relay(s)", fleet.device, fleet.peers.len(), fleet.relays.len()));
    let alive = agent::agent_alive(dir);
    line(
        alive,
        if alive {
            "sync agent running".into()
        } else {
            "sync agent not running: `nerve connect` installs it, or run `nerve sync agent`".into()
        },
    );
    let sock = super::modhub::socket_path(dir);
    line(sock.exists(), format!("mod socket {}", sock.display()));
    let mod_json = dir.join("mod/nerve/nerve.json");
    let mod_ok = std::fs::read_to_string(&mod_json).is_ok_and(|s| s.contains(&*sock.to_string_lossy()));
    line(
        mod_ok,
        if mod_ok {
            "nerve mod installed and pointed at this agent".into()
        } else {
            "nerve mod missing or stale: run `nerve connect`".into()
        },
    );
    let sessions = crate::detect::claude::discover_sessions(Some(paths.statusline_dir()));
    line(true, format!("{} Claude session(s) on this device", sessions.len()));
    let (_, states, health) = peer_states(dir)?;
    for h in &health {
        line(h.connected, format!("relay {} {}", h.url, if h.connected { "connected" } else { "unreachable" }));
    }
    let now = crate::util::now_secs();
    for (name, state) in &states {
        match state {
            Some(d) => line(
                now.saturating_sub(d.generated_at) < 180,
                format!(
                    "{name}: last synced {}",
                    crate::util::text::format_age(now.saturating_sub(d.generated_at) as f64)
                ),
            ),
            None => line(false, format!("{name}: no state seen (is its agent running? same relays?)")),
        }
    }
    Ok(out.trim_end().to_string())
}

fn id(dir: &std::path::Path) -> Result<String, String> {
    let keys = fleet::device_keys(dir).map_err(|e| e.to_string())?;
    Ok(fleet::npub(&keys.public_key()))
}

/// How often a waiting `nerve sync agent` tries for `agent.lock`.
const LOCK_POLL: Duration = Duration::from_secs(2);

fn run_agent(paths: &Paths, dir: &std::path::Path) -> Result<String, String> {
    set_up(dir)?;
    // A TUI may be this device's agent: wait for it to quit, then take over.
    let mut told = false;
    let lock = loop {
        match agent::take_lock(dir) {
            Ok(l) => break l,
            Err(LockError::AlreadyHeld { pid }) => {
                if !std::mem::replace(&mut told, true) {
                    let pid = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
                    println!("another nerve runs this device's sync agent{pid}; taking over when it stops");
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(LockError::Io(e)) => return Err(format!("agent.lock: {e}")),
        }
    };
    let handle = agent::spawn(Ctx {
        paths: paths.clone(),
        sync_dir: dir.to_path_buf(),
        // Read now: it may have changed while this waited.
        fleet: set_up(dir)?,
        statusline_dir: Some(paths.statusline_dir()),
        lock: Some(lock),
        notifier: Some(crate::notify::Notifier::new(crate::config::Config::load(paths).notifications.sound)),
    })
    .map_err(|e| e.to_string())?;
    super::install::refresh_mod(dir);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let built = exe_id(&exe);
    let mut seen = 0;
    let mut checked = std::time::Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        // A relay that is down is retried by the agent, which notes each try.
        let v = handle.snapshot();
        for line in v.lines_since(seen) {
            println!("{line}");
        }
        seen = v.seq;
        // An upgrade replaced the binary: carry on as the new one once no
        // transfer is mid-way, so a service never runs a stale nerve.
        if checked.elapsed() >= Duration::from_secs(10) && v.transfers.is_empty() {
            checked = std::time::Instant::now();
            if exe_id(&exe) != built && runs(&exe) {
                println!("nerve was updated; restarting the agent");
                let e =
                    std::os::unix::process::CommandExt::exec(std::process::Command::new(&exe).args(["sync", "agent"]));
                return Err(format!("couldn't restart as the updated nerve: {e}"));
            }
        }
    }
}

/// The binary's inode and mtime: installers replace it by rename.
fn exe_id(exe: &std::path::Path) -> Option<(u64, std::time::SystemTime)> {
    let m = std::fs::metadata(exe).ok()?;
    Some((std::os::unix::fs::MetadataExt::ino(&m), m.modified().ok()?))
}

/// A half-copied or unsigned binary fails this instead of the agent.
fn runs(exe: &std::path::Path) -> bool {
    std::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

fn status(dir: &std::path::Path, json: bool) -> Result<String, String> {
    let (fleet, states, health) = peer_states(dir)?;
    if json {
        let out: Vec<Value> = states.iter().filter_map(|(_, d)| serde_json::to_value(d.as_ref()?).ok()).collect();
        return Ok(serde_json::to_string_pretty(&out).unwrap_or_default());
    }
    let now = crate::util::now_secs();
    let mut out =
        format!("this device: {}{}\n", fleet.device, if agent::agent_alive(dir) { " (agent running)" } else { "" });
    for h in health {
        out.push_str(&format!("  relay {} {}\n", h.url, if h.connected { "connected" } else { "DOWN" }));
    }
    for (name, st) in &states {
        match st {
            Some(d) => {
                out.push_str(&format!(
                    "\n{name}: last synced {} ({}){}\n",
                    crate::util::text::format_age(now.saturating_sub(d.generated_at) as f64),
                    ipc::clock(d.generated_at),
                    if d.away { " · away" } else { "" }
                ));
                out.push_str(&format!(
                    "  {} {} · caps {}\n",
                    Sanitised::new(d.os.clone()),
                    Sanitised::new(d.arch.clone()),
                    Sanitised::new(d.caps.join(","))
                ));
                for s in &d.sessions {
                    out.push_str(&format!(
                        "  [{}] {} — {}{} · {}{}\n",
                        Sanitised::new(protocol::short_id(&s.id, 8)),
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
                        Sanitised::new(r.id.clone()),
                        Sanitised::new(r.kind.clone()),
                        Sanitised::new(r.tool.clone()),
                        Sanitised::new(r.summary.clone()),
                        r.questions
                            .first()
                            .map(|q| format!("  options: {}", Sanitised::new(q.options.join(" | "))))
                            .unwrap_or_default()
                    ));
                }
                for r in &d.runs {
                    let title = Sanitised::new(r.title.clone());
                    let (id, phase) = (Sanitised::new(r.id.clone()), Sanitised::new(r.phase.clone()));
                    let code = r.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default();
                    out.push_str(&format!("  run {id} '{title}': {phase}{code} · {} line(s)\n", r.lines));
                    if let Some(last) = r.tail.last() {
                        out.push_str(&format!("      {}\n", Sanitised::new(last.clone())));
                    }
                }
            }
            None => out.push_str(&format!("\n{name}: no state yet (never synced, or expired)\n")),
        }
    }
    Ok(out)
}

/// Its own short-lived agent, since the text comes back to the agent
/// that asked.
fn fetch(dir: &std::path::Path, device: &str, session: &str, what: FetchWhat) -> Result<String, String> {
    let handle = ipc::own_agent(dir)?;
    let id = handle.request(Request::To { device: device.into(), body: Body::Fetch { session: session.into(), what } });
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(250));
        if let Some((title, text)) = handle.last_blob() {
            return Ok(format!("=== {title}\n{text}"));
        }
        let v = handle.snapshot();
        if let Some(l) = v.log.iter().find(|l| ipc::failed(l, &id)) {
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
    oneshot(dir, Request::Handoff { session_id: sid, cwd, device, continue_with, place, force }, 120)
}

/// `start <device> <dir> <prompt…> [--wait] [--timeout S]`, parsed.
#[derive(Debug, PartialEq)]
struct StartArgs {
    device: String,
    dir: String,
    prompt: String,
    wait: Option<Duration>,
}

/// How long `start --wait` waits for the first turn unless told.
const TURN_WAIT: Duration = Duration::from_secs(3600);
/// How long the target has to say it started the session.
const START_WAIT: Duration = Duration::from_secs(60);

/// The prompt is every other word, joined by spaces as `send` joins
/// them; words after `--` are prompt even when they look like flags.
fn parse_start(rest: &[&str]) -> Result<StartArgs, String> {
    let [device, dir, rest @ ..] = rest else {
        return Err("usage: start <device> <dir> <prompt> [--wait] [--timeout S]".into());
    };
    let (mut wait, mut timeout, mut words) = (false, None, Vec::new());
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match *a {
            "--" => words.extend(it.by_ref().copied()),
            "--wait" => wait = true,
            "--timeout" => {
                timeout = Some(it.next().and_then(|t| t.parse().ok()).ok_or("--timeout takes seconds")?);
            }
            f if f.starts_with("--") => {
                return Err(format!("unknown flag {f}; a prompt that starts with -- goes after `--`"))
            }
            w => words.push(w),
        }
    }
    if timeout.is_some() && !wait {
        return Err("--timeout goes with --wait".into());
    }
    let prompt = words.join(" ");
    if prompt.trim().is_empty() {
        return Err("start needs a prompt".into());
    }
    Ok(StartArgs {
        device: device.to_string(),
        dir: dir.to_string(),
        prompt,
        wait: wait.then(|| timeout.map_or(TURN_WAIT, Duration::from_secs)),
    })
}

/// `nerve sync start`: the session id, then the tmux session, as soon as
/// the device has started it; with `--wait`, then its first reply.
fn start(dir: &Path, rest: &[&str]) -> Result<Out, String> {
    let a = parse_start(rest)?;
    let session = protocol::new_session_id();
    // A path under this home means the same path under the device's: the
    // shell has already expanded an unquoted `~`.
    let spec = StartSpec { session: session.clone(), dir: crate::paths::tilde(&a.dir), prompt: a.prompt };
    let id = protocol::new_id();
    let req = Request::To { device: a.device.clone(), body: Body::Start(spec) };
    match drive(dir, req, Until::Quiet(id), START_WAIT, false)? {
        Some(Ended { ok: true, .. }) => {}
        Some(Ended { msg, .. }) => return Err(msg),
        None => {
            return Err(format!(
                "no answer from {} within {}s (is its agent running, on a nerve with `sync start`?)",
                a.device,
                START_WAIT.as_secs()
            ))
        }
    }
    println!("{session}\n{}", super::handoff::tmux_name(&session));
    match a.wait {
        None => Ok(Out::Msg(String::new())),
        Some(max) => wait_turn(dir, &a.device, &session, max).map(Out::Msg),
    }
}

/// What `start --wait` has seen of the new session in its device's state.
#[derive(Debug, Default)]
struct Turn {
    seen: bool,
    busy: bool,
    asked: bool,
}

#[derive(Debug, PartialEq)]
enum Step {
    Wait,
    /// It waits for a person: say so once, and keep waiting.
    Asking(String),
    Done,
    Failed(String),
}

impl Turn {
    /// The turn is over once the session is idle after working, or idle
    /// with a reply after its prompt (a turn quicker than a state).
    fn see(&mut self, s: Option<&RemoteSession>) -> Step {
        let Some(s) = s else {
            return if self.seen { Step::Failed("the session ended before its turn did".into()) } else { Step::Wait };
        };
        self.seen = true;
        let why = || crate::util::sanitize::clean(s.waiting_for.clone().unwrap_or_else(|| s.state.clone()));
        match s.state.as_str() {
            "Processing" => self.busy = true,
            t if t.starts_with("Tool") => self.busy = true,
            "Idle" if self.busy || replied(&s.tail) => return Step::Done,
            "Error" | "Gone" => return Step::Failed(format!("the session stopped: {}", why())),
            "Waiting" if !std::mem::replace(&mut self.asked, true) => return Step::Asking(why()),
            _ => {}
        }
        Step::Wait
    }
}

/// A reply after the last prompt, in a transcript tail as states carry it.
fn replied(tail: &[String]) -> bool {
    let last = |mark: &str| tail.iter().rposition(|l| l.starts_with(mark));
    match (last("● "), last("› ")) {
        (Some(reply), Some(prompt)) => reply > prompt,
        (reply, _) => reply.is_some(),
    }
}

/// Follow session `sid` on `device` through the states it publishes, as
/// the TUI does, until its first turn ends; then its last reply.
fn wait_turn(dir: &Path, device: &str, sid: &str, max: Duration) -> Result<String, String> {
    let handle = ipc::own_agent(dir)?;
    let deadline = Instant::now() + max;
    let mut turn = Turn::default();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        let v = handle.snapshot();
        let state = if v.me == device {
            v.local.as_ref()
        } else {
            v.peers.iter().find(|p| p.name == device).and_then(|p| p.state.as_ref())
        };
        let s = state.and_then(|st| st.sessions.iter().find(|s| s.id == sid));
        match turn.see(s) {
            Step::Wait => {}
            Step::Asking(why) => eprintln!("{sid} waits for an answer on {device}: {why}"),
            Step::Failed(e) => return Err(e),
            Step::Done => {
                let tail = s.map(|s| s.tail.clone()).unwrap_or_default();
                return Ok(last_reply(&handle, device, sid, &tail));
            }
        }
    }
    Err(format!("{sid}: its turn didn't end within {}s", max.as_secs()))
}

/// The session's last reply in full, fetched from its device; the tail's
/// last reply line when that doesn't answer.
fn last_reply(handle: &agent::AgentHandle, device: &str, sid: &str, tail: &[String]) -> String {
    let body = Body::Fetch { session: sid.to_string(), what: FetchWhat::Reply };
    let id = handle.request(Request::To { device: device.to_string(), body });
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(250));
        if let Some((_, text)) = handle.last_blob().filter(|(title, _)| title.starts_with("reply:")) {
            return text;
        }
        if handle.snapshot().log.iter().any(|l| ipc::failed(l, &id)) {
            break;
        }
    }
    tail.iter().rev().find_map(|l| l.strip_prefix("● ")).map(crate::util::sanitize::clean).unwrap_or_default()
}

/// `nerve sync run <device> [--place P | --cwd DIR] [--timeout S] [--wait] -- <argv>`
fn run_cmd(dir: &std::path::Path, device: &str, rest: &[&str]) -> Result<Out, String> {
    let split = rest.iter().position(|a| *a == "--").ok_or("run needs `-- <command> [args...]`")?;
    let (flags, argv) = (&rest[..split], &rest[split + 1..]);
    if argv.is_empty() {
        return Err("run needs a command after `--`".into());
    }
    let mut spec = RunSpec { argv: argv.iter().map(|s| s.to_string()).collect(), ..Default::default() };
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
    let (id, timeout) = (protocol::new_id(), spec.timeout_secs);
    let req = Request::Run { spec, device: device.to_string() };
    if !wait {
        return match drive(dir, req, Until::Id(id.clone()), Duration::from_secs(30), false)? {
            Some(Ended { ok: true, .. }) => {
                Ok(Out::Msg(format!("run {id} started; follow it with `nerve sync status`, or use --wait")))
            }
            Some(_) => Err(format!("run {id} was not started")),
            None => Err(format!("run {id}: no answer within 30s (is the agent running on that device?)")),
        };
    }
    let max = Duration::from_secs(timeout.map(|t| t + 330).unwrap_or(24 * 3600));
    match drive(dir, req, Until::RunEnd(id.clone()), max, false)? {
        Some(Ended { code: Some(c), .. }) => Ok(Out::Exit(c)),
        _ => {
            eprintln!("nerve sync: run {id}: no result before the wait ran out");
            Ok(Out::Exit(NOT_RUN))
        }
    }
}

/// How long a waited-on transfer may take before the CLI gives up.
const XFER_WAIT: Duration = Duration::from_secs(30 * 60);

/// Follow transfer `id` to its end. `rsync` finishes it from this side
/// when the relay failed it or went quiet, where this command is the one
/// waiting (no agent here, or `--wait`).
fn follow_xfer(
    dir: &std::path::Path,
    id: String,
    req: Request,
    wait: bool,
    rsync: Option<&dyn Fn() -> Result<String, String>>,
) -> Result<String, String> {
    let rsync = rsync.filter(|_| wait || !agent::agent_alive(dir));
    let ended = drive(dir, req, Until::Id(id.clone()), XFER_WAIT, !wait)?;
    if let (Some(go), true) = (rsync, matches!(ended, Some(Ended { relay: true, .. }) | None)) {
        eprintln!("transfer {id}: the relay did not carry it; copying over rsync instead");
        return go();
    }
    match ended {
        Some(Ended { ok: true, .. }) => Ok(String::new()),
        Some(_) => Err(format!("transfer {id} failed")),
        None if !wait => Ok(format!(
            "queued as {id} with the running agent; its outcome goes to {}",
            ipc::activity_file(dir).display()
        )),
        None => Err(format!("transfer {id}: no outcome (nothing heard for {} minutes)", STALL.as_secs() / 60)),
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
        return Err(format!(
            "{} needs at least one path relative to the place (`.` for all of it)",
            if pull { "get" } else { "put" }
        ));
    }
    let fleet = set_up(dir)?;
    let host = fleet.ssh.get(device).cloned();
    let id = protocol::new_id();
    // No agent here and a route: a big put needs nothing from the relay.
    if let (Some(h), false, false) = (&host, pull, agent::agent_alive(dir)) {
        let base = fleet.place(place, &Fleet::file(dir))?;
        if super::places::scan(&base, &roots)?.bytes >= fleet.xfer.bulk_over {
            return direct_rsync(&fleet, dir, h, device, place, &roots, delete, pull, &id);
        }
    }
    let offer = XferOffer {
        place: place.to_string(),
        what: XferWhat::Files { roots: roots.clone(), delete },
        ..Default::default()
    };
    let req = Request::Xfer { device: device.to_string(), offer, pull };
    let go =
        || direct_rsync(&fleet, dir, host.as_deref().unwrap_or_default(), device, place, &roots, delete, pull, &id);
    follow_xfer(dir, id.clone(), req, wait, host.is_some().then_some(&go as &dyn Fn() -> Result<String, String>))
}

/// The CLI's own rsync over `host`, for when the agent or the relay
/// can't coordinate: the far side's nerve, reached over the same ssh,
/// gives its path of the place and checks the roots by its own rules.
#[allow(clippy::too_many_arguments)]
fn direct_rsync(
    fleet: &Fleet,
    dir: &std::path::Path,
    host: &str,
    device: &str,
    place: &str,
    roots: &[String],
    delete: bool,
    pull: bool,
    id: &str,
) -> Result<String, String> {
    use super::places::{self, End};
    let base = fleet.place(place, &Fleet::file(dir))?;
    let me = fleet.device.clone();
    let far = places::remote_place(host, place, pull, roots)?;
    let remote = End::Remote { host: host.to_string(), path: far.path.into() };
    let (src, dst, scan, into) = if pull {
        let scan = far.scan.ok_or_else(|| format!("{device} did not say what {} holds", roots.join(" ")))?;
        (remote, End::Local(base), scan, me.clone())
    } else {
        let scan = places::scan(&base, roots)?;
        (End::Local(base), remote, scan, device.to_string())
    };
    let line = |l: String| eprintln!("{} {me} [{id}] {l}", ipc::clock(crate::util::now_secs()));
    let msg = places::copy(&src, &dst, roots, &scan, delete, &line)?;
    let done = ipc::outcome_line(&me, ipc::Outcome::Ok, id, &format!("{into} place {place}: {msg}"));
    eprintln!("{} {done}", ipc::clock(crate::util::now_secs()));
    Ok(String::new())
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
                    return Err(format!(
                        "{pair}: the destination must be a full ref, e.g. main:refs/remotes/laptop/main"
                    ));
                }
                map.push(RefMap { src: src.to_string(), dst: dst.to_string() });
            }
        }
    }
    if map.is_empty() {
        return Err("refs needs at least one <src-ref>:<dst-ref>".into());
    }
    let id = protocol::new_id();
    let offer = XferOffer {
        place: place.to_string(),
        what: XferWhat::Refs { map, repo_id: String::new() },
        ..Default::default()
    };
    follow_xfer(dir, id, Request::Xfer { device: device.to_string(), offer, pull }, wait, None)
}

// ---- everyday commands: each does what a key in the TUI does ----

/// Help for the everyday commands, appended to `nerve --help`.
pub(crate) const HELP: &str = "\
Devices
  nerve connect [--with all|now|D,…]   show a code for a new device; `all` (default)
                                       also lets it meet devices you add later
  nerve connect <code> [--with D,…]    join the devices that code offers
  nerve devices                        your devices, and which are online
  nerve link <device> all|D,…          connect a device to more of your devices
  nerve forget <device>                drop a device from all your devices

Requests (new devices, new folders)
  nerve requests                       what waits for a yes, on any device
  nerve allow <id> [--at DIR] [--others ask|inside|any]
  nerve refuse <id>

Sessions
  nerve sync start <device> <dir> <prompt> [--wait] [--timeout S]
                                       start Claude in a folder on a device ('~/…', a path
                                       or a place); prints the session id, then its tmux
                                       session; --wait also prints its first reply

Folders
  nerve sync <dir> --with D,… [--when turn|watch|manual] [--others ask|inside|any]
                                       keep a folder the same on those devices
  nerve sync <folder>                  sync it now
  nerve unsync <folder>                stop syncing it on this device
  nerve folders                        each folder and how it stands
  nerve versions [<folder>]            files a sync replaced, kept for a while
  nerve restore <id>                   put one back
";

/// `nerve <verb> …`, when the verb is one of these.
pub(crate) fn verb(paths: &Paths, verb: &str, args: &[String]) -> Option<std::io::Result<()>> {
    let dir = fleet::sync_dir(paths);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let fleet_req = |r: FleetReq| ask_agent(&dir, Request::Fleet(r));
    let res = match (verb, a.as_slice()) {
        ("connect" | "setup", rest) => connect::run(paths, rest),
        ("devices", []) => devices(&dir),
        ("forget", [device]) => fleet_req(FleetReq::Forget { device: device.to_string() }),
        ("link", [device, with]) => fleet_req(FleetReq::Link {
            device: device.to_string(),
            with: if *with == "all" { Vec::new() } else { names(with) },
        }),
        ("requests", []) => requests(&dir),
        ("allow", [id, rest @ ..]) => decide(&dir, id, true, rest),
        ("refuse", [id]) => decide(&dir, id, false, &[]),
        ("unsync", [folder]) => {
            folder_name(&dir, folder).and_then(|name| ask_agent(&dir, Request::Folder(FolderReq::Remove { name })))
        }
        ("folders", []) => folders(&dir),
        ("versions", rest) if rest.len() <= 1 => versions(&dir, rest.first().copied()),
        ("restore", [id]) => fleet_req(FleetReq::Restore { id: id.to_string() }),
        (
            "devices" | "forget" | "link" | "requests" | "allow" | "refuse" | "unsync" | "folders" | "versions"
            | "restore",
            _,
        ) => Err(format!("usage:\n{HELP}")),
        _ => return None,
    };
    Some(match res {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("nerve: {e}");
            std::process::exit(1);
        }
    })
}

fn local_state(dir: &Path) -> Option<DeviceState> {
    serde_json::from_str(&std::fs::read_to_string(super::agent::local_file(dir)).ok()?).ok()
}

fn ago(at: u64) -> String {
    format_age(crate::util::now_secs().saturating_sub(at) as f64)
}

/// `nerve sync <folder>` (now) or `nerve sync <dir> --with …` (start).
pub(crate) fn sync(dir: &Path, word: &str, rest: &[&str]) -> Result<String, String> {
    let (mut with, mut when, mut others) = (Vec::new(), When::Turn, Others::Ask);
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let mut val = || it.next().copied().ok_or(format!("{arg} needs a value"));
        match *arg {
            "--with" => with.extend(names(val()?)),
            "--when" => when = When::parse(val()?).ok_or("--when: use turn, watch or manual")?,
            "--others" => others = Others::parse(val()?).ok_or("--others: use ask, inside or any")?,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if with.is_empty() {
        let name = folder_name(dir, word)
            .map_err(|_| format!("{word} isn't synced yet; start it with `nerve sync {word} --with <devices>`"))?;
        return ask_agent(dir, Request::Folder(FolderReq::Now { name }));
    }
    let path = std::fs::canonicalize(word).map_err(|e| format!("{word}: {e}"))?;
    ask_agent(dir, Request::Folder(FolderReq::Add { path, with, when, others, yes: false }))
}

/// A synced folder by its name, or by its path here.
fn folder_name(dir: &Path, word: &str) -> Result<String, String> {
    let fleet = set_up(dir)?;
    let path = std::fs::canonicalize(word).ok();
    fleet
        .folders
        .iter()
        .find(|c| {
            c.name == word
                || path.as_ref().is_some_and(|p| fleet.places.get(&c.name).is_some_and(|q| Path::new(q) == p))
        })
        .map(|c| c.name.clone())
        .ok_or_else(|| format!("{word} is not a synced folder here (see `nerve folders`)"))
}

fn devices(dir: &Path) -> Result<String, String> {
    let (fleet, states, _) = peer_states(dir)?;
    let w = states.iter().map(|(n, _)| n.len()).chain([fleet.device.len()]).max().unwrap_or(0);
    let mut out = vec![format!("{:w$}  this device · nerve {}", fleet.device, env!("CARGO_PKG_VERSION"))];
    for (name, st) in &states {
        let mut parts = vec![match st {
            Some(d) if crate::util::now_secs().saturating_sub(d.generated_at) < 180 => "online".to_string(),
            Some(d) => format!("last seen {}", ago(d.generated_at)),
            None => "not heard from yet".into(),
        }];
        if let Some(d) = st {
            parts.push(format!("nerve {}", Sanitised::new(d.nerve_version.clone())));
        }
        if fleet.closed.contains(name) {
            parts.push("devices you add later don't meet it".into());
        }
        out.push(format!("{name:w$}  {}", parts.join(" · ")));
    }
    Ok(out.join("\n"))
}

/// Every approval on any device: (the device holding it, the approval).
fn all_approvals(dir: &Path) -> Result<(fleet::Fleet, Vec<(String, Approval)>), String> {
    let (fleet, states, _) = peer_states(dir)?;
    let mut out: Vec<(String, Approval)> =
        local_state(dir).into_iter().flat_map(|l| l.approvals).map(|a| (fleet.device.clone(), a)).collect();
    for (n, s) in states {
        out.extend(s.into_iter().flat_map(|s| s.approvals).map(|a| (n.clone(), a)));
    }
    Ok((fleet, out))
}

fn requests(dir: &Path) -> Result<String, String> {
    let (fleet, all) = all_approvals(dir)?;
    if all.is_empty() {
        return Ok("nothing waits for a yes".into());
    }
    let keys = fleet.peer_keys();
    let me = fleet::device_keys(dir).map(|k| k.public_key().to_hex()).unwrap_or_default();
    let name_of = |hex: &str| {
        if hex == me {
            fleet.device.clone()
        } else {
            keys.iter().find(|(_, k)| k.to_hex() == hex).map_or_else(|| "a device".into(), |(n, _)| n.clone())
        }
    };
    let lines: Vec<String> = all
        .iter()
        .map(|(holder, a)| {
            let (title, detail) = a.describe(&name_of(&a.from), holder);
            format!("{}  {title}, {}\n        {detail}", protocol::short_id(&a.id, 6), ago(a.at))
        })
        .collect();
    Ok(crate::util::sanitize::clean(format!("{}\n\nnerve allow <id>, or nerve refuse <id>", lines.join("\n"))))
}

fn decide(dir: &Path, id: &str, yes: bool, rest: &[&str]) -> Result<String, String> {
    let mut d = Decision { id: id.to_string(), yes, ..Default::default() };
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let val = it.next().ok_or(format!("{arg} needs a value"))?;
        match *arg {
            "--at" => d.at = Some(std::path::absolute(val).map_err(|e| e.to_string())?.display().to_string()),
            "--others" => d.others = Some(Others::parse(val).ok_or("--others: use ask, inside or any")?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    ask_agent(dir, Request::Fleet(FleetReq::Decide(d)))
}

fn folders(dir: &Path) -> Result<String, String> {
    let fleet = set_up(dir)?;
    if fleet.folders.is_empty() {
        return Ok("no synced folders; start one with `nerve sync <dir> --with <devices>`".into());
    }
    let local = local_state(dir);
    let now = crate::util::now_secs();
    let out: Vec<String> = fleet
        .folders
        .iter()
        .map(|c| {
            let path = fleet.places.get(&c.name).cloned().unwrap_or_default();
            let parent = Path::new(&path).parent().map(|p| p.display().to_string()).unwrap_or_default();
            let how = match local
                .as_ref()
                .and_then(|l| l.folders.iter().find(|f| f.name == c.name))
                .and_then(|f| f.mine.as_ref())
            {
                None => "not running (start nerve's background agent)".to_string(),
                Some(v) => super::folders::status(v, now).0,
            };
            format!(
                "{}  {path}\n  with {} · {} · {} · {how}",
                c.name,
                c.with.join(", "),
                c.when.label(),
                c.others.label(&parent)
            )
        })
        .collect();
    Ok(crate::util::sanitize::clean(out.join("\n")))
}

fn versions(dir: &Path, folder: Option<&str>) -> Result<String, String> {
    let fleet = set_up(dir)?;
    let all: Vec<_> = Versions::new(dir, fleet.xfer.bulk_over)
        .list()
        .into_iter()
        .filter(|v| folder.is_none_or(|f| v.folder == f))
        .collect();
    if all.is_empty() {
        return Ok(format!(
            "no earlier versions kept (a sync keeps what it replaces for {} hours)",
            fleet.keep_versions_hours
        ));
    }
    let lines: Vec<String> =
        all.iter().map(|v| format!("{}  {:>8}  {}  {}  {}", v.id, ago(v.at), v.folder, v.rel, v.why)).collect();
    Ok(crate::util::sanitize::clean(format!("{}\n\nnerve restore <id> puts one back", lines.join("\n"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_takes_a_prompt_and_its_flags_in_any_order() {
        let a = parse_start(&["beta", "~/code", "fix the tests", "--wait"]).unwrap();
        assert_eq!(
            a,
            StartArgs {
                device: "beta".into(),
                dir: "~/code".into(),
                prompt: "fix the tests".into(),
                wait: Some(TURN_WAIT)
            }
        );
        let a = parse_start(&["beta", "proj", "--wait", "--timeout", "90", "say", "hi"]).unwrap();
        assert_eq!((a.prompt.as_str(), a.wait), ("say hi", Some(Duration::from_secs(90))));
        let a = parse_start(&["beta", "/srv/x", "--", "--wait", "is", "a", "word", "here"]).unwrap();
        assert_eq!((a.prompt.as_str(), a.wait), ("--wait is a word here", None));
        let quoted = "a \"b\"; $(c)\nd";
        assert_eq!(parse_start(&["beta", "~", quoted]).unwrap().prompt, quoted, "one argument stays as it is");
        for bad in [&["beta"][..], &["beta", "~"], &["beta", "~", "  "], &["beta", "~", "x", "--timeout", "9"]] {
            assert!(parse_start(bad).is_err(), "{bad:?}");
        }
        assert!(parse_start(&["beta", "~", "x", "--wait", "--timeout", "soon"]).unwrap_err().contains("seconds"));
        assert!(parse_start(&["beta", "~", "x", "--verify"]).unwrap_err().contains("unknown flag"));
    }

    fn session(state: &str, tail: &[&str]) -> RemoteSession {
        RemoteSession { state: state.into(), tail: tail.iter().map(|l| l.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn wait_ends_when_the_first_turn_does() {
        let mut t = Turn::default();
        assert_eq!(t.see(None), Step::Wait, "not reported yet");
        assert_eq!(t.see(Some(&session("Idle", &["› do it"]))), Step::Wait, "idle before it began");
        assert_eq!(t.see(Some(&session("Processing", &["› do it"]))), Step::Wait);
        assert!(matches!(t.see(Some(&session("Waiting", &[]))), Step::Asking(_)));
        assert_eq!(t.see(Some(&session("Waiting", &[]))), Step::Wait, "said once");
        assert_eq!(t.see(Some(&session("Idle", &["› do it"]))), Step::Done, "idle after working");

        let mut quick = Turn::default();
        assert_eq!(
            quick.see(Some(&session("Idle", &["› do it", "● done"]))),
            Step::Done,
            "a turn quicker than a state"
        );
        let mut again = Turn::default();
        assert_eq!(again.see(Some(&session("Idle", &["● old", "› new"]))), Step::Wait, "a reply before the prompt");

        let mut gone = Turn::default();
        gone.see(Some(&session("Processing", &[])));
        assert!(matches!(gone.see(None), Step::Failed(_)), "it went away mid-turn");
        assert!(matches!(Turn::default().see(Some(&session("Error", &[]))), Step::Failed(_)));
    }
}
