use std::process::{Command, Stdio};

use super::SessionTarget;
use crate::util::applescript;

struct TerminalInfo {
    terminal_id: String,
    cwd: String,
    name: String,
}

pub struct GhosttyBridge {
    nerve_terminal_id: Option<String>,
}

impl GhosttyBridge {
    pub fn new() -> Self {
        let nerve_terminal_id = detect_own_terminal();
        Self { nerve_terminal_id }
    }

    pub fn capture_screen(&self, target: &SessionTarget) -> Option<String> {
        let terminals = query_terminals().ok()?;

        let found = find_terminal_for_session(
            &terminals, &target.cwd, &target.name, &target.dir_name,
            self.nerve_terminal_id.as_deref(),
            target.tty.as_deref(),
        )?;

        let (_tab_idx, term_idx) = find_terminal_position(&found.terminal_id)?;

        // All user-controlled values pass through argv. The empty string for
        // arg 3 means "no restore" — checked inside the script.
        const SCRIPT: &str = r#"on run argv
    set tid to item 1 of argv
    set termIdx to (item 2 of argv) as integer
    set restoreId to item 3 of argv
    set capturedText to ""
    tell application "Ghostty"
        focus terminal id tid
    end tell
    delay 0.15
    tell application "System Events"
        tell process "Ghostty"
            set w to window 1
            set g to group 1 of w
            set g2 to group 1 of g
            set topGroups to every group of g2
            set allAreas to {}
            repeat with tg in topGroups
                set midGroups to every group of tg
                repeat with mg in midGroups
                    try
                        set ta to text area 1 of scroll area 1 of mg
                        set end of allAreas to ta
                    end try
                end repeat
            end repeat
            if (count of allAreas) >= termIdx then
                set ta to item termIdx of allAreas
                set buf to value of ta
                set bufLen to length of buf
                set grabLen to 5000
                if bufLen < grabLen then set grabLen to bufLen
                set capturedText to text (bufLen - grabLen + 1) thru bufLen of buf
            end if
        end tell
    end tell
    if restoreId is not equal to "" then
        tell application "Ghostty"
            activate
            focus terminal id restoreId
        end tell
    end if
    return capturedText
end run"#;
        let term_idx_str = term_idx.to_string();
        let restore_id = self.nerve_terminal_id.as_deref().unwrap_or("");
        let args = [&found.terminal_id[..], &term_idx_str, restore_id];
        let output = applescript::run(SCRIPT, &args).ok()?;

        if !output.status.success() {
            return None;
        }

        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            return None;
        }
        Some(text)
    }

    pub fn go_to_session(&self, target: &SessionTarget) -> anyhow::Result<()> {
        let terminals = query_terminals()?;

        let found = find_terminal_for_session(
            &terminals, &target.cwd, &target.name, &target.dir_name,
            self.nerve_terminal_id.as_deref(),
            target.tty.as_deref(),
        )
        .ok_or_else(|| anyhow::anyhow!("session not in a visible tab"))?;

        focus_terminal(&found.terminal_id)
    }

    pub fn resolve_terminal_id(&self, target: &SessionTarget) -> Option<String> {
        let terminals = query_terminals().ok()?;
        find_terminal_for_session(
            &terminals,
            &target.cwd,
            &target.name,
            &target.dir_name,
            self.nerve_terminal_id.as_deref(),
            target.tty.as_deref(),
        )
        .map(|t| t.terminal_id.clone())
    }
}

