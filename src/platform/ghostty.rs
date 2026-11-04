use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::run_applescript;
use super::SessionTarget;

struct TerminalInfo {
    terminal_id: String,
    cwd: String,
    /// Canonicalized once at query time so the matching loop in
    /// `find_terminal_for_session` doesn't `realpath(3)` every
    /// candidate per lookup. Falls back to the raw `cwd` if
    /// canonicalize fails.
    cwd_canonical: PathBuf,
    name: String,
}

pub(crate) struct GhosttyBridge {
    probe: Arc<Probe>,
}

/// Finds nerve's own terminal: the UI thread shows `marker` as the window
/// title, then a background thread asks Ghostty which terminal carries it.
/// Only the UI thread writes to the screen, so the title can't interleave
/// with a frame.
struct Probe {
    marker: String,
    shown: Sender<()>,
    /// `None` until the probe finishes.
    id: OnceLock<Option<String>>,
}

/// How long the probe waits for the marker to be shown. A window in the
/// background doesn't draw, so the probe falls back to matching the cwd.
const PROBE_WAIT: Duration = Duration::from_secs(5);

impl GhosttyBridge {
    /// Starts the probe; `on_found` gets nerve's own terminal id, off the
    /// UI thread.
    pub(crate) fn new(on_found: impl FnOnce(String) + Send + 'static) -> Self {
        let (shown, rx) = channel();
        let probe = Arc::new(Probe { marker: format!("nerve-{}", std::process::id()), shown, id: OnceLock::new() });
        let p = Arc::clone(&probe);
        let spawned = std::thread::Builder::new().name("nerve-ghostty-probe".into()).spawn(move || {
            let marker = rx.recv_timeout(PROBE_WAIT).is_ok().then_some(p.marker.as_str());
            let id = detect_own_terminal(marker);
            if let Some(id) = &id {
                on_found(id.clone());
            }
            let _ = p.id.set(id);
        });
        if spawned.is_err() {
            let _ = probe.id.set(None);
        }
        Self { probe }
    }

    /// The window title to show while the probe runs.
    pub(crate) fn probe_title(&self) -> Option<&str> {
        self.probe.id.get().is_none().then_some(self.probe.marker.as_str())
    }

    /// The UI thread has shown `probe_title`.
    pub(crate) fn title_shown(&self) {
        let _ = self.probe.shown.send(());
    }

    /// nerve's own terminal, once the probe has found it.
    pub(crate) fn own_id(&self) -> Option<&str> {
        self.probe.id.get().and_then(|o| o.as_deref())
    }

    pub(crate) fn capture_screen(&self, target: &SessionTarget) -> Option<String> {
        let terminals = query_terminals().ok()?;

        let found = find_terminal_for_session(
            &terminals,
            &target.cwd,
            &target.name,
            &target.dir_name,
            self.own_id(),
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
            -- Each terminal surface is a text area, in the tab's terminal
            -- order. Match by role rather than a fixed group path: the
            -- nesting depth differs across Ghostty versions (1.3 puts them
            -- three groups deep, which the old fixed path missed).
            set allAreas to {}
            set uiItems to entire contents of window 1
            repeat with el in uiItems
                if class of el is text area then set end of allAreas to contents of el
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
        let restore_id = self.own_id().unwrap_or("");
        let args = [&found.terminal_id[..], &term_idx_str, restore_id];
        let output = run_applescript(SCRIPT, &args).ok()?;

        if !output.status.success() {
            return None;
        }

        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            return None;
        }
        Some(text)
    }

    pub(crate) fn go_to_session(&self, target: &SessionTarget) -> anyhow::Result<()> {
        let terminals = query_terminals()?;

        let found = find_terminal_for_session(
            &terminals,
            &target.cwd,
            &target.name,
            &target.dir_name,
            self.own_id(),
            target.tty.as_deref(),
        )
        .ok_or_else(|| anyhow::anyhow!("session not in a visible tab"))?;

        focus_terminal(&found.terminal_id)
    }
}

