use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub tty: String,
    pub comm: String,
    pub cpu: f32,
    pub args: String,
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
    use super::resume_session_id;

    #[test]
    fn resume_session_id_handles_all_forms() {
        assert_eq!(resume_session_id("claude --resume abc-123 --foo"), Some("abc-123"));
        assert_eq!(resume_session_id("claude -r abc-123"), Some("abc-123"));
        assert_eq!(resume_session_id("claude --resume=abc-123"), Some("abc-123"));
        assert_eq!(resume_session_id("claude --foo"), None);
        assert_eq!(resume_session_id("claude --resume"), None);
        assert_eq!(resume_session_id("claude --resume="), None);
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