fn detect_own_terminal() -> Option<String> {
    use std::io::Write;

    let marker = format!("nerve-{}", std::process::id());
    print!("\x1b]2;{}\x07", marker);
    std::io::stdout().flush().ok();
    std::thread::sleep(std::time::Duration::from_millis(100));

    let terminals = query_terminals().ok()?;
    let result = terminals.iter()
        .find(|t| t.name.contains(&marker))
        .map(|t| t.terminal_id.clone());

    print!("\x1b]2;nerve\x07");
    std::io::stdout().flush().ok();

    if result.is_some() {
        return result;
    }

    let nerve_cwd = std::env::current_dir().ok()?;
    let nerve_canonical = std::fs::canonicalize(&nerve_cwd)
        .unwrap_or_else(|_| nerve_cwd.clone());
    terminals.iter()
        .find(|t| {
            let t_path = std::path::Path::new(&t.cwd);
            t_path == nerve_cwd.as_path()
                || std::fs::canonicalize(t_path)
                    .map(|c| c == nerve_canonical)
                    .unwrap_or(false)
        })
        .map(|t| t.terminal_id.clone())
}

fn query_terminals() -> anyhow::Result<Vec<TerminalInfo>> {
    let script = r#"
tell application "Ghostty"
    set out to ""
    set winCount to count of windows
    repeat with w from 1 to winCount
        tell window w
            set tabCount to count of tabs
            repeat with i from 1 to tabCount
                tell tab i
                    set termCount to count of terminals
                    repeat with j from 1 to termCount
                        tell terminal j
                            set tid to id
                            set tcwd to working directory
                            set tname to name
                            set out to out & tid & "|" & tcwd & "|" & tname & linefeed
                        end tell
                    end repeat
                end tell
            end repeat
        end tell
    end repeat
    return out
end tell
"#;

    let output = Command::new("osascript")
        .args(["-e", script])
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("AppleScript failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut terminals = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '|').collect();
        if parts.len() < 3 {
            continue;
        }
        terminals.push(TerminalInfo {
            terminal_id: parts[0].trim().to_string(),
            cwd: parts[1].trim().to_string(),
            name: parts[2].trim().to_string(),
        });
    }

    Ok(terminals)
}

fn probe_terminal_by_tty(candidate_ids: &[&str], tty: &str) -> Option<String> {
    use std::io::Write;

    let dev_path = if tty.starts_with("/dev/") {
        tty.to_string()
    } else {
        format!("/dev/{tty}")
    };

    let marker = format!("nerve-probe-{}", std::process::id());

    let mut file = std::fs::OpenOptions::new().write(true).open(&dev_path).ok()?;
    file.write_all(format!("\x1b]2;{marker}\x07").as_bytes()).ok()?;
    file.flush().ok()?;

    std::thread::sleep(std::time::Duration::from_millis(80));

    // Pass every candidate id through argv. The script iterates the
    // arguments rather than the interpolated list literal.
    const SCRIPT: &str = r#"on run argv
    set out to ""
    tell application "Ghostty"
        repeat with tid in argv
            try
                set tname to name of (terminal id tid)
                set out to out & tid & "|" & tname & linefeed
            end try
        end repeat
    end tell
    return out
end run"#;
    let output = applescript::run(SCRIPT, candidate_ids).ok()?;

    // Reset title
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&dev_path) {
        let _ = f.write_all(b"\x1b]2;\x07");
    }

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((id, name)) = line.split_once('|') {
            if name.contains(&marker) {
                return Some(id.trim().to_string());
            }
        }
    }
    None
}

