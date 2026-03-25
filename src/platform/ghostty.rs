use std::process::{Command, Stdio};

struct TerminalInfo {
    terminal_id: String,
    cwd: String,
    name: String,
}

pub struct GhosttyBridge;

impl GhosttyBridge {
    pub fn new() -> Self {
        Self
    }

    pub fn go_to_session(&self, cwd: &str, session_name: &str, dir_name: &str) -> anyhow::Result<()> {
        let terminals = query_terminals()?;
        let nerve_cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let target = find_terminal_for_session(&terminals, cwd, session_name, dir_name, &nerve_cwd)
            .ok_or_else(|| anyhow::anyhow!("session not in a visible tab"))?;

        focus_terminal(&target.terminal_id)
    }
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

fn find_terminal_for_session<'a>(
    terminals: &'a [TerminalInfo],
    session_cwd: &str,
    session_name: &str,
    dir_name: &str,
    nerve_cwd: &str,
) -> Option<&'a TerminalInfo> {
    let is_nerve_terminal = |t: &TerminalInfo| {
        t.cwd == nerve_cwd && t.name.contains("/nerve")
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
        // Try display name first (may be a config override), then dir name
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

    // Longest prefix match for cd'd sessions
    let mut best: Option<(usize, &TerminalInfo)> = None;
    for t in terminals {
        if is_nerve_terminal(t) {
            continue;
        }
        let t_path = std::path::Path::new(&t.cwd);
        if session_path.starts_with(t_path) || t_path.starts_with(session_path) {
            let common = common_prefix_len(session_path, t_path);
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

fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn focus_terminal(terminal_id: &str) -> anyhow::Result<()> {
    let safe_id = escape_applescript(terminal_id);
    let script = format!(
        "tell application \"Ghostty\"\n\
             activate\n\
             focus terminal id \"{safe_id}\"\n\
         end tell"
    );
    let output = Command::new("osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("focus terminal failed: {stderr}");
    }
    Ok(())
}
