use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::detect::git::BranchCache;
use crate::detect::jsonl_cache::JsonlCache;
use crate::detect::{jsonl, process};
use crate::state::session::{DiscoverySnapshot, SessionId, SessionState};
use crate::state::token_usage::TokenUsage;
use crate::util::sanitize::Sanitised;

/// Cap on the size of a per-pid session JSON file. The format is small
/// (under 1 KiB in practice). Anything bigger is almost certainly garbage
/// or hostile content planted in `~/.claude/sessions/`.
const SESSION_JSON_MAX_BYTES: u64 = 64 * 1024;

/// Caps on `read_dir` enumeration of `~/.claude/{sessions,projects}/`.
/// Real users have at most a few hundred entries; tens of thousands is
/// a runaway-state signal (claude-code bug, accidental script, junk in
/// the directory). Hitting the cap is logged so the source can be
/// investigated.
const MAX_SESSION_ENTRIES: usize = 10_000;
const MAX_PROJECT_ENTRIES: usize = 10_000;

#[derive(Deserialize)]
struct SessionFile {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
    status: Option<String>,
}

fn sessions_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("sessions"))
}

/// CLI / one-shot path. Returns snapshots directly — `--dump` and
/// `--list` work with the worker's observation, not a synthesised
/// `Session` that would carry meaningless registry-owned fields
/// (state-machine confirmation counter, empty activity history,
/// zero-duration since "now").
pub(crate) fn discover_sessions() -> Vec<DiscoverySnapshot> {
    let table = process::ProcessTable::refreshed();
    let mut cache = JsonlCache::new();
    let mut branch_cache = BranchCache::new();
    discover_sessions_with(&table, &mut cache, &mut branch_cache)
}

/// Discover sessions against the cached process table, JSONL cache,
/// and branch cache. On an idle session this collapses to one
/// `stat(2)` per JSONL plus one `stat(2)` per HEAD — no reads, no
/// re-walks.
pub(crate) fn discover_sessions_with(
    table: &process::ProcessTable,
    cache: &mut JsonlCache,
    branch_cache: &mut BranchCache,
) -> Vec<DiscoverySnapshot> {
    let dir = match sessions_dir() {
        Some(d) if d.exists() => d,
        _ => return Vec::new(),
    };

    let mut sessions = Vec::new();
    let mut seen_jsonls: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut seen_cwds: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    // Enumerate `~/.claude/projects/` once per scan instead of
    // once per session — every cwd that didn't match the exact-
    // encoded path used to trigger its own read_dir.
    let project_dirs = list_project_dirs();

    for (i, entry) in entries.enumerate() {
        if i >= MAX_SESSION_ENTRIES {
            crate::log_warn!(
                "discovery: sessions dir exceeded {MAX_SESSION_ENTRIES} entries; stopping enumeration"
            );
            break;
        }
        // Log per-entry IO errors but don't bail the scan — one
        // corrupt per-pid JSON shouldn't blank the dashboard.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                crate::log_warn!("discovery: read_dir entry error: {e}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Some(session) = load_session(&path, table, cache, branch_cache, &project_dirs)
            {
                if let Some(ref jp) = session.jsonl_path {
                    seen_jsonls.insert(jp.clone());
                }
                seen_cwds.insert(session.cwd.clone());
                sessions.push(session);
            }
        }
    }

    // Evict cache entries whose JSONLs / cwds are no longer
    // referenced — bounds both caches to the live working set
    // instead of growing forever across days of uptime.
    cache.retain_present(|p| seen_jsonls.contains(p));
    branch_cache.retain_present(|p| seen_cwds.contains(p));

    deduplicate_sessions(sessions)
}

fn list_project_dirs() -> Vec<PathBuf> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let projects_dir = home.join(".claude").join("projects");
    let entries = match fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut result: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .take(MAX_PROJECT_ENTRIES + 1)
        .collect();
    if result.len() > MAX_PROJECT_ENTRIES {
        crate::log_warn!(
            "discovery: projects dir exceeded {MAX_PROJECT_ENTRIES} entries; truncating"
        );
        result.truncate(MAX_PROJECT_ENTRIES);
    }
    result
}