fn find_terminal_for_session<'a>(
    terminals: &'a [TerminalInfo],
    session_cwd: &str,
    session_name: &str,
    dir_name: &str,
    nerve_terminal_id: Option<&str>,
    session_tty: Option<&str>,
) -> Option<&'a TerminalInfo> {
    let is_nerve_terminal = |t: &TerminalInfo| {
        nerve_terminal_id.is_some_and(|id| t.terminal_id == id)
    };

    let session_path = std::path::Path::new(session_cwd);
    let session_canonical =
        std::fs::canonicalize(session_path).unwrap_or(session_path.to_path_buf());

    let mut exact_matches: Vec<&TerminalInfo> = Vec::new();

    for t in terminals {
        if is_nerve_terminal(t) {
            continue;
        }
        let t_path = std::path::Path::new(&t.cwd);
        let is_match = t_path == session_path
            || std::fs::canonicalize(t_path)
                .map(|c| c == session_canonical)
                .unwrap_or(false);
        if is_match {
            exact_matches.push(t);
        }
    }

    if exact_matches.len() == 1 {
        return Some(exact_matches[0]);
    }

    if exact_matches.len() > 1 {
        if let Some(tty) = session_tty {
            let ids: Vec<&str> = exact_matches.iter().map(|t| t.terminal_id.as_str()).collect();
            if let Some(matched_id) = probe_terminal_by_tty(&ids, tty) {
                if let Some(t) = exact_matches.iter().find(|t| t.terminal_id == matched_id) {
                    return Some(t);
                }
            }
        }

        for candidate_name in [session_name, dir_name] {
            let name_lower = candidate_name.to_lowercase();
            if name_lower.is_empty() {
                continue;
            }
            for t in &exact_matches {
                let tname_lower = t.name.to_lowercase();
                if tname_lower.contains(&name_lower) || name_lower.contains(&tname_lower) {
                    return Some(t);
                }
            }
        }
        return Some(exact_matches[0]);
    }

    // Longest prefix match for cd'd sessions. Canonicalize both sides so
    // a malicious cwd that uses `..` traversal can't false-match an
    // unrelated terminal (S14). `Path::starts_with` is component-based,
    // not byte-based, so once both paths are canonical the comparison
    // can no longer be tricked by traversal segments.
    let mut best: Option<(usize, &TerminalInfo)> = None;
    for t in terminals {
        if is_nerve_terminal(t) {
            continue;
        }
        let t_path = std::path::Path::new(&t.cwd);
        let t_canonical =
            std::fs::canonicalize(t_path).unwrap_or_else(|_| t_path.to_path_buf());
        let session_for_match = &session_canonical;
        if session_for_match.starts_with(&t_canonical)
            || t_canonical.starts_with(session_for_match)
        {
            let common = common_prefix_len(session_for_match, &t_canonical);
            if best.map_or(true, |(best_len, _)| common > best_len) {
                best = Some((common, t));
            }
        }
    }
    best.map(|(_, t)| t)
}

fn common_prefix_len(a: &std::path::Path, b: &std::path::Path) -> usize {
    a.components()
        .zip(b.components())
        .take_while(|(ca, cb)| ca == cb)
        .count()
}

fn find_terminal_position(target_id: &str) -> Option<(usize, usize)> {
    let script = r#"
tell application "Ghostty"
    set out to ""
    set winCount to count of windows
    repeat with w from 1 to winCount
        tell window w
            set tabCount to count of tabs
            repeat with i from 1 to tabCount
                tell tab i
                    set termCount to count of terminals
                    repeat with j from 1 to termCount
                        tell terminal j
                            set tid to id
                            set out to out & w & "|" & i & "|" & j & "|" & tid & linefeed
                        end tell
                    end repeat
                end tell
            end repeat
        end tell
    end repeat
    return out
end tell
"#;
    let output = Command::new("osascript")
        .args(["-e", script])
        .stdin(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(4, '|').collect();
        if parts.len() < 4 {
            continue;
        }
        let tid = parts[3].trim();
        if tid == target_id {
            let tab_idx: usize = parts[1].trim().parse().ok()?;
            let term_idx: usize = parts[2].trim().parse().ok()?;
            return Some((tab_idx, term_idx));
        }
    }
    None
}

pub(crate) fn focus_terminal(terminal_id: &str) -> anyhow::Result<()> {
    const SCRIPT: &str = r#"on run argv
    set tid to item 1 of argv
    tell application "Ghostty"
        activate
        focus terminal id tid
    end tell
end run"#;
    let output = applescript::run(SCRIPT, &[terminal_id])?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("focus terminal failed: {stderr}");
    }
    Ok(())
}
