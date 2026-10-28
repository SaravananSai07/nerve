//! Typing a remote instruction into a running interactive session.
//!
//! Claude's TUI accepts a bracketed paste followed by Enter as a normal
//! user turn (verified on 2.1.288: multi-line text arrives as one
//! message). tmux panes get `paste-buffer -p`; Ghostty tabs get the
//! AppleScript `input text` command. Background and desktop sessions
//! have no pane to type into.

use std::process::{Command, Stdio};

use crate::state::session::{DiscoverySnapshot, SessionKind};

/// Where a session's prompt can be typed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputTarget {
    TmuxPane(String),
    #[cfg(target_os = "macos")]
    Ghostty(String),
}

pub(crate) fn tmux_panes() -> Vec<(String, String)> {
    let Ok(out) = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id} #{pane_tty}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string())))
        .collect()
}

/// Match by tty: the pane's tty is the session's controlling tty.
pub(crate) fn pane_for_tty(panes: &[(String, String)], tty: &str) -> Option<String> {
    let want = tty.trim_start_matches("/dev/");
    panes
        .iter()
        .find(|(_, t)| t.trim_start_matches("/dev/") == want)
        .map(|(id, _)| id.clone())
}

/// Resolve how to reach a session. Spawns tmux / osascript: worker only.
pub(crate) fn resolve(snap: &DiscoverySnapshot, panes: &[(String, String)]) -> Option<InputTarget> {
    if snap.kind != SessionKind::Terminal {
        return None;
    }
    let tty = snap.tty.as_ref()?.as_str();
    if let Some(p) = pane_for_tty(panes, tty) {
        return Some(InputTarget::TmuxPane(p));
    }
    #[cfg(target_os = "macos")]
    {
        let target = crate::platform::SessionTarget {
            cwd: snap.cwd.to_string_lossy().into_owned(),
            name: snap.name.as_str().to_string(),
            dir_name: snap.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            tty: Some(tty.to_string()),
            kind: snap.kind,
        };
        if let Some(id) = crate::platform::ghostty::terminal_id_headless(&target) {
            return Some(InputTarget::Ghostty(id));
        }
    }
    None
}

/// Instructions are capped and stripped of control characters other than
/// newline/tab: a remote peer must not be able to send Escape sequences or
/// Ctrl-keys that drive the TUI (e.g. accept a dialog) instead of typing.
pub(crate) fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(16_000)
        .collect::<String>()
        .trim_end()
        .to_string()
}

pub(crate) fn type_into(target: &InputTarget, text: &str) -> Result<(), String> {
    let text = clean_text(text);
    if text.is_empty() {
        return Err("empty instruction".into());
    }
    match target {
        InputTarget::TmuxPane(pane) => {
            let buf = format!("nerve-{}", super::protocol::new_id());
            let mut child = Command::new("tmux")
                .args(["load-buffer", "-b", &buf, "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("tmux: {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            }
            if !child.wait().is_ok_and(|s| s.success()) {
                return Err("tmux load-buffer failed".into());
            }
            let run = |args: &[&str]| {
                Command::new("tmux")
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            };
            if !run(&["paste-buffer", "-p", "-d", "-b", &buf, "-t", pane]) {
                return Err(format!("tmux paste into {pane} failed"));
            }
            // Let the TUI finish ingesting the paste before submitting.
            std::thread::sleep(std::time::Duration::from_millis(300));
            if !run(&["send-keys", "-t", pane, "Enter"]) {
                return Err("tmux send Enter failed".into());
            }
            Ok(())
        }
        #[cfg(target_os = "macos")]
        InputTarget::Ghostty(id) => crate::platform::ghostty::input_text(id, &text).map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_matches_tty_with_or_without_dev_prefix() {
        let panes = vec![("%1".to_string(), "/dev/pts/3".to_string()), ("%2".to_string(), "/dev/ttys004".to_string())];
        assert_eq!(pane_for_tty(&panes, "pts/3"), Some("%1".into()));
        assert_eq!(pane_for_tty(&panes, "/dev/ttys004"), Some("%2".into()));
        assert_eq!(pane_for_tty(&panes, "ttys009"), None);
    }

    #[test]
    fn remote_text_cannot_carry_control_keys() {
        assert_eq!(clean_text("fix it\x1b[A\x03\nnow\t!\r"), "fix it[A\nnow\t!");
    }
}
