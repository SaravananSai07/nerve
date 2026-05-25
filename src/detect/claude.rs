use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::detect::git::BranchCache;
use crate::detect::jsonl_cache::JsonlCache;
use crate::detect::process;
use crate::state::log_entry::LogEntry;
use crate::state::session::{DiscoverySnapshot, Session, SessionId, SessionState, TokenUsage};
use crate::util::sanitize::strip_ansi;

/// Cap on the size of a per-pid session JSON file. The format is small
/// (under 1 KiB in practice). Anything bigger is almost certainly garbage
/// or hostile content planted in `~/.claude/sessions/`.
const SESSION_JSON_MAX_BYTES: u64 = 64 * 1024;

/// Open a JSONL transcript file with `O_NOFOLLOW`. Defends against
/// an attacker swapping a final-component symlink between our
/// `exists()` check in `find_jsonl` and the actual open. Multi-
/// component directory-level traversal is out of scope —
/// `~/.claude/projects/` is user-owned, so any cross-user attack
/// already presupposes a prior compromise.
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

/// CLI / one-shot path. Materialises each snapshot into a Session so
/// the JSON wire schema produced by `--dump` and `--list` stays
/// stable. The TUI hot path consumes snapshots directly through
/// `discover_sessions_with`.
pub fn discover_sessions() -> Vec<Session> {
    let table = process::ProcessTable::refreshed();
    let mut cache = JsonlCache::new();
    let mut branch_cache = BranchCache::new();
    discover_sessions_with(&table, &mut cache, &mut branch_cache)
        .into_iter()
        .map(Session::from_snapshot)
        .collect()
}

/// Discover sessions against the cached process table, JSONL cache,
/// and branch cache. On an idle session this collapses to one
/// `stat(2)` per JSONL plus one `stat(2)` per HEAD — no reads, no
/// re-walks.
pub fn discover_sessions_with(
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

    for entry in entries {
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
    match fs::read_dir(&projects_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    }
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
    let name = cwd
        .file_name()
        .map(|n| strip_ansi(&n.to_string_lossy()).into_owned())
        .unwrap_or_else(|| "unknown".into());
    let branch = branch_cache.read_or_refresh(&cwd);

    let jsonl_path = find_jsonl(&resolved_id, &cwd, project_dirs);
    let (detected, usage, jsonl_age_secs) = if let Some(ref jp) = jsonl_path {
        let (tail_state, usage) = cache.read_or_refresh(
            jp,
            |p| read_tail_state(p).unwrap_or(SessionState::Idle),
            parse_token_usage,
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
        SessionState::ToolRunning(tool) => Some(tool.clone()),
        _ => None,
    };

    Some(DiscoverySnapshot {
        id: SessionId::new(resolved_id),
        cwd,
        name,
        tty: Some(proc.tty.clone()),
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

/// Lightweight schema for the subset of JSONL fields we read when
/// inferring a session's state from its tail. Deriving over a thin
/// borrowed struct is ~10× faster than building a full
/// `serde_json::Value` tree per line and avoids the per-line
/// `HashMap<String, Value>` allocation.
#[derive(Deserialize)]
struct StateRow<'a> {
    #[serde(default, borrow, rename = "type")]
    entry_type: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    subtype: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    message: Option<StateMessage<'a>>,
}

#[derive(Deserialize)]
struct StateMessage<'a> {
    #[serde(default, borrow)]
    role: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow, rename = "stop_reason")]
    stop_reason: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    content: Option<Vec<StateContentItem<'a>>>,
}

#[derive(Deserialize)]
struct StateContentItem<'a> {
    #[serde(default, borrow, rename = "type")]
    item_type: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    name: Option<std::borrow::Cow<'a, str>>,
}