fn load_session(
    path: &Path,
    table: &process::ProcessTable,
    cache: &mut JsonlCache,
    branch_cache: &mut BranchCache,
    project_dirs: &[PathBuf],
) -> Option<DiscoverySnapshot> {
    let sf = read_session_file(path)?;

    // O(1) lookups via the cached pid index, rather than repeated
    // linear scans through the full process table.
    let proc = table.find_by_pid(sf.pid)?;
    let comm = proc.comm.rsplit('/').next().unwrap_or(&proc.comm);
    if comm != "claude" {
        return None;
    }

    // Filter out daemon-spawned claude processes (e.g. background Go binaries
    // that invoke `claude` repeatedly). Real interactive sessions either have a
    // real TTY or are launched from a shell / terminal multiplexer.
    if proc.tty == "??" || proc.tty == "?" {
        let parent_is_shell = table.find_by_pid(proc.ppid).is_some_and(|p| {
            let name = p.comm.rsplit('/').next().unwrap_or(&p.comm);
            matches!(name, "zsh" | "bash" | "fish" | "sh" | "dash" | "csh" | "tcsh"
                         | "nu" | "tmux" | "screen")
        });
        if !parent_is_shell {
            return None;
        }
    }

    // Claude Code rewrites the per-PID session file's sessionId after a
    // --resume, but the actual transcript JSONL keeps the original id. Trust
    // the command line as the source of truth — both for finding the JSONL and
    // as nerve's canonical session id, so the registry isn't churned every
    // time Claude rewrites sf.session_id mid-run.
    //
    // Both inputs are validated through the same charset/traversal gate.
    // An attacker who can plant a session JSON file with `"sessionId":
    // "../../etc/passwd"` would otherwise drive `find_jsonl` into a
    // traversal probe; drop the session instead.
    let resolved_id = process::resume_session_id(&proc.args)
        .or_else(|| process::valid_session_id(sf.session_id.as_str()))?
        .to_string();

    let cwd = PathBuf::from(&sf.cwd);
    let name = Sanitised::new(
        cwd.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into()),
    );
    let branch = branch_cache.read_or_refresh(&cwd).map(Sanitised::new);

    let jsonl_path = find_jsonl(&resolved_id, &cwd, project_dirs);
    let (detected, usage, jsonl_age_secs) = if let Some(ref jp) = jsonl_path {
        let (tail_state, usage) = cache.read_or_refresh(
            jp,
            |p| jsonl::read_tail_state(p).unwrap_or(SessionState::Idle),
            jsonl::parse_token_usage,
        );
        let refined = refine_with_runtime(tail_state, jp, sf.pid, table);
        (refined, usage, Some(file_age_secs(jp)))
    } else {
        // No transcript to read. Trust Claude's own status field if it set one;
        // otherwise default to Idle. CPU is deliberately not used — Claude's
        // TUI burns CPU on keystroke rendering, which would otherwise flip an
        // idle session to Processing whenever the user is typing.
        let detected = match sf.status.as_deref() {
            Some("busy") => SessionState::Processing,
            _ => SessionState::Idle,
        };
        (detected, TokenUsage::default(), None)
    };

    let current_tool = match &detected {
        SessionState::ToolRunning(tool) => Some(Sanitised::new(tool.clone())),
        _ => None,
    };

    Some(DiscoverySnapshot {
        id: SessionId::new(resolved_id),
        cwd,
        name,
        tty: Some(Sanitised::new(proc.tty.clone())),
        branch,
        cpu_percent: proc.cpu,
        pid: Some(sf.pid),
        detected_state: detected,
        current_tool,
        usage,
        jsonl_path,
        jsonl_age_secs,
    })
}

fn read_session_file(path: &Path) -> Option<SessionFile> {
    // Same O_NOFOLLOW guard we use for JSONL transcripts.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut buf = String::new();
    // Cap the read so a planted multi-GB file can't drive the discovery
    // path into OOM. serde will reject anything truncated.
    file.take(SESSION_JSON_MAX_BYTES)
        .read_to_string(&mut buf)
        .ok()?;
    serde_json::from_str(&buf).ok()
}

