use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::detect::process;
use crate::state::session::{Session, SessionState};

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

    disambiguate_names(&mut sessions);
    sessions
}

fn load_session(
    path: &Path,
    procs: &[process::ProcessInfo],
    child_map: &HashMap<u32, Vec<u32>>,
) -> Option<Session> {
    let content = fs::read_to_string(path).ok()?;
    let sf: SessionFile = serde_json::from_str(&content).ok()?;

    if !is_alive(sf.pid) {
        return None;
    }

    let cwd = PathBuf::from(&sf.cwd);
    let mut session = Session::new(sf.session_id.clone(), cwd.clone());

    session.tty = process::get_tty_for_pid(procs, sf.pid);
    session.cpu_percent = process::get_cpu_for_pid(procs, sf.pid);
    session.branch = detect_branch(&cwd);

    let jsonl_path = find_jsonl(&sf.session_id, &cwd);
    if let Some(ref jp) = jsonl_path {
        session.state = infer_state_from_jsonl(jp, sf.pid, procs, child_map);
        session.jsonl_path = Some(jp.clone());
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

    if mtime_age > 300.0 {
        if cpu > 5.0 || process::has_child_named(procs, child_map, pid, "caffeinate") {
            return SessionState::Processing;
        }
        return SessionState::Idle;
    }

    if let Some(last_entry) = read_tail_jsonl(path) {
        if let Some(state) = parse_jsonl_state(&last_entry) {
            return state;
        }
    }

    if process::has_child_named(procs, child_map, pid, "caffeinate") {
        return SessionState::Processing;
    }

    infer_state_from_cpu(cpu)
}

fn infer_state_from_cpu(cpu: f32) -> SessionState {
    if cpu > 5.0 {
        SessionState::Processing
    } else if cpu > 0.5 {
        SessionState::WaitingForInput
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

fn read_tail_jsonl(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let seek_pos = if len > 8192 { len - 8192 } else { 0 };
    file.seek(SeekFrom::Start(seek_pos)).ok()?;

    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;

    buf.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find(|l| {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                t != "progress" && t != "file-history-snapshot"
            } else {
                false
            }
        })
        .map(|s| s.to_string())
}

fn parse_jsonl_state(line: &str) -> Option<SessionState> {
    let val: serde_json::Value = serde_json::from_str(line).ok()?;

    let entry_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let role = val
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or("");

    if entry_type == "result" {
        if val.get("subtype").and_then(|s| s.as_str()) == Some("error") {
            return Some(SessionState::Error);
        }
        return Some(SessionState::WaitingForInput);
    }

    if role == "assistant" {
        if let Some(content) = val.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
            for item in content {
                if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                    return Some(SessionState::ToolRunning(name.to_string()));
                }
            }
        }
        return Some(SessionState::WaitingForInput);
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

fn disambiguate_names(sessions: &mut [Session]) {
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for s in sessions.iter() {
        *name_counts.entry(s.name.clone()).or_default() += 1;
    }

    let mut name_indices: HashMap<String, usize> = HashMap::new();
    for s in sessions.iter_mut() {
        if name_counts.get(&s.name).copied().unwrap_or(0) > 1 {
            let idx = name_indices.entry(s.name.clone()).or_insert(0);
            *idx += 1;
            s.name = format!("{} ({})", s.name, idx);
        }
    }
}
