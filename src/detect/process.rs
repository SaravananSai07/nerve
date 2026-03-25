use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub tty: String,
    pub comm: String,
    pub cpu: f32,
}

pub fn scan_processes() -> Vec<ProcessInfo> {
    let output = match std::process::Command::new("ps")
        .args(["-eo", "pid,ppid,tty,comm,%cpu"])
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
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    Some(ProcessInfo {
        pid: parts[0].parse().ok()?,
        ppid: parts[1].parse().ok()?,
        tty: parts[2].to_string(),
        comm: parts[3].to_string(),
        cpu: parts[4].parse().unwrap_or(0.0),
    })
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