fn parse_jsonl_state(line: &str) -> Option<SessionState> {
    let row: StateRow = serde_json::from_str(line).ok()?;
    let entry_type = row.entry_type.as_deref().unwrap_or("");

    if entry_type == "result" {
        if row.subtype.as_deref() == Some("error") {
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

    let msg = row.message.as_ref()?;
    let role = msg.role.as_deref().unwrap_or("");

    if role == "assistant" {
        if let Some(content) = msg.content.as_ref() {
            for item in content {
                if item.item_type.as_deref() == Some("tool_use") {
                    let raw_name = item.name.as_deref().unwrap_or("unknown");
                    // JSONL is attacker-influenced; sanitise before any
                    // comparison so a name like "AskUserQuestion\x1b[2J"
                    // doesn't fall through with the escape attached.
                    let name = strip_ansi(raw_name).into_owned();
                    if name == "AskUserQuestion" || name == "ExitPlanMode" {
                        return Some(SessionState::WaitingForInput);
                    }
                    return Some(SessionState::ToolRunning(name));
                }
            }
        }
        return Some(match msg.stop_reason.as_deref().unwrap_or("") {
            "end_turn" | "max_tokens" | "stop_sequence" => SessionState::Idle,
            _ => SessionState::Processing,
        });
    }

    if role == "user" {
        if let Some(content) = msg.content.as_ref() {
            for item in content {
                if item.item_type.as_deref() == Some("tool_result") {
                    return Some(SessionState::Processing);
                }
            }
        }
        return Some(SessionState::Processing);
    }

    None
}

/// Mirror of the `assistant`-row schema used by `parse_token_usage`.
/// Like `StateRow` above, borrowed-Cow fields keep the per-line
/// parse cost dominated by JSON tokenisation rather than allocation.
#[derive(Deserialize)]
struct UsageRow<'a> {
    #[serde(default, borrow, rename = "type")]
    entry_type: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    message: Option<UsageMessage<'a>>,
}

#[derive(Deserialize)]
struct UsageMessage<'a> {
    #[serde(default, borrow)]
    model: Option<std::borrow::Cow<'a, str>>,
    #[serde(default)]
    usage: Option<UsageBlock>,
}

#[derive(Deserialize, Default)]
struct UsageBlock {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Copy, Clone, PartialEq)]
enum ModelClass {
    Opus,
    Haiku,
    Other,
}

impl ModelClass {
    fn classify(model: &str) -> Self {
        if model.contains("opus") {
            Self::Opus
        } else if model.contains("haiku") {
            Self::Haiku
        } else {
            Self::Other
        }
    }

    /// (input, output, cache_read, cache_create) — dollars per
    /// million tokens.
    fn rate(self) -> (f64, f64, f64, f64) {
        match self {
            Self::Opus => (15.0, 75.0, 1.50, 18.75),
            Self::Haiku => (0.80, 4.0, 0.08, 1.0),
            Self::Other => (3.0, 15.0, 0.30, 3.75),
        }
    }
}

