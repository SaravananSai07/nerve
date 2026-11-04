//! CLI ⇄ agent. A command files each request under an id it picks: with
//! the long-lived agent (the TUI or `nerve sync agent`) through
//! `<sync>/spool/`, since that agent holds this device's identity, or
//! with a short-lived agent of its own when none runs. Every request ends
//! in one outcome line naming its id, which the command follows.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::agent::{self, AgentHandle, Ctx, Request};
use super::fleet;
use super::protocol;

// ---- spool ----

#[derive(Serialize, Deserialize)]
struct Spooled<R> {
    id: String,
    req: R,
}

/// Queue a request for the running agent; the file appears whole. Named
/// `<nanos>-<id>`, so name order is arrival order.
pub(crate) fn put(sync_dir: &Path, id: &str, req: &Request) -> Result<(), String> {
    let spool = sync_dir.join("spool");
    std::fs::create_dir_all(&spool).map_err(|e| e.to_string())?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tag: String = id.chars().map(|c| if crate::util::is_safe_id_char(c) { c } else { '_' }).collect();
    let name = format!("{nanos:020}-{tag}");
    let tmp = spool.join(format!(".{name}"));
    let json = serde_json::to_string(&Spooled { id: id.to_string(), req }).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, spool.join(format!("{name}.json"))).map_err(|e| e.to_string())
}

/// Take every queued request with its id, oldest first, deleting the files.
/// One that cannot be read still comes back, as an error, so its id gets an
/// outcome instead of silence.
pub(crate) fn drain(sync_dir: &Path) -> Vec<(String, Result<Request, String>)> {
    let Ok(rd) = std::fs::read_dir(sync_dir.join("spool")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let raw = std::fs::read_to_string(&p);
            let _ = std::fs::remove_file(&p);
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let named = stem.split_once('-').map_or(stem, |(_, id)| id).to_string();
            match raw.ok().and_then(|r| serde_json::from_str::<Spooled<serde_json::Value>>(&r).ok()) {
                Some(Spooled { id, req }) => (id, serde_json::from_value(req).map_err(|_| UNREADABLE.to_string())),
                None => (named, Err(UNREADABLE.to_string())),
            }
        })
        .collect()
}

const UNREADABLE: &str = "unreadable request";

// ---- activity lines ----

/// The long-lived agent's activity, one line per event: `<clock> <text>`.
pub(crate) fn activity_file(sync_dir: &Path) -> PathBuf {
    sync_dir.join("activity.log")
}

/// The activity file grows to this, then becomes `activity.log.1`.
const ACTIVITY_MAX: u64 = 1 << 20;

/// Add `line` to the activity file, starting a fresh one (and keeping the
/// last full one) once it passes `ACTIVITY_MAX`. Followers see a shorter
/// file and start over.
pub(crate) fn append_activity(path: &Path, line: &str) {
    use std::io::Write;
    if std::fs::metadata(path).is_ok_and(|m| m.len() > ACTIVITY_MAX) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(out, "{line}");
    }
}

pub(crate) fn clock(secs: u64) -> String {
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

const OK: &str = "\u{2713}";
const FAIL: &str = "\u{2717}";
/// Between a device and a run id on a line of the run's output.
const OUTPUT: &str = "\u{2502}";

/// How a request ended, as its outcome line marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ok,
    Failed,
    /// Failed because the relay could not carry it, not on its merits:
    /// a follower with an rsync route may finish it.
    RelayFailed,
}

impl From<bool> for Outcome {
    fn from(ok: bool) -> Self {
        if ok {
            Self::Ok
        } else {
            Self::Failed
        }
    }
}

/// After the id on a `RelayFailed` outcome line.
const RELAY: &str = " (relay)";

/// How request `id` ended, as `device` saw it: `<device> ✓|✗ <id>: <msg>`,
/// or `<device> ✗ <id> (relay): <msg>`.
pub(crate) fn outcome_line(device: &str, outcome: Outcome, id: &str, msg: &str) -> String {
    let (mark, tag) = match outcome {
        Outcome::Ok => (OK, ""),
        Outcome::Failed => (FAIL, ""),
        Outcome::RelayFailed => (FAIL, RELAY),
    };
    format!("{device} {mark} {id}{tag}: {msg}")
}