/// The terminal titled `marker`, else the one whose cwd is nerve's.
fn detect_own_terminal(marker: Option<&str>) -> Option<String> {
    if marker.is_some() {
        // Let Ghostty pick up the new title.
        std::thread::sleep(Duration::from_millis(100));
    }
    let terminals = query_terminals().ok()?;
    if let Some(t) = marker.and_then(|m| terminals.iter().find(|t| t.name.contains(m))) {
        return Some(t.terminal_id.clone());
    }
    let nerve_cwd = std::env::current_dir().ok()?;
    let nerve_canonical = std::fs::canonicalize(&nerve_cwd).unwrap_or_else(|_| nerve_cwd.clone());
    terminals
        .iter()
        .find(|t| t.cwd_canonical == nerve_canonical || Path::new(&t.cwd) == nerve_cwd)
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

    let output = Command::new("osascript").args(["-e", script]).stdin(Stdio::null()).output()?;

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
        let cwd = parts[1].trim().to_string();
        let cwd_canonical = std::fs::canonicalize(&cwd).unwrap_or_else(|_| PathBuf::from(&cwd));
        terminals.push(TerminalInfo {
            terminal_id: parts[0].trim().to_string(),
            cwd,
            cwd_canonical,
            name: parts[2].trim().to_string(),
        });
    }

    Ok(terminals)
}

fn probe_terminal_by_tty(candidate_ids: &[&str], tty: &str) -> Option<String> {
    use std::io::Write;

    let dev_path = if tty.starts_with("/dev/") { tty.to_string() } else { format!("/dev/{tty}") };

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
    let output = run_applescript(SCRIPT, candidate_ids).ok()?;

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
    let is_nerve_terminal = |t: &TerminalInfo| nerve_terminal_id.is_some_and(|id| t.terminal_id == id);

    let session_path = std::path::Path::new(session_cwd);
    let session_canonical = std::fs::canonicalize(session_path).unwrap_or(session_path.to_path_buf());

    let mut exact_matches: Vec<&TerminalInfo> = Vec::new();

    for t in terminals {
        if is_nerve_terminal(t) {
            continue;
        }
        // Pre-canonicalized at query time — no per-lookup
        // `realpath(3)` syscall here.
        let is_match = t.cwd_canonical == session_canonical || std::path::Path::new(&t.cwd) == session_path;
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

    // Longest prefix match for cd'd sessions. Canonicalise both
    // sides first so a malicious cwd using `..` traversal can't
    // false-match an unrelated terminal. `Path::starts_with` is
    // component-based, so once both paths are canonical the
    // comparison can't be tricked by traversal segments.
    let mut best: Option<(usize, &TerminalInfo)> = None;
    for t in terminals {
        if is_nerve_terminal(t) {
            continue;
        }
        // Pre-canonicalized terminal cwd — no syscall here.
        let t_canonical = &t.cwd_canonical;
        let session_for_match = &session_canonical;
        if session_for_match.starts_with(t_canonical) || t_canonical.starts_with(session_for_match) {
            let common = common_prefix_len(session_for_match, t_canonical);
            if best.is_none_or(|(best_len, _)| common > best_len) {
                best = Some((common, t));
            }
        }
    }
    best.map(|(_, t)| t)
}

fn common_prefix_len(a: &std::path::Path, b: &std::path::Path) -> usize {
    a.components().zip(b.components()).take_while(|(ca, cb)| ca == cb).count()
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
    let output = Command::new("osascript").args(["-e", script]).stdin(Stdio::null()).output().ok()?;

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

pub(super) fn focus_terminal(terminal_id: &str) -> anyhow::Result<()> {
    const SCRIPT: &str = r#"on run argv
    set tid to item 1 of argv
    tell application "Ghostty"
        activate
        focus terminal id tid
    end tell
end run"#;
    let output = run_applescript(SCRIPT, &[terminal_id])?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("focus terminal failed: {stderr}");
    }
    Ok(())
}

/// Terminal id for a session without the own-terminal probe, which needs
/// a TUI to show its title and so can't run from a headless sync agent.
pub(crate) fn terminal_id_headless(target: &SessionTarget) -> Option<String> {
    let terminals = query_terminals().ok()?;
    find_terminal_for_session(&terminals, &target.cwd, &target.name, &target.dir_name, None, target.tty.as_deref())
        .map(|t| t.terminal_id.clone())
}

/// Type `text` into a terminal as a paste, then press Enter. Ghostty's
/// `input text` pastes verbatim (no shell expansion; verified with
/// `$(...)` and quotes), so multi-line instructions arrive intact.
pub(crate) fn input_text(terminal_id: &str, text: &str) -> anyhow::Result<()> {
    const SCRIPT: &str = r#"on run argv
    set tid to item 1 of argv
    tell application "Ghostty"
        input text (item 2 of argv) to terminal id tid
        delay 0.3
        send key "enter" to terminal id tid
    end tell
end run"#;
    let output = run_applescript(SCRIPT, &[terminal_id, text])?;
    if !output.status.success() {
        anyhow::bail!("ghostty input failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}
