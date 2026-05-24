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

/// Cached snapshot of the system's process table plus its parent→child
/// index. Refreshed lazily on a TTL configured by `process_scan_interval_ms`
/// (closes A7 — the setting was previously dead config). Each refresh is
/// one `ps -eo` fork; with the default 5 s TTL that's roughly 80%
/// fewer forks than the previous per-tick scan.
pub struct ProcessTable {
    procs: Vec<ProcessInfo>,
    child_map: HashMap<u32, Vec<u32>>,
    snapshot_at: Instant,
}

impl ProcessTable {
    pub fn refreshed() -> Self {
        let procs = scan_processes();
        let child_map = build_child_map(&procs);
        Self {
            procs,
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .skip(1)
        .filter_map(parse_ps_line)
        .collect()
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
    let mut tokens = args.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "--resume" || tok == "-r" {
            return tokens.next().filter(|v| !v.is_empty());
        }
        if let Some(rest) = tok.strip_prefix("--resume=") {
            return Some(rest).filter(|v| !v.is_empty());
        }
    }
    None
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
    fn process_table_refreshes_once_per_ttl_window() {
        let mut table = ProcessTable::refreshed();
        let first_snapshot_at = table.snapshot_at;
        // Within the TTL window the snapshot stamp stays put.
        table.refresh_if_stale(Duration::from_secs(60));
        assert_eq!(table.snapshot_at, first_snapshot_at);
        // A zero-TTL forces an immediate refresh; the stamp advances.
        std::thread::sleep(Duration::from_millis(2));
        table.refresh_if_stale(Duration::from_millis(0));
        assert!(table.snapshot_at > first_snapshot_at);
    }
}

pub fn build_child_map(procs: &[ProcessInfo]) -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        map.entry(p.ppid).or_default().push(p.pid);
    }
    map
}

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

pub fn get_tty_for_pid(procs: &[ProcessInfo], pid: u32) -> Option<String> {
    find_process(procs, pid).map(|p| p.tty.clone())
}

pub fn get_cpu_for_pid(procs: &[ProcessInfo], pid: u32) -> f32 {
    find_process(procs, pid).map(|p| p.cpu).unwrap_or(0.0)
}