fn find_jsonl(session_id: &str, cwd: &Path, project_dirs: &[PathBuf]) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return None;
    }

    let expected_dir = cwd.to_string_lossy().replace('/', "-");
    let jsonl_name = format!("{session_id}.jsonl");

    // Exact match: try the path Claude Code would naturally write to
    // before walking the fallback list.
    let exact = projects_dir.join(&expected_dir).join(&jsonl_name);
    if exact.exists() {
        return Some(exact);
    }

    // Fallback: caller has already listed the project subdirs (once
    // per scan, not once per session) — just stat each candidate.
    for dir in project_dirs {
        let candidate = dir.join(&jsonl_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

/// Apply the runtime conditions (CPU%, caffeinate child, mtime age)
/// to a tail-parse result. Pure: doesn't touch the file system aside
/// from the cheap `mtime` call.
fn refine_with_runtime(
    tail_parse: SessionState,
    path: &Path,
    pid: u32,
    table: &process::ProcessTable,
) -> SessionState {
    let mtime_age = file_age_secs(path);
    let cpu = table.find_by_pid(pid).map(|p| p.cpu).unwrap_or(0.0);

    let state = tail_parse;
    if matches!(state, SessionState::Idle | SessionState::Error) {
        return state;
    }
    if state == SessionState::WaitingForInput {
        if mtime_age <= 172_800.0 || cpu > 1.0 {
            return state;
        }
        return SessionState::Dormant;
    }
    // Stick with the tail-derived Processing state only when there's
    // independent evidence the session is still doing work: a recent JSONL
    // write (Claude writes on every roundtrip / tool result) or a
    // caffeinate child (long-running task holding the system awake).
    // CPU is deliberately NOT used here — Claude's TUI burns CPU on
    // keystroke rendering, which would otherwise flip an idle session to
    // Processing whenever the user is typing.
    if mtime_age <= 300.0
        || process::has_child_named(table.procs(), table.child_map(), pid, "caffeinate")
    {
        return state;
    }
    SessionState::Idle
}

fn file_age_secs(path: &Path) -> f64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(f64::MAX)
}

/// Resolve a session id to its pid AND re-validate that the pid is
/// still a `claude` process, then send SIGTERM — all in the same
/// function. Between a separate resolve-then-kill the kernel could
/// recycle the pid to an unrelated user process and we'd SIGTERM
/// that instead.
pub(crate) fn kill_by_session_id(session_id: &str) -> Result<u32, String> {
    let dir = sessions_dir().ok_or_else(|| "no sessions dir".to_string())?;
    let entries = fs::read_dir(&dir).map_err(|e| e.to_string())?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let sf = match read_session_file(&path) {
            Some(s) => s,
            None => continue,
        };
        if sf.session_id != session_id {
            continue;
        }

        if !is_claude_process(sf.pid) {
            return Err(format!(
                "pid {} is no longer a claude process (likely already exited)",
                sf.pid
            ));
        }
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(sf.pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .map_err(|e| format!("kill pid {}: {e}", sf.pid))?;
        return Ok(sf.pid);
    }
    Err("session id not found in ~/.claude/sessions/".to_string())
}

fn is_claude_process(pid: u32) -> bool {
    process::scan_processes().iter().any(|p| {
        p.pid == pid && p.comm.rsplit('/').next().unwrap_or(&p.comm) == "claude"
    })
}

fn should_replace(existing: &DiscoverySnapshot, candidate: &DiscoverySnapshot) -> bool {
    candidate.detected_state.sort_priority() < existing.detected_state.sort_priority()
        || (candidate.detected_state.sort_priority() == existing.detected_state.sort_priority()
            && candidate.pid > existing.pid)
}

fn deduplicate_sessions(sessions: Vec<DiscoverySnapshot>) -> Vec<DiscoverySnapshot> {
    // Phase 1: Deduplicate by PID — multiple session files for the same
    // Claude process collapse into the one with the best state.
    let mut by_pid: HashMap<u32, DiscoverySnapshot> = HashMap::new();
    let mut no_pid: Vec<DiscoverySnapshot> = Vec::new();
    for snap in sessions {
        if let Some(pid) = snap.pid {
            by_pid
                .entry(pid)
                .and_modify(|existing| {
                    if should_replace(existing, &snap) {
                        *existing = snap.clone();
                    }
                })
                .or_insert(snap);
        } else {
            no_pid.push(snap);
        }
    }

    let pid_deduped: Vec<DiscoverySnapshot> = by_pid.into_values().chain(no_pid).collect();

    // Phase 2: Deduplicate by TTY — multiple processes on the same terminal
    // collapse into the most active one.
    let mut by_tty: HashMap<String, DiscoverySnapshot> = HashMap::new();
    for snap in pid_deduped {
        let tty = match &snap.tty {
            Some(t) if t.as_str() != "??" && t.as_str() != "?" => t.as_str().to_string(),
            _ => format!("__notty_{}", snap.id),
        };
        by_tty
            .entry(tty)
            .and_modify(|existing| {
                if should_replace(existing, &snap) {
                    *existing = snap.clone();
                }
            })
            .or_insert(snap);
    }

    by_tty.into_values().collect()
}