/// A line of run `id`'s output: `<device> │ <id> <text>`.
pub(crate) fn output_line(device: &str, id: &str, text: &str) -> String {
    format!("{device} {OUTPUT} {id} {text}")
}

/// An outcome line, with its clock: (id, outcome, msg). Read by position,
/// so a ✓ inside command output never counts.
fn parse_outcome(line: &str) -> Option<(&str, Outcome, &str)> {
    let mut it = line.splitn(4, ' ');
    let (_clock, _device, mark, rest) = (it.next()?, it.next()?, it.next()?, it.next()?);
    let ok = match mark {
        OK => true,
        FAIL => false,
        _ => return None,
    };
    let (id, msg) = rest.split_once(": ").unwrap_or((rest.trim_end_matches(':'), ""));
    Some(match id.strip_suffix(RELAY) {
        Some(id) if !ok => (id, Outcome::RelayFailed, msg),
        _ => (id, ok.into(), msg),
    })
}

/// How request `id` ended, if `line` says: (ok, message).
pub(crate) fn outcome_of<'a>(line: &'a str, id: &str) -> Option<(bool, &'a str)> {
    parse_outcome(line).filter(|(i, _, _)| *i == id).map(|(_, o, msg)| (o == Outcome::Ok, msg))
}

/// Is `line` the failed outcome of request `id`?
pub(crate) fn failed(line: &str, id: &str) -> bool {
    outcome_of(line, id).is_some_and(|(ok, _)| !ok)
}

/// The text of a line of run `id`'s output.
fn output_of<'a>(line: &'a str, id: &str) -> Option<&'a str> {
    let mark = format!(" {OUTPUT} {id} ");
    line.find(&mark).map(|at| &line[at + mark.len()..])
}

// ---- following ----

/// Activity lines as they are noted.
enum Follower {
    /// The running agent's log, from byte `at` (it may rotate).
    Log { path: PathBuf, at: u64 },
    /// A short-lived agent of our own, from line `seen`.
    Own { handle: AgentHandle, seen: u64 },
}

impl Follower {
    /// The running agent's log, from its end now.
    fn log(sync_dir: &Path) -> Self {
        let path = activity_file(sync_dir);
        let at = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self::Log { path, at }
    }

    /// Lines noted since the last call. Only whole lines: the agent may
    /// be mid-write.
    fn poll(&mut self) -> Result<Vec<String>, String> {
        match self {
            Self::Log { path, at } => {
                let Ok(mut f) = std::fs::File::open(&*path) else { return Ok(Vec::new()) };
                if f.metadata().map(|m| m.len()).unwrap_or(0) < *at {
                    *at = 0;
                }
                let mut new = Vec::new();
                if f.seek(SeekFrom::Start(*at)).and_then(|_| f.read_to_end(&mut new)).is_err() {
                    return Ok(Vec::new());
                }
                let Some(end) = new.iter().rposition(|b| *b == b'\n') else { return Ok(Vec::new()) };
                *at += end as u64 + 1;
                Ok(String::from_utf8_lossy(&new[..end]).lines().map(String::from).collect())
            }
            Self::Own { handle, seen } => {
                let v = handle.snapshot();
                if let Some(e) = v.error {
                    return Err(e);
                }
                let lines = v.lines_since(*seen).cloned().collect();
                *seen = v.seq;
                Ok(lines)
            }
        }
    }
}

