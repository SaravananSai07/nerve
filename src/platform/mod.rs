use std::fmt;
use std::str::FromStr;

use crate::state::session::Session;

#[cfg(target_os = "macos")]
pub(crate) mod ghostty;
pub(crate) mod tmux;

/// Run an AppleScript with values passed via `argv`, never interpolated into
/// the script body. Reads `argv` inside the script with `on run argv` /
/// `item N of argv`. This is the only safe way to feed user-controlled
/// strings to `osascript(1)` — string interpolation invariably leaves
/// injection holes (newlines, comment markers, `¬` continuation, etc.).
#[cfg(target_os = "macos")]
pub(crate) fn run_applescript(script: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new("osascript");
    cmd.arg("-e").arg(script).arg("--").args(args);
    // `setsid()` in the child between fork and exec: it leaves nerve's
    // process group, so a SIGTERM to nerve doesn't kill a notification
    // mid-display. It fails only when the child already leads its group,
    // and the spawn is fine either way.
    unsafe {
        cmd.pre_exec(|| {
            let _ = nix::libc::setsid();
            Ok(())
        });
    }
    cmd.stdin(std::process::Stdio::null()).output()
}

pub(crate) struct SessionTarget {
    pub(crate) cwd: String,
    /// Ghostty finds a tab by its title.
    #[cfg(target_os = "macos")]
    pub(crate) name: String,
    #[cfg(target_os = "macos")]
    pub(crate) dir_name: String,
    pub(crate) tty: Option<String>,
}

impl From<&Session> for SessionTarget {
    fn from(s: &Session) -> Self {
        Self {
            cwd: s.cwd.to_string_lossy().into_owned(),
            #[cfg(target_os = "macos")]
            name: s.name().to_string(),
            #[cfg(target_os = "macos")]
            dir_name: s.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            tty: s.tty.as_ref().map(|t| t.as_str().to_string()),
        }
    }
}

pub(crate) enum Bridge {
    #[cfg(target_os = "macos")]
    Ghostty(ghostty::GhosttyBridge),
    Tmux(tmux::TmuxBridge),
    /// No terminal integration available — running outside a
    /// supported host. Lets us hand back a `Bridge` value instead
    /// of `Option<Bridge>` so the rest of the codebase doesn't
    /// pepper itself with `if let Some(bridge) = ...`. All
    /// operations are no-ops with a clear error surface.
    NoOp,
}

impl Bridge {
    /// The host terminal. `on_own_tab` gets nerve's own tab or pane once
    /// known, possibly from another thread.
    pub(crate) fn detect(on_own_tab: impl FnOnce(BridgeId) + Send + 'static) -> Self {
        let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
        match term.as_str() {
            #[cfg(target_os = "macos")]
            "ghostty" => Self::Ghostty(ghostty::GhosttyBridge::new(|id| on_own_tab(BridgeId::Ghostty(id)))),
            _ if term == "tmux" || std::env::var_os("TMUX").is_some() => {
                if let Some(pane) = tmux::own_pane() {
                    on_own_tab(BridgeId::Tmux(pane));
                }
                Self::Tmux(tmux::TmuxBridge)
            }
            _ => Self::NoOp,
        }
    }

    /// nerve's own tab or pane, once known: where a notification click
    /// lands.
    pub(crate) fn own_tab(&self) -> Option<BridgeId> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Ghostty(g) => g.own_id().map(|id| BridgeId::Ghostty(id.to_string())),
            Self::Tmux(_) => tmux::own_pane().map(BridgeId::Tmux),
            Self::NoOp => None,
        }
    }

    /// The window title to show while nerve looks for its own tab. Call
    /// `title_shown` once it is on screen.
    pub(crate) fn probe_title(&self) -> Option<&str> {
        #[cfg(target_os = "macos")]
        if let Self::Ghostty(g) = self {
            return g.probe_title();
        }
        None
    }

    pub(crate) fn title_shown(&self) {
        #[cfg(target_os = "macos")]
        if let Self::Ghostty(g) = self {
            g.title_shown();
        }
    }

    pub(crate) fn go_to_session(&self, target: &SessionTarget) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Ghostty(g) => g.go_to_session(target),
            Self::Tmux(t) => t.go_to_session(target),
            Self::NoOp => anyhow::bail!("no terminal bridge detected (start nerve inside Ghostty or tmux)"),
        }
    }

    pub(crate) fn capture_screen(&self, target: &SessionTarget) -> Option<String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Ghostty(g) => g.capture_screen(target),
            Self::Tmux(t) => t.capture_screen(target),
            Self::NoOp => None,
        }
    }

    /// Run a command in a new pane beside nerve. Returns `Ok(false)` when
    /// the host terminal has no scriptable way to do that.
    pub(crate) fn open_command(&self, argv: &[&str]) -> anyhow::Result<bool> {
        match self {
            Self::Tmux(_) => tmux::open_split(argv).map(|()| true),
            _ => Ok(false),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !matches!(self, Self::NoOp)
    }
}

/// Bring the Claude desktop app forward. There's no API to select a
/// specific session inside it.
pub(crate) fn focus_desktop_app() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status =
            std::process::Command::new("open").args(["-a", "Claude"]).stdin(std::process::Stdio::null()).status()?;
        if !status.success() {
            anyhow::bail!("could not open the Claude app");
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("desktop sessions can only be focused on macOS")
}

/// Best-effort clipboard write. Tries each platform tool in turn
/// (Wayland, then X11 on Linux) and returns false if none worked.
pub(crate) fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    const TOOLS: &[&[&str]] = &[&["pbcopy"]];
    #[cfg(not(target_os = "macos"))]
    const TOOLS: &[&[&str]] =
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "--clipboard", "--input"]];
    TOOLS.iter().any(|argv| pipe_to(argv, text))
}

fn pipe_to(argv: &[&str], text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && wrote
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BridgeId {
    Ghostty(String),
    Tmux(String),
}

impl BridgeId {
    pub(crate) fn focus(&self) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Ghostty(id) => ghostty::focus_terminal(id),
            #[cfg(not(target_os = "macos"))]
            Self::Ghostty(_) => anyhow::bail!("Ghostty focus is only supported on macOS"),
            Self::Tmux(id) => tmux::focus_pane(id),
        }
    }
}

impl fmt::Display for BridgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ghostty(id) => write!(f, "ghostty:{id}"),
            Self::Tmux(id) => write!(f, "tmux:{id}"),
        }
    }
}

impl FromStr for BridgeId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (prefix, payload) =
            s.split_once(':').ok_or_else(|| anyhow::anyhow!("expected '<bridge>:<id>', got {s:?}"))?;
        if payload.is_empty() {
            anyhow::bail!("empty id in {s:?}");
        }
        match prefix {
            "ghostty" => Ok(Self::Ghostty(payload.to_string())),
            "tmux" => Ok(Self::Tmux(payload.to_string())),
            other => anyhow::bail!("unknown bridge {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_id_round_trips() {
        for id in [BridgeId::Ghostty("abc-123".to_string()), BridgeId::Tmux("%5".to_string())] {
            let s = id.to_string();
            let parsed = BridgeId::from_str(&s).unwrap();
            assert_eq!(parsed, id);
        }
    }

    #[test]
    fn bridge_id_rejects_malformed() {
        assert!(BridgeId::from_str("ghostty").is_err());
        assert!(BridgeId::from_str("ghostty:").is_err());
        assert!(BridgeId::from_str("kitty:abc").is_err());
    }
}
