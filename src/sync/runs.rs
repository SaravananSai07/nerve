//! Plain-command runs (`nerve sync run`): argv as given, in a place or a
//! directory, allowed only by an explicit `[run] allow` rule in this
//! device's fleet.toml. A peer can already type into sessions, but a
//! command runs with no human watching, so nothing runs by default.
//!
//! Progress reuses `JobStatus` (kind "run"): an output tail, the line
//! count, and finally the exit code.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use super::fleet::{Fleet, RunRule};
use super::protocol::{now_secs, JobStatus, RunSpec};

/// Output lines carried per update.
pub(crate) const RUN_TAIL: usize = 20;
/// At most one progress update per this, so a chatty command can't
/// flood the relay.
const UPDATE_EVERY: Duration = Duration::from_secs(1);
/// What `timeout(1)` exits with; reported when `--timeout` kills it.
pub(crate) const TIMED_OUT: i32 = 124;

/// How an allowed run executes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub(crate) cwd: PathBuf,
    pub(crate) argv: Vec<String>,
    pub(crate) shell: bool,
}

/// A rule's argv: literal, with an optional final `"*"` for any trailing
/// arguments. A `*` anywhere else makes the rule invalid (never matches).
pub(crate) fn rule_matches(pattern: &[String], argv: &[String]) -> bool {
    match pattern.split_last() {
        None => false,
        Some((last, head)) if last == "*" => {
            !head.is_empty() && !head.iter().any(|p| p == "*") && argv.len() >= head.len() && argv[..head.len()] == *head
        }
        Some(_) => !pattern.iter().any(|p| p == "*") && pattern == argv,
    }
}

/// Where the run goes and whether a rule allows it; the refusal says
/// what to add to `fleet_file`.
pub(crate) fn authorize(fleet: &Fleet, fleet_file: &Path, spec: &RunSpec) -> Result<Plan, String> {
    if spec.argv.is_empty() || spec.argv.iter().any(|a| a.contains('\0')) {
        return Err("empty or malformed command".into());
    }
    if spec.place.is_some() && spec.cwd.is_some() {
        return Err("give --place or --cwd, not both".into());
    }
    let cwd = match (&spec.place, &spec.cwd) {
        (Some(p), _) => fleet.place(p, fleet_file)?,
        (None, Some(c)) if Path::new(c).is_absolute() => PathBuf::from(c),
        (None, Some(c)) => return Err(format!("--cwd must be absolute: {c}")),
        (None, None) => dirs::home_dir().ok_or("no home directory")?,
    };
    let fits = |r: &RunRule| {
        r.place.as_ref().is_none_or(|p| spec.place.as_ref() == Some(p))
            && r.cwd.as_ref().is_none_or(|c| Path::new(c) == cwd)
            && rule_matches(&r.argv, &spec.argv)
    };
    let Some(rule) = fleet.run.allow.iter().find(|r| fits(r)) else {
        let where_ = match (&spec.place, &spec.cwd) {
            (Some(p), _) => format!("place = \"{p}\", "),
            (None, Some(c)) => format!("cwd = \"{c}\", "),
            _ => String::new(),
        };
        let argv: Vec<String> = spec.argv.iter().map(|a| format!("{a:?}")).collect();
        return Err(format!(
            "not allowed on {}: no [run] allow rule matches. To allow it, add {{ {where_}argv = [{}] }} to `allow` under [run] in {} (a final \"*\" allows any further arguments)",
            fleet.device,
            argv.join(", "),
            fleet_file.display()
        ));
    };
    if !cwd.is_dir() {
        return Err(format!("{} is not a directory on {}", cwd.display(), fleet.device));
    }
    Ok(Plan { cwd, argv: spec.argv.clone(), shell: rule.shell })
}

fn push_tail(st: &mut JobStatus, line: &str) {
    st.tail.push(line.chars().take(240).collect());
    if st.tail.len() > RUN_TAIL {
        st.tail.remove(0);
    }
    st.lines += 1;
}

/// The status a run starts with, before its thread reports.
pub(crate) fn queued(spec: &RunSpec, device: &str) -> JobStatus {
    JobStatus {
        id: spec.id.clone(),
        title: spec.argv.join(" ").chars().take(80).collect(),
        device: device.to_string(),
        phase: "queued".into(),
        kind: "run".into(),
        updated_at: now_secs(),
        ..Default::default()
    }
}

