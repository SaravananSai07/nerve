use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub tty: String,
    pub comm: String,
    pub cpu: f32,
    pub args: String,
}

type PidIndex = HashMap<u32, usize>;

/// Cached snapshot of the process table plus its parent→child index.
/// Refreshed on a TTL (`process_scan_interval_ms`) so we don't fork
/// `ps -eo` on every tick — at the default 5 s that's roughly 80 %
/// fewer forks than a per-tick scan.
pub struct ProcessTable {
    procs: Vec<ProcessInfo>,
    pid_index: PidIndex,
    child_map: HashMap<u32, Vec<u32>>,
    snapshot_at: Instant,
}

impl ProcessTable {
    pub fn refreshed() -> Self {
        let procs = scan_processes();
        let pid_index: PidIndex = procs
            .iter()
            .enumerate()
            .map(|(idx, p)| (p.pid, idx))
            .collect();
        let child_map = build_child_map(&procs);
        Self {
            procs,
            pid_index,
            child_map,
            snapshot_at: Instant::now(),
        }
    }

    pub fn refresh_if_stale(&mut self, max_age: Duration) {
        if self.snapshot_at.elapsed() >= max_age {
            *self = Self::refreshed();
        }
    }

    pub fn procs(&self) -> &[ProcessInfo] {
        &self.procs
    }

    pub fn child_map(&self) -> &HashMap<u32, Vec<u32>> {
        &self.child_map
    }

    /// O(1) pid lookup over the cached snapshot.
    pub fn find_by_pid(&self, pid: u32) -> Option<&ProcessInfo> {
        let idx = *self.pid_index.get(&pid)?;
        self.procs.get(idx)
    }
}

pub fn scan_processes() -> Vec<ProcessInfo> {
    let output = match std::process::Command::new("ps")
        .args(["-eo", "pid,ppid,tty,comm,%cpu,args"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    // Iterate bytes-then-validate so we don't pay for a single
    // `from_utf8_lossy` over the entire ps output (~70 KiB at 600
    // procs); each line is validated lazily as we go.
    let mut out = Vec::with_capacity(64);
    let mut header_seen = false;
    for chunk in output.stdout.split(|&b| b == b'\n') {
        if !header_seen {
            header_seen = true;
            continue;
        }
        if chunk.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(chunk).unwrap_or("");
        if let Some(info) = parse_ps_line(line) {
            out.push(info);
        }
    }
    out
}

fn parse_ps_line(line: &str) -> Option<ProcessInfo> {
    // Split off the first five whitespace-separated columns and keep everything
    // after as `args` verbatim. The `comm` column can contain a path with
    // slashes but never whitespace, so this is unambiguous.
    let mut iter = line.split_whitespace();
    let pid = iter.next()?.parse().ok()?;
    let ppid = iter.next()?.parse().ok()?;
    let tty = iter.next()?.to_string();
    let comm = iter.next()?.to_string();
    let cpu = iter.next()?.parse().unwrap_or(0.0);
    let args = iter.collect::<Vec<_>>().join(" ");
    Some(ProcessInfo {
        pid,
        ppid,
        tty,
        comm,
        cpu,
        args,
    })
}

pub fn resume_session_id(args: &str) -> Option<&str> {
    // Claude Code rewrites the per-PID session file's sessionId after a
    // --resume, but the actual transcript JSONL keeps the original id. The
    // command line is the only place that still names it correctly.
    //
    // The extracted value is used later as a filename component
    // (`{id}.jsonl`), so reject anything that isn't a plausible session id.
    // `is_safe_id_char` already excludes `/` and `.` is fine on its own but
    // we further forbid `..` to close path-traversal entirely.
    let mut tokens = args.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "--resume" || tok == "-r" {
            return tokens.next().and_then(valid_session_id);
        }
        if let Some(rest) = tok.strip_prefix("--resume=") {
            return valid_session_id(rest);
        }
    }
    None
}

fn valid_session_id(raw: &str) -> Option<&str> {
    if raw.is_empty() || raw.contains("..") {
        return None;
    }
    if !raw.chars().all(crate::util::focus_arg::is_safe_id_char) {
        return None;
    }
    Some(raw)
}

pub fn build_child_map(procs: &[ProcessInfo]) -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        map.entry(p.ppid).or_default().push(p.pid);
    }
    map
}

/// Linear scan over a slice. Hot-path callers (`load_session`,
/// `infer_state_from_jsonl`) use `ProcessTable::find_by_pid` instead;
/// this exists for `is_claude_process` and other places that only
/// have a `&[ProcessInfo]`.
pub fn find_process(procs: &[ProcessInfo], pid: u32) -> Option<&ProcessInfo> {
    procs.iter().find(|p| p.pid == pid)
}

pub fn has_child_named(
    procs: &[ProcessInfo],
    child_map: &HashMap<u32, Vec<u32>>,
    parent_pid: u32,
    name: &str,
) -> bool {
    let mut stack = vec![parent_pid];
    let mut visited = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if let Some(children) = child_map.get(&pid) {
            for &child_pid in children {
                if let Some(child) = find_process(procs, child_pid) {
                    let comm_name = child.comm.rsplit('/').next().unwrap_or(&child.comm);
                    if comm_name == name {
                        return true;
                    }
                }
                stack.push(child_pid);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_session_id_handles_all_forms() {
        assert_eq!(resume_session_id("claude --resume abc-123 --foo"), Some("abc-123"));
        assert_eq!(resume_session_id("claude -r abc-123"), Some("abc-123"));
        assert_eq!(resume_session_id("claude --resume=abc-123"), Some("abc-123"));
        assert_eq!(resume_session_id("claude --foo"), None);
        assert_eq!(resume_session_id("claude --resume"), None);
        assert_eq!(resume_session_id("claude --resume="), None);
    }

    #[test]
    fn resume_session_id_rejects_path_traversal_and_unsafe_chars() {
        // `..` segments would let a hostile `claude --resume` escape the
        // projects dir; `is_safe_id_char` forbids `/` and shell metacharacters.
        assert_eq!(resume_session_id("claude --resume ../../etc/passwd"), None);
        assert_eq!(resume_session_id("claude --resume=foo/../bar"), None);
        assert_eq!(resume_session_id("claude --resume foo;bar"), None);
        assert_eq!(resume_session_id("claude --resume foo bar"), Some("foo"));
    }

    #[test]
    fn process_table_refreshes_once_per_ttl_window() {
        let mut table = ProcessTable::refreshed();
        let first_snapshot_at = table.snapshot_at;
        table.refresh_if_stale(Duration::from_secs(60));
        assert_eq!(table.snapshot_at, first_snapshot_at);
        std::thread::sleep(Duration::from_millis(2));
        table.refresh_if_stale(Duration::from_millis(0));
        assert!(table.snapshot_at > first_snapshot_at);
    }
}

