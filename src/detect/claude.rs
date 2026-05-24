use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::detect::process;
use crate::state::session::{Session, SessionId, SessionState, TokenUsage};
use crate::util::sanitize::strip_ansi;

/// Cap on the size of a per-pid session JSON file. The format is small
/// (under 1 KiB in practice). Anything bigger is almost certainly garbage
/// or hostile content planted in `~/.claude/sessions/`.
const SESSION_JSON_MAX_BYTES: u64 = 64 * 1024;

/// Open a JSONL transcript file with `O_NOFOLLOW`. Defends against an
/// attacker swapping a final-component symlink between our exists()-check
/// in `find_jsonl` and the actual open call (S13). Multi-component
/// directory-level traversal is out of scope — `~/.claude/projects/` is
/// user-owned, so cross-user attacks already require a prior compromise.
fn open_jsonl(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
}

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

pub fn discover_sessions() -> Vec<Session> {
    // CLI / one-shot path: take a fresh process snapshot every call.
    // The TUI hot path goes through `discover_sessions_with`.
    let table = process::ProcessTable::refreshed();
    discover_sessions_with(&table)
}

/// Discovery against a (possibly cached) process table. The App holds
/// a `ProcessTable` that it refreshes only every
/// `process_scan_interval_ms`, so a 1 Hz refresh tick no longer pays
/// the `ps -eo` fork cost on every iteration (closes A7).
pub fn discover_sessions_with(table: &process::ProcessTable) -> Vec<Session> {
    let dir = match sessions_dir() {
        Some(d) if d.exists() => d,
        _ => return Vec::new(),
    };

    let mut sessions = Vec::new();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Some(session) = load_session(&path, table.procs(), table.child_map()) {
                sessions.push(session);
            }
        }
    }

    deduplicate_sessions(sessions)
}