/// Run to completion on this thread, sending throttled status updates
/// and always a final one with the exit code.
pub(crate) fn run(spec: &RunSpec, plan: &Plan, device: &str, tx: &Sender<JobStatus>) {
    let mut st = queued(spec, device);
    st.phase = "running".into();
    let send = |st: &mut JobStatus| {
        st.updated_at = now_secs();
        let _ = tx.send(st.clone());
    };
    send(&mut st);
    // What actually runs: argv as given, or `sh -c` for a shell rule.
    let target: Vec<String> = if plan.shell {
        vec!["/bin/sh".into(), "-c".into(), plan.argv.join(" ")]
    } else {
        // `./bin/x` means the one in the run's directory, not the agent's.
        let prog = &plan.argv[0];
        let prog = if prog.contains('/') && !prog.starts_with('/') { plan.cwd.join(prog).to_string_lossy().into_owned() } else { prog.clone() };
        match super::bins::find(&prog) {
            Ok(exe) => std::iter::once(exe.to_string_lossy().into_owned()).chain(plan.argv[1..].iter().cloned()).collect(),
            Err(e) => {
                st.phase = "failed".into();
                st.result = Some(e);
                return send(&mut st);
            }
        }
    };
    // A process with a SIGCHLD reaper (the TUI hosting an agent) can reap
    // the child before `wait` sees it, losing the exit code. So the
    // command runs under `"$@"` (argv passed through verbatim, nothing
    // parsed) and its status also goes to a file, passed as `$0`.
    let stage = match super::handoff::tempdir() {
        Ok(t) => t,
        Err(e) => {
            st.phase = "failed".into();
            st.result = Some(e);
            return send(&mut st);
        }
    };
    let status_file = stage.path().join("status");
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(r#""$@"; s=$?; echo "$s" >"$0"; exit "$s""#).arg(&status_file).args(&target);
    cmd.current_dir(&plan.cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Its own process group, so a timeout takes its children with it.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            st.phase = "failed".into();
            st.result = Some(format!("could not start {}: {e}", plan.argv[0]));
            return send(&mut st);
        }
    };
    let (ltx, lrx) = channel::<String>();
    for out in [child.stdout.take().map(|o| Box::new(o) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|e| Box::new(e) as Box<dyn std::io::Read + Send>)]
        .into_iter()
        .flatten()
    {
        let ltx = ltx.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if ltx.send(l).is_err() {
                    break;
                }
            }
        });
    }
    drop(ltx);
    let started = Instant::now();
    let deadline = spec.timeout_secs.map(|s| started + Duration::from_secs(s));
    let mut last_sent = Instant::now();
    let mut dirty = false;
    let mut timed_out = false;
    let status = loop {
        match lrx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) => {
                push_tail(&mut st, &l);
                dirty = true;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // Both pipes closed, but the command may still run: keep
            // polling it below, so the timeout still applies.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(100)),
        }
        if deadline.is_some_and(|d| Instant::now() >= d) && !timed_out {
            timed_out = true;
            kill_group(child.id(), &mut child);
        }
        match child.try_wait() {
            Ok(Some(s)) => {
                // Drain what is already buffered, then stop.
                while let Ok(l) = lrx.recv_timeout(Duration::from_millis(50)) {
                    push_tail(&mut st, &l);
                }
                break Some(s);
            }
            Ok(None) => {}
            // Reaped elsewhere: gone, and its status is in the file.
            Err(_) => {
                while let Ok(l) = lrx.recv_timeout(Duration::from_millis(50)) {
                    push_tail(&mut st, &l);
                }
                break None;
            }
        }
        if dirty && last_sent.elapsed() >= UPDATE_EVERY {
            send(&mut st);
            last_sent = Instant::now();
            dirty = false;
        }
    };
    let code = status
        .map(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
        })
        .or_else(|| std::fs::read_to_string(&status_file).ok().and_then(|c| c.trim().parse().ok()));
    st.exit_code = Some(if timed_out { TIMED_OUT } else { code.unwrap_or(-1) });
    st.phase = if st.exit_code == Some(0) { "done" } else { "failed" }.into();
    st.result = Some(if timed_out {
        format!("timed out after {}s and was killed", spec.timeout_secs.unwrap_or(0))
    } else {
        format!("exit {} after {:.1}s", st.exit_code.unwrap_or(-1), started.elapsed().as_secs_f64())
    });
    send(&mut st);
}