/// Start a short-lived agent here and wait until it is on the relays.
pub(crate) fn own_agent(sync_dir: &Path) -> Result<AgentHandle, String> {
    let fleet = fleet::set_up(sync_dir)?;
    let handle = agent::spawn(Ctx {
        paths: crate::paths::Paths::at(sync_dir.parent().unwrap_or(sync_dir).to_path_buf()),
        sync_dir: sync_dir.to_path_buf(),
        fleet,
        statusline_dir: None,
        lock: None,
        notifier: None,
    })
    .map_err(|e| e.to_string())?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        let v = handle.snapshot();
        if let Some(e) = v.error {
            return Err(e);
        }
        if v.running {
            return Ok(handle);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("timed out connecting to relays".into())
}

/// What following a request waits for, and what it prints meanwhile.
pub(crate) enum Until {
    /// Request `id`'s outcome, printing nothing.
    Quiet(String),
    /// Its outcome, printing every line meanwhile, as the short commands do.
    Echo(String),
    /// Its outcome, printing the lines that name it to stderr. A request
    /// that says nothing about itself for `STALL` is taken as stuck (a
    /// running rsync speaks every minute).
    Id(String),
    /// The end of run `id`: its `run finished` line, or a refusal. Its
    /// output goes to stdout as plain lines, so a waited-on run pipes like
    /// a local one.
    RunEnd(String),
}

/// How a followed request ended.
pub(crate) struct Ended {
    pub(crate) ok: bool,
    pub(crate) msg: String,
    /// For runs: the remote exit code.
    pub(crate) code: Option<i32>,
    /// It failed because the relay could not carry it, not on its merits.
    pub(crate) relay: bool,
}

pub(crate) const STALL: Duration = Duration::from_secs(5 * 60);
/// `run --wait` exit code when the command never ran (refused, unknown
/// device, no answer), as `docker run` and `env` use it.
pub(crate) const NOT_RUN: i32 = 125;

impl Until {
    fn id(&self) -> &str {
        match self {
            Self::Quiet(id) | Self::Echo(id) | Self::Id(id) | Self::RunEnd(id) => id,
        }
    }

    /// Print what matters about `line`; Some once it is the end.
    fn feed(&self, line: &str) -> Option<Ended> {
        let id = self.id();
        if let Self::RunEnd(_) = self {
            if let Some(text) = output_of(line, id) {
                println!("{text}");
                return None;
            }
        }
        let outcome = parse_outcome(line).filter(|(i, _, _)| *i == id);
        match self {
            Self::Quiet(_) | Self::RunEnd(_) => {}
            Self::Echo(_) => println!("{line}"),
            Self::Id(_) if outcome.is_some() || line.contains(id) => eprintln!("{line}"),
            Self::Id(_) => {}
        }
        let (_, outcome, msg) = outcome?;
        let ok = outcome == Outcome::Ok;
        let ended = Ended { ok, msg: msg.to_string(), code: None, relay: outcome == Outcome::RelayFailed };
        let Self::RunEnd(_) = self else { return Some(ended) };
        eprintln!("{line}");
        if let Some(code) = msg.strip_prefix("run finished: exit ") {
            let code = code.split_whitespace().next().and_then(|c| c.parse().ok()).unwrap_or(NOT_RUN);
            return Some(Ended { code: Some(code), ..ended });
        }
        // Accepted: keep following. Refused: it never ran.
        (!ok).then_some(Ended { code: Some(NOT_RUN), ..ended })
    }

    /// Has the request said nothing about itself for `STALL`? `last`:
    /// when it last did.
    fn stalled(&self, line: &str, last: &mut Instant) -> bool {
        let Self::Id(id) = self else { return false };
        if line.contains(id.as_str()) {
            *last = Instant::now();
        }
        last.elapsed() > STALL
    }
}

/// File `req` with this device's agent and follow it until `until` says
/// it ended (Some) or `max` passes (None). With `detach` and a running
/// agent, return as soon as it is queued.
pub(crate) fn drive(
    sync_dir: &Path,
    req: Request,
    until: Until,
    max: Duration,
    detach: bool,
) -> Result<Option<Ended>, String> {
    fleet::set_up(sync_dir)?;
    let id = until.id().to_string();
    let mut follower = if agent::agent_alive(sync_dir) {
        let f = Follower::log(sync_dir);
        put(sync_dir, &id, &req)?;
        if detach {
            return Ok(None);
        }
        f
    } else {
        if detach {
            eprintln!("no sync agent runs here, so this command stays until the transfer ends");
        }
        let handle = own_agent(sync_dir)?;
        handle.request_as(id, req);
        Follower::Own { handle, seen: 0 }
    };
    let deadline = Instant::now() + max;
    let mut last = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if until.stalled("", &mut last) {
            return Ok(None);
        }
        for line in follower.poll()? {
            until.stalled(&line, &mut last);
            if let Some(e) = until.feed(&line) {
                return Ok(Some(e));
            }
        }
    }
    Ok(None)
}