fn load_session(
    path: &Path,
    procs: &[process::ProcessInfo],
    child_map: &HashMap<u32, Vec<u32>>,
) -> Option<Session> {
    let sf = read_session_file(path)?;

    let proc = process::find_process(procs, sf.pid)?;
    let comm = proc.comm.rsplit('/').next().unwrap_or(&proc.comm);
    if comm != "claude" {
        return None;
    }

    // Filter out daemon-spawned claude processes (e.g. background Go binaries
    // that invoke `claude` repeatedly). Real interactive sessions either have a
    // real TTY or are launched from a shell / terminal multiplexer.
    if proc.tty == "??" || proc.tty == "?" {
        let parent_is_shell = process::find_process(procs, proc.ppid).is_some_and(|p| {
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
    let resolved_id = process::resume_session_id(&proc.args)
        .unwrap_or(sf.session_id.as_str())
        .to_string();

    let cwd = PathBuf::from(&sf.cwd);
    let session_id = SessionId::new(resolved_id.clone());
    let mut session = Session::new(session_id, cwd.clone());
    session.pid = Some(sf.pid);

    session.tty = process::get_tty_for_pid(procs, sf.pid);
    session.cpu_percent = process::get_cpu_for_pid(procs, sf.pid);
    session.branch = crate::detect::git::read_branch(&cwd);

    let jsonl_path = find_jsonl(&resolved_id, &cwd);
    if let Some(ref jp) = jsonl_path {
        session.state = infer_state_from_jsonl(jp, sf.pid, procs, child_map);
        session.jsonl_path = Some(jp.clone());
        session.jsonl_age_secs = Some(file_age_secs(jp));
    } else {
        // No transcript to read. Trust Claude's own status field if it set one;
        // otherwise default to Idle. CPU is deliberately not used — Claude's
        // TUI burns CPU on keystroke rendering, which would otherwise flip an
        // idle session to Processing whenever the user is typing.
        session.state = match sf.status.as_deref() {
            Some("busy") => SessionState::Processing,
            _ => SessionState::Idle,
        };
    }

    if let SessionState::ToolRunning(ref tool) = session.state {
        session.current_tool = Some(tool.clone());
    }

    Some(session)
}

fn read_session_file(path: &Path) -> Option<SessionFile> {
    // O_NOFOLLOW guards the per-pid session JSON the same way we guard
    // JSONL transcripts (S13).
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

fn find_jsonl(session_id: &str, cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return None;
    }

    let expected_dir = cwd.to_string_lossy().replace('/', "-");
    let jsonl_name = format!("{session_id}.jsonl");

    let exact = projects_dir.join(&expected_dir).join(&jsonl_name);
    if exact.exists() {
        return Some(exact);
    }

    for entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        if entry.file_type().ok()?.is_dir() {
            let jsonl = entry.path().join(&jsonl_name);
            if jsonl.exists() {
                return Some(jsonl);
            }
        }
    }

    None
}

pub fn infer_state_from_jsonl(
    path: &Path,
    pid: u32,
    procs: &[process::ProcessInfo],
    child_map: &HashMap<u32, Vec<u32>>,
) -> SessionState {
    let mtime_age = file_age_secs(path);
    let cpu = process::get_cpu_for_pid(procs, pid);

    if let Some(state) = read_tail_state(path) {
        if matches!(state, SessionState::Idle | SessionState::Error) {
            return state;
        }
        if state == SessionState::WaitingForInput {
            if mtime_age <= 172_800.0 || cpu > 1.0 {
                return state;
            }
            return SessionState::Stale;
        }
        // Stick with the tail-derived Processing state only when there's
        // independent evidence the session is still doing work: a recent JSONL
        // write (Claude writes on every roundtrip / tool result) or a
        // caffeinate child (long-running task holding the system awake).
        // CPU is deliberately NOT used here — Claude's TUI burns CPU on
        // keystroke rendering, which would otherwise flip an idle session to
        // Processing whenever the user is typing.
        if mtime_age <= 300.0
            || process::has_child_named(procs, child_map, pid, "caffeinate")
        {
            return state;
        }
        return SessionState::Idle;
    }

    // No state-bearing entry found in the tail. With Claude Code 2.x the JSONL
    // is padded with metadata types (file-history-snapshot, last-prompt, etc.)
    // that can crowd out real state at the tail; treating that as Processing
    // misfires whenever the user is typing and the Claude TUI is burning CPU.
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

fn read_tail_state(path: &Path) -> Option<SessionState> {
    let mut file = open_jsonl(path).ok()?;
    let len = file.metadata().ok()?.len();

    // Claude Code 2.x writes large entries that we skip past:
    // - file-history-snapshot: ~22KB each
    // - assistant entries with extended thinking: 50–100KB+
    // The tail window has to step past at least one of each, so we read a
    // generous slice rather than try to parse the whole file.
    let seek_pos = len.saturating_sub(262_144);
    file.seek(SeekFrom::Start(seek_pos)).ok()?;

    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;

    buf.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find_map(parse_jsonl_state)
}

fn parse_jsonl_state(line: &str) -> Option<SessionState> {
    let val: serde_json::Value = serde_json::from_str(line).ok()?;

    let entry_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");

    if entry_type == "result" {
        if val.get("subtype").and_then(|s| s.as_str()) == Some("error") {
            return Some(SessionState::Error);
        }
        return Some(SessionState::Idle);
    }

    if entry_type == "system" {
        return None;
    }

    if matches!(entry_type, "progress" | "agent_progress" | "hook_progress") {
        return Some(SessionState::Processing);
    }

    let role = val
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or("");

    if role == "assistant" {
        if let Some(content) = val.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
            for item in content {
                if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                    if name == "AskUserQuestion" || name == "ExitPlanMode" {
                        return Some(SessionState::WaitingForInput);
                    }
                    return Some(SessionState::ToolRunning(name.to_string()));
                }
            }
        }

        let stop_reason = val
            .get("message")
            .and_then(|m| m.get("stop_reason"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        return Some(match stop_reason {
            "end_turn" | "max_tokens" | "stop_sequence" => SessionState::Idle,
            _ => SessionState::Processing,
        });
    }

    if role == "user" {
        if let Some(content) = val.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
            for item in content {
                if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    return Some(SessionState::Processing);
                }
            }
        }
        return Some(SessionState::Processing);
    }

    None
}

fn cost_per_million(model: &str) -> (f64, f64, f64, f64) {
    if model.contains("opus") {
        (15.0, 75.0, 1.50, 18.75)
    } else if model.contains("haiku") {
        (0.80, 4.0, 0.08, 1.0)
    } else {
        (3.0, 15.0, 0.30, 3.75)
    }
}

pub fn parse_token_usage(path: &Path, from_offset: u64) -> (TokenUsage, u64) {
    let mut usage = TokenUsage::default();

    let file = match open_jsonl(path) {
        Ok(f) => f,
        Err(_) => return (usage, from_offset),
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    if file_len < from_offset {
        return parse_token_usage(path, 0);
    }

    let mut reader = std::io::BufReader::new(file);
    if from_offset > 0 {
        if reader.seek(SeekFrom::Start(from_offset)).is_err() {
            return (usage, from_offset);
        }
        let mut skip = [0u8; 1];
        loop {
            match std::io::Read::read(&mut reader, &mut skip) {
                Ok(0) => break,
                Ok(_) => {
                    if skip[0] == b'\n' {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    } else if reader.seek(SeekFrom::Start(0)).is_err() {
        return (usage, from_offset);
    }

    let mut line = String::new();
    loop {
        line.clear();
        match std::io::BufRead::read_line(&mut reader, &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type != "assistant" {
            continue;
        }

        let msg = val.get("message");
        if let Some(u) = msg.and_then(|m| m.get("usage")) {
            let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_read = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_creation = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

            let model = msg
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .unwrap_or("sonnet");

            let (rate_in, rate_out, rate_cache_read, rate_cache_create) = cost_per_million(model);

            usage.input_tokens += input;
            usage.output_tokens += output;
            usage.cache_read_tokens += cache_read;
            usage.cache_creation_tokens += cache_creation;
            usage.cost_usd += (input as f64 * rate_in
                + output as f64 * rate_out
                + cache_read as f64 * rate_cache_read
                + cache_creation as f64 * rate_cache_create)
                / 1_000_000.0;
        }
    }

    let new_offset = reader.stream_position().unwrap_or(file_len);
    (usage, new_offset)
}

#[derive(Debug, Clone)]
pub enum LogEntry {
    UserText(String),
    AssistantText(String),
    ToolUse { name: String, detail: String },
    ToolResult { status: String, snippet: String },
    Result { is_error: bool },
}

fn extract_tool_result_snippet(item: &serde_json::Value) -> String {
    let content = match item.get("content") {
        Some(c) => c,
        None => return String::new(),
    };

    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        arr.iter()
            .filter_map(|v| {
                if v.get("type").and_then(|t| t.as_str()) == Some("text") {
                    v.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .next()
            .unwrap_or_default()
    } else {
        return String::new();
    };

    let first_line = text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let trimmed = first_line.trim();
    let truncated = if trimmed.len() > 80 {
        format!("{}…", &trimmed[..80])
    } else {
        trimmed.to_string()
    };
    // Strip ANSI / OSC / C0+C1 controls before the snippet reaches the TUI.
    // Defends against an untrusted JSONL painting the host terminal via
    // escape sequences (cursor jumps, OSC 52 clipboard writes, etc.).
    strip_ansi(&truncated).into_owned()
}

pub fn read_tail_entries(path: &Path, max_entries: usize) -> Vec<LogEntry> {
    let mut file = match open_jsonl(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let seek_pos = len.saturating_sub(65536);
    if file.seek(SeekFrom::Start(seek_pos)).is_err() {
        return Vec::new();
    }

    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }

    let mut entries = Vec::new();
    for line in buf.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type == "progress" || entry_type == "file-history-snapshot" {
            continue;
        }

        if entry_type == "result" {
            let is_error = val.get("subtype").and_then(|s| s.as_str()) == Some("error");
            entries.push(LogEntry::Result { is_error });
            continue;
        }

        let role = val
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");

        if let Some(content) = val.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
            for item in content {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match item_type {
                    "tool_use" => {
                        let raw_name =
                            item.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                        let name = strip_ansi(raw_name).into_owned();
                        let raw_detail: String = item
                            .get("input")
                            .and_then(|i| {
                                i.get("command")
                                    .or_else(|| i.get("file_path"))
                                    .or_else(|| i.get("pattern"))
                                    .or_else(|| i.get("description"))
                                    .or_else(|| {
                                        i.get("questions")
                                            .and_then(|q| q.as_array())
                                            .and_then(|q| q.first())
                                            .and_then(|q| q.get("question"))
                                    })
                                    .or_else(|| i.get("query"))
                                    .or_else(|| i.get("skill"))
                                    .and_then(|v| v.as_str())
                            })
                            .unwrap_or("")
                            .chars()
                            .take(200)
                            .collect();
                        let detail = strip_ansi(&raw_detail).into_owned();
                        entries.push(LogEntry::ToolUse { name, detail });
                    }
                    "tool_result" => {
                        let is_err = item.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        let status = if is_err { "error".to_string() } else { "ok".to_string() };
                        let snippet = extract_tool_result_snippet(item);
                        entries.push(LogEntry::ToolResult { status, snippet });
                    }
                    "text" => {
                        let raw = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if !raw.is_empty() {
                            // Sanitize at the boundary so the TUI never sees
                            // raw ESC / OSC bytes coming from an untrusted
                            // JSONL transcript.
                            let text = strip_ansi(raw).into_owned();
                            if role == "user" {
                                entries.push(LogEntry::UserText(text));
                            } else if role == "assistant" {
                                entries.push(LogEntry::AssistantText(text));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let skip = entries.len().saturating_sub(max_entries);
    entries.into_iter().skip(skip).collect()
}

/// Resolve a session id to its pid AND atomically (within the same function
/// scope) re-validate that the pid is still a `claude` process before
/// sending SIGTERM. This closes the pid-reuse TOCTOU (S4 / L19): between
/// a separate resolve-then-kill, the kernel could have recycled the pid
/// to an unrelated user process and we'd SIGTERM that instead.
pub fn kill_by_session_id(session_id: &str) -> Result<u32, String> {
    let dir = sessions_dir().ok_or_else(|| "no sessions dir".to_string())?;
    let entries = fs::read_dir(&dir).map_err(|e| e.to_string())?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "json") {
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

fn should_replace(existing: &Session, candidate: &Session) -> bool {
    candidate.state.sort_priority() < existing.state.sort_priority()
        || (candidate.state.sort_priority() == existing.state.sort_priority()
            && candidate.pid > existing.pid)
}

fn deduplicate_sessions(sessions: Vec<Session>) -> Vec<Session> {
    // Phase 1: Deduplicate by PID — multiple session files for the same
    // Claude process collapse into the one with the best state.
    let mut by_pid: HashMap<u32, Session> = HashMap::new();
    let mut no_pid: Vec<Session> = Vec::new();
    for session in sessions {
        if let Some(pid) = session.pid {
            by_pid
                .entry(pid)
                .and_modify(|existing| {
                    if should_replace(existing, &session) {
                        *existing = session.clone();
                    }
                })
                .or_insert(session);
        } else {
            no_pid.push(session);
        }
    }

    let pid_deduped: Vec<Session> = by_pid.into_values().chain(no_pid).collect();

    // Phase 2: Deduplicate by TTY — multiple processes on the same terminal
    // collapse into the most active one.
    let mut by_tty: HashMap<String, Session> = HashMap::new();
    for session in pid_deduped {
        let tty = match &session.tty {
            Some(t) if t != "??" && t != "?" => t.clone(),
            _ => format!("__notty_{}", session.id),
        };
        by_tty
            .entry(tty)
            .and_modify(|existing| {
                if should_replace(existing, &session) {
                    *existing = session.clone();
                }
            })
            .or_insert(session);
    }

    by_tty.into_values().collect()
}