fn kill_group(pid: u32, child: &mut std::process::Child) {
    use nix::sys::signal::{killpg, Signal};
    let pg = nix::unistd::Pid::from_raw(pid as i32);
    let _ = killpg(pg, Signal::SIGTERM);
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = killpg(pg, Signal::SIGKILL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_a_final_star_globs() {
        let p = v(&["node", "bin/v2.mjs", "*"]);
        assert!(rule_matches(&p, &v(&["node", "bin/v2.mjs"])));
        assert!(rule_matches(&p, &v(&["node", "bin/v2.mjs", "verify", "S5"])));
        assert!(!rule_matches(&p, &v(&["node", "other.mjs", "x"])));
        assert!(!rule_matches(&p, &v(&["node"])));
        assert!(rule_matches(&v(&["make", "test"]), &v(&["make", "test"])));
        assert!(!rule_matches(&v(&["make", "test"]), &v(&["make", "test", "extra"])));
        assert!(!rule_matches(&v(&["sh", "*", "x"]), &v(&["sh", "*", "x"])), "a mid-argv star invalidates the rule");
        assert!(!rule_matches(&v(&["*"]), &v(&["rm", "-rf", "/"])), "a bare star allows nothing");
        assert!(!rule_matches(&[], &v(&["ls"])));
    }

    fn fleet_with(dir: &Path) -> Fleet {
        let mut f = Fleet { device: "pi".into(), ..Default::default() };
        f.places.insert("harness".into(), dir.to_string_lossy().into_owned());
        f.run.allow.push(RunRule { place: Some("harness".into()), argv: v(&["echo", "*"]), ..Default::default() });
        f
    }

    #[test]
    fn authorize_needs_a_matching_rule_and_says_how_to_add_one() {
        let dir = tempfile::tempdir().unwrap();
        let f = fleet_with(dir.path());
        let file = Path::new("/x/fleet.toml");
        let spec = RunSpec { argv: v(&["echo", "hi"]), place: Some("harness".into()), ..Default::default() };
        let plan = authorize(&f, file, &spec).unwrap();
        assert_eq!((plan.cwd.as_path(), plan.shell), (dir.path(), false));

        let other_dir = RunSpec { cwd: Some("/tmp".into()), place: None, ..spec.clone() };
        let e = authorize(&f, file, &other_dir).unwrap_err();
        assert!(e.contains("no [run] allow rule") && e.contains("cwd = \"/tmp\"") && e.contains("/x/fleet.toml"), "{e}");

        let rm = RunSpec { argv: v(&["rm", "-rf", "x"]), ..spec.clone() };
        assert!(authorize(&f, file, &rm).unwrap_err().contains("argv = [\"rm\", \"-rf\", \"x\"]"));

        let unknown = RunSpec { place: Some("nope".into()), ..spec };
        let e = authorize(&f, file, &unknown).unwrap_err();
        assert!(e.contains("no place 'nope'") && e.contains("[places]"), "{e}");
    }

    #[test]
    fn run_streams_output_and_reports_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = channel();
        let spec = RunSpec { id: "r1".into(), argv: v(&["sh", "-c", "echo out; echo err >&2; pwd; exit 3"]), ..Default::default() };
        let plan = Plan { cwd: dir.path().to_path_buf(), argv: spec.argv.clone(), shell: false };
        run(&spec, &plan, "pi", &tx);
        let last = rx.try_iter().last().unwrap();
        assert_eq!((last.phase.as_str(), last.exit_code, last.kind.as_str()), ("failed", Some(3), "run"));
        assert_eq!(last.lines, 3);
        assert!(last.tail.contains(&"out".to_string()) && last.tail.contains(&"err".to_string()));
        let pwd = dir.path().canonicalize().unwrap();
        assert!(last.tail.iter().any(|l| Path::new(l).canonicalize().ok().as_deref() == Some(&pwd)));
    }

    #[test]
    fn argv_is_not_shell_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = channel();
        let spec = RunSpec { id: "r2".into(), argv: v(&["echo", "$HOME; touch pwned"]), ..Default::default() };
        let plan = Plan { cwd: dir.path().to_path_buf(), argv: spec.argv.clone(), shell: false };
        run(&spec, &plan, "pi", &tx);
        let last = rx.try_iter().last().unwrap();
        assert_eq!(last.exit_code, Some(0));
        assert_eq!(last.tail, vec!["$HOME; touch pwned".to_string()]);
        assert!(!dir.path().join("pwned").exists());
    }

    #[test]
    fn a_relative_program_is_found_in_the_run_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        let script = dir.path().join("bin/v2.sh");
        std::fs::write(&script, "#!/bin/sh\necho \"v2 $*\"\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let (tx, rx) = channel();
        let spec = RunSpec { id: "r4".into(), argv: v(&["./bin/v2.sh", "verify", "S5"]), ..Default::default() };
        let plan = Plan { cwd: dir.path().to_path_buf(), argv: spec.argv.clone(), shell: false };
        run(&spec, &plan, "pi", &tx);
        let last = rx.try_iter().last().unwrap();
        assert_eq!((last.exit_code, last.tail.clone()), (Some(0), vec!["v2 verify S5".to_string()]));
    }

    #[test]
    fn timeout_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = channel();
        let spec = RunSpec { id: "r3".into(), argv: v(&["sleep", "30"]), timeout_secs: Some(1), ..Default::default() };
        let plan = Plan { cwd: dir.path().to_path_buf(), argv: spec.argv.clone(), shell: false };
        let t = Instant::now();
        run(&spec, &plan, "pi", &tx);
        assert!(t.elapsed() < Duration::from_secs(10));
        let last = rx.try_iter().last().unwrap();
        assert_eq!(last.exit_code, Some(TIMED_OUT));
        assert!(last.result.unwrap().contains("timed out"));
    }
}