/// File `req` and print the activity until its outcome, or `secs` pass.
pub(crate) fn oneshot(sync_dir: &Path, req: Request, secs: u64) -> Result<String, String> {
    match drive(sync_dir, req, Until::Echo(protocol::new_id()), Duration::from_secs(secs), false)? {
        None if agent::agent_alive(sync_dir) => {
            Ok("handed to the running sync agent; no answer yet (see `nerve sync status`)".into())
        }
        _ => Ok(String::new()),
    }
}

/// Hand `req` to the background agent and wait (briefly) for its outcome.
pub(crate) fn ask_agent(sync_dir: &Path, req: Request) -> Result<String, String> {
    fleet::set_up(sync_dir)?;
    if !agent::agent_alive(sync_dir) {
        return Err(
            "this needs nerve's background agent: `nerve connect` installs it, or run `nerve sync agent`".into()
        );
    }
    match drive(sync_dir, req, Until::Quiet(protocol::new_id()), Duration::from_secs(10), false)? {
        Some(Ended { ok: true, msg, .. }) => Ok(msg),
        Some(Ended { msg, .. }) => Err(msg),
        None => Ok("no answer from the background agent yet; it may still be working (see `nerve sync status`)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::agent::{FleetReq, FolderReq};
    use crate::sync::folders::{Others, When};
    use crate::sync::protocol::{Answer, Body, RunSpec, XferOffer, XferWhat};

    #[test]
    fn requests_round_trip_through_the_spool_with_their_ids() {
        let dir = tempfile::tempdir().unwrap();
        let add = FolderReq::Add {
            path: "/x/site".into(),
            with: vec!["server".into()],
            when: When::Watch,
            others: Others::Inside,
            yes: true,
        };
        let reqs = [
            Request::To { device: "server".into(), body: Body::Answer { request: "ab".into(), answer: Answer::Allow } },
            Request::SetAway(true),
            Request::Xfer {
                device: "server".into(),
                offer: XferOffer {
                    place: "h".into(),
                    what: XferWhat::Files { roots: vec!["a".into()], delete: true },
                    ..Default::default()
                },
                pull: true,
            },
            Request::Run { spec: RunSpec { argv: vec!["ls".into()], ..Default::default() }, device: "server".into() },
            Request::Handoff {
                session_id: "s".into(),
                cwd: "/x".into(),
                device: "server".into(),
                continue_with: None,
                place: Some("h".into()),
                force: true,
            },
            Request::Folder(add.clone()),
            Request::Fleet(FleetReq::Link { device: "pi".into(), with: vec![] }),
            Request::SetPrivate { target: crate::sync::fleet::PrivateTarget::Session("s1".into()), on: true },
        ];
        for (i, r) in reqs.iter().enumerate() {
            put(dir.path(), &format!("r{i}"), r).unwrap();
        }
        let got = drain(dir.path());
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7"], "in order, each with its id");
        let got: Vec<Request> = got.into_iter().map(|(_, r)| r.unwrap()).collect();
        assert!(matches!(&got[0], Request::To { device, .. } if device == "server"));
        assert!(matches!(got[1], Request::SetAway(true)));
        assert!(matches!(&got[2], Request::Xfer { pull: true, offer, .. } if offer.place == "h"));
        assert!(matches!(&got[3], Request::Run { spec, .. } if spec.argv == ["ls"]));
        assert!(matches!(&got[4], Request::Handoff { place: Some(p), force: true, .. } if p == "h"));
        assert!(matches!(&got[5], Request::Folder(f) if *f == add));
        assert!(
            matches!(&got[6], Request::Fleet(FleetReq::Link { device, with }) if device == "pi" && with.is_empty())
        );
        assert!(matches!(&got[7], Request::SetPrivate { on: true, .. }));
        assert!(drain(dir.path()).is_empty(), "drained files are removed");
    }

    #[test]
    fn an_unreadable_spool_file_is_reported_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "good", &Request::SetAway(true)).unwrap();
        let spool = dir.path().join("spool");
        // Known id, request from some other version.
        std::fs::write(spool.join("00000000000000000000-odd.json"), r#"{"id":"odd","req":{"t":"nope"}}"#).unwrap();
        // Not even JSON: the file name still names the request.
        std::fs::write(spool.join("00000000000000000001-junk.json"), "{ torn").unwrap();
        let got = drain(dir.path());
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["odd", "junk", "good"]);
        assert!(got[0].1.is_err() && got[1].1.is_err() && got[2].1.is_ok());
        assert!(drain(dir.path()).is_empty());
    }

    #[test]
    fn outcome_lines_read_back_as_written() {
        let line = format!("10:00:00Z {}", outcome_line("server", Outcome::Failed, "x1", "no such place"));
        assert_eq!(parse_outcome(&line), Some(("x1", Outcome::Failed, "no such place")));
        let line = format!("10:00:00Z {}", outcome_line("server", Outcome::RelayFailed, "x1", "timed out"));
        assert_eq!(parse_outcome(&line), Some(("x1", Outcome::RelayFailed, "timed out")));
        assert!(failed(&line, "x1"));
        let line = format!("10:00:00Z {}", outcome_line("server", Outcome::Ok, "x1", "relay failed: just words"));
        assert_eq!(parse_outcome(&line), Some(("x1", Outcome::Ok, "relay failed: just words")));
        let out = format!("10:00:00Z {}", output_line("server", "x1", "\u{2713} x1: not an outcome"));
        assert_eq!(parse_outcome(&out), None, "output is never an outcome");
        assert_eq!(output_of(&out, "x1"), Some("\u{2713} x1: not an outcome"));
    }

    #[test]
    fn run_end_is_read_from_outcome_lines_not_output() {
        let u = Until::RunEnd("ab12".into());
        assert!(u.feed("10:00:00Z server \u{2502} ab12 \u{2713} looks like an outcome").is_none());
        assert!(
            u.feed("10:00:00Z server \u{2713} ab12: run accepted: node x in /h").is_none(),
            "accepted is not the end"
        );
        let e = u.feed("10:00:01Z server \u{2717} ab12: run finished: exit 3 (exit 3 after 0.1s)").unwrap();
        assert_eq!((e.ok, e.code), (false, Some(3)));
        let r = u.feed("10:00:00Z server \u{2717} ab12: not allowed on server: …").unwrap();
        assert_eq!(r.code, Some(NOT_RUN));
        assert!(u.feed("10:00:00Z server \u{2717} other: run finished: exit 0 (x)").is_none(), "other ids are ignored");
    }

    #[test]
    fn requests_end_on_their_own_id() {
        let u = Until::Id("x1".into());
        assert!(u.feed("10:00:00Z laptop \u{2713} y2: something else").is_none());
        let e = u.feed("10:00:00Z server \u{2713} x1: server place h: 1 file(s) written").unwrap();
        assert!(e.ok && e.msg == "server place h: 1 file(s) written");
        let r = Until::Quiet("x2".into()).feed("10:00:00Z laptop \u{2717} x2 (relay): no relay").unwrap();
        assert!(!r.ok && r.relay);
        let r = Until::Quiet("x3".into()).feed("10:00:00Z laptop \u{2717} x3: relay failed: only words").unwrap();
        assert!(!r.ok && !r.relay, "the words don't make it a relay failure");
    }

    #[test]
    fn the_log_follower_reads_whole_lines_and_survives_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let log = activity_file(dir.path());
        std::fs::write(&log, "old line\n").unwrap();
        let mut f = Follower::log(dir.path());
        std::fs::write(&log, "old line\nnew one\nhalf").unwrap();
        assert_eq!(f.poll().unwrap(), ["new one"]);
        assert!(f.poll().unwrap().is_empty(), "a half-written line waits");
        std::fs::write(&log, "fresh\n").unwrap();
        assert_eq!(f.poll().unwrap(), ["fresh"], "a shorter file is a new one");
    }

    #[test]
    fn the_activity_file_rotates_past_its_cap_and_followers_carry_on() {
        let dir = tempfile::tempdir().unwrap();
        let log = activity_file(dir.path());
        std::fs::write(&log, vec![b'x'; ACTIVITY_MAX as usize + 1]).unwrap();
        let mut f = Follower::log(dir.path());
        append_activity(&log, "10:00:00Z after rotation");
        assert_eq!(f.poll().unwrap(), ["10:00:00Z after rotation"]);
        assert!(dir.path().join("activity.log.1").exists());
        assert!(std::fs::metadata(&log).unwrap().len() < 100);
    }
}
