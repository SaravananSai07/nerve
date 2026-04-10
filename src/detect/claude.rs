use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::detect::process;
use crate::state::session::{Session, SessionState, TokenUsage};

#[derive(Deserialize)]
struct SessionFile {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
}

fn sessions_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("sessions"))
}

pub fn discover_sessions() -> Vec<Session> {
    let dir = match sessions_dir() {
        Some(d) if d.exists() => d,
        _ => return Vec::new(),
    };

    let procs = process::scan_processes();
    let child_map = process::build_child_map(&procs);
    let mut sessions = Vec::new();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Some(session) = load_session(&path, &procs, &child_map) {
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
    let content = fs::read_to_string(path).ok()?;
    let sf: SessionFile = serde_json::from_str(&content).ok()?;

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

    let cwd = PathBuf::from(&sf.cwd);
    let mut session = Session::new(sf.session_id.clone(), cwd.clone());
    session.pid = Some(sf.pid);

    session.tty = process::get_tty_for_pid(procs, sf.pid);
    session.cpu_percent = process::get_cpu_for_pid(procs, sf.pid);
    session.branch = detect_branch(&cwd);

    let jsonl_path = find_jsonl(&sf.session_id, &cwd);
    if let Some(ref jp) = jsonl_path {
        session.state = infer_state_from_jsonl(jp, sf.pid, procs, child_map);
        session.jsonl_path = Some(jp.clone());
        session.jsonl_age_secs = Some(file_age_secs(jp));
    } else {
        session.state = infer_state_from_cpu(session.cpu_percent);
    }

    if let SessionState::ToolRunning(ref tool) = session.state {
        session.current_tool = Some(tool.clone());
    }

    Some(session)
}

fn is_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

fn detect_branch(cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "rev-parse", "--abbrev-ref", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(branch)
    } else {
        None
    }
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
        if mtime_age <= 300.0
            || cpu > 5.0
            || process::has_child_named(procs, child_map, pid, "caffeinate")
        {
            return state;
        }
        return SessionState::Idle;
    }

    if cpu > 5.0 || process::has_child_named(procs, child_map, pid, "caffeinate") {
        return SessionState::Processing;
    }
    SessionState::Idle
}

fn infer_state_from_cpu(cpu: f32) -> SessionState {
    if cpu > 5.0 {
        SessionState::Processing
    } else {
        SessionState::Idle
    }
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
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let seek_pos = len.saturating_sub(8192);
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

    let file = match fs::File::open(path) {
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
    if trimmed.len() > 80 {
        format!("{}…", &trimmed[..80])
    } else {
        trimmed.to_string()
    }
}

pub fn read_tail_entries(path: &Path, max_entries: usize) -> Vec<LogEntry> {
    let mut file = match fs::File::open(path) {
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
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("unknown").to_string();
                        let detail = item
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
                        entries.push(LogEntry::ToolUse { name, detail });
                    }
                    "tool_result" => {
                        let is_err = item.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        let status = if is_err { "error".to_string() } else { "ok".to_string() };
                        let snippet = extract_tool_result_snippet(item);
                        entries.push(LogEntry::ToolResult { status, snippet });
                    }
                    "text" => {
                        let text = item
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !text.is_empty() {
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

pub fn read_session_pid(session_id: &str) -> Option<u32> {
    let dir = sessions_dir()?;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(sf) = serde_json::from_str::<SessionFile>(&content) {
                    if sf.session_id == session_id && is_alive(sf.pid) {
                        return Some(sf.pid);
                    }
                }
            }
        }
    }
    None
}

pub fn kill_session(pid: u32) -> Result<(), String> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|e| format!("failed to kill pid {pid}: {e}"))
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