fn parse_token_usage(path: &Path, from_offset: u64) -> (TokenUsage, u64) {
    let usage = TokenUsage::default();

    let file = match open_jsonl(path) {
        Ok(f) => f,
        Err(_) => return (usage, from_offset),
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    if file_len < from_offset {
        // File was truncated/rotated under us. Re-parse from offset 0
        // *iteratively* (recursion previously meant an arbitrary stack
        // for a 50 MB JSONL); the call below is safe because we know
        // `file_len >= 0` makes the next `file_len < 0` impossible.
        return parse_token_usage_inner(path, 0);
    }
    parse_token_usage_inner(path, from_offset)
}

fn parse_token_usage_inner(path: &Path, from_offset: u64) -> (TokenUsage, u64) {
    let mut usage = TokenUsage::default();

    let file = match open_jsonl(path) {
        Ok(f) => f,
        Err(_) => return (usage, from_offset),
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

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

    // Memoize the per-line model classification: most JSONLs use
    // one model end-to-end, so a single substring scan up front
    // covers every subsequent line.
    let mut last_class: Option<ModelClass> = None;
    let mut last_rate = ModelClass::Other.rate();

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
        let row: UsageRow = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(_) => continue,
        };

        if row.entry_type.as_deref() != Some("assistant") {
            continue;
        }

        let Some(msg) = row.message.as_ref() else { continue };
        if let Some(u) = msg.usage.as_ref() {
            let input = u.input_tokens.unwrap_or(0);
            let output = u.output_tokens.unwrap_or(0);
            let cache_read = u.cache_read_input_tokens.unwrap_or(0);
            let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);

            let model = msg.model.as_deref().unwrap_or("sonnet");
            let class = ModelClass::classify(model);
            if last_class != Some(class) {
                last_rate = class.rate();
                last_class = Some(class);
            }
            let (rate_in, rate_out, rate_cache_read, rate_cache_create) = last_rate;

            // JSONL is untrusted: cap any single row's token claim so an
            // adversarial line claiming `u64::MAX` can't panic in debug or
            // produce `inf`/`NaN` cost in release.
            const PER_ROW_TOKEN_CAP: u64 = 10_000_000;
            let input = input.min(PER_ROW_TOKEN_CAP);
            let output = output.min(PER_ROW_TOKEN_CAP);
            let cache_read = cache_read.min(PER_ROW_TOKEN_CAP);
            let cache_creation = cache_creation.min(PER_ROW_TOKEN_CAP);

            usage.input_tokens = usage.input_tokens.saturating_add(input);
            usage.output_tokens = usage.output_tokens.saturating_add(output);
            usage.cache_read_tokens = usage.cache_read_tokens.saturating_add(cache_read);
            usage.cache_creation_tokens = usage.cache_creation_tokens.saturating_add(cache_creation);
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
    // Truncate by chars, not bytes — slicing at byte 80 would panic on
    // any codepoint straddling the boundary (e.g. a CJK glyph or emoji
    // in a tool result), which a hostile JSONL row could trigger
    // deterministically.
    let mut truncated: String = trimmed.chars().take(80).collect();
    if trimmed.chars().count() > 80 {
        truncated.push('…');
    }
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

/// Resolve a session id to its pid AND re-validate that the pid is
/// still a `claude` process, then send SIGTERM — all in the same
/// function. Between a separate resolve-then-kill the kernel could
/// recycle the pid to an unrelated user process and we'd SIGTERM
/// that instead.
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
            Some(t) if t != "??" && t != "?" => t.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_with_ansi_is_sanitised_before_storage() {
        // A `tool_use` row whose name carries an OSC-52 clipboard write
        // would propagate raw into the cards view on every tick without
        // sanitisation. The JSON source uses `` so serde_json
        // (which rejects raw control bytes) decodes it cleanly.
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\\u001b]52;c;ZXZpbA==\\u0007after\"}]}}";
        match parse_jsonl_state(line).expect("parses") {
            SessionState::ToolRunning(name) => assert_eq!(name, "Bashafter"),
            other => panic!("expected ToolRunning, got {other:?}"),
        }
    }

    #[test]
    fn tool_name_ansi_smuggled_keyword_routes_to_correct_state() {
        // `AskUserQuestion<ESC>[2J` — without sanitisation would fall
        // through to ToolRunning with the escape attached; with it, the
        // sanitised name matches the WaitingForInput keyword.
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\\u001b[2J\"}]}}";
        assert!(matches!(
            parse_jsonl_state(line),
            Some(SessionState::WaitingForInput)
        ));
    }

    #[test]
    fn snippet_truncation_does_not_panic_on_utf8_boundary() {
        // 79 ASCII chars + a 4-byte emoji = 83 bytes; byte-slicing at
        // 80 would split the codepoint and panic. With char-based
        // truncation this is well-defined.
        let text = format!("{}🎉 trailing", "x".repeat(79));
        let item = serde_json::json!({
            "type": "tool_result",
            "content": text,
        });
        let snippet = extract_tool_result_snippet(&item);
        assert!(snippet.starts_with(&"x".repeat(79)));
        assert!(snippet.contains('🎉'));
        // Char count cap is 80, plus the ellipsis when truncated.
        assert!(snippet.chars().count() <= 81);
    }
}
