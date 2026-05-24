use std::fmt;
use std::str::FromStr;

#[cfg(target_os = "macos")]
pub mod ghostty;
pub mod tmux;

pub struct SessionTarget {
    pub cwd: String,
    pub name: String,
    pub dir_name: String,
    pub tty: Option<String>,
}

pub enum Bridge {
    #[cfg(target_os = "macos")]
    Ghostty(ghostty::GhosttyBridge),
    Tmux(tmux::TmuxBridge),
    /// No terminal integration available — running outside a supported
    /// terminal multiplexer / host. All operations are no-ops that
    /// surface a clear error to the user. Eliminates the
    /// `Option<Bridge>` shape that previously peppered the codebase
    /// with `if let Some(...)` checks (closes A4).
    NoOp,
}

impl Bridge {
    pub fn auto_detect() -> Self {
        let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
        match term.as_str() {
            #[cfg(target_os = "macos")]
            "ghostty" => Self::Ghostty(ghostty::GhosttyBridge::new()),
            "tmux" => Self::Tmux(tmux::TmuxBridge),
            _ => {
                if std::env::var("TMUX").is_ok() {
                    Self::Tmux(tmux::TmuxBridge)
                } else {
                    Self::NoOp
                }
            }
        }
    }

    pub fn go_to_session(&self, target: &SessionTarget) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Ghostty(g) => g.go_to_session(target),
            Self::Tmux(t) => t.go_to_session(target),
            Self::NoOp => anyhow::bail!(
                "no terminal bridge detected (start nerve inside Ghostty or tmux)"
            ),
        }
    }

    pub fn capture_screen(&self, target: &SessionTarget) -> Option<String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Ghostty(g) => g.capture_screen(target),
            Self::Tmux(t) => t.capture_screen(target),
            Self::NoOp => None,
        }
    }

    pub fn resolve_id(&self, target: &SessionTarget) -> Option<BridgeId> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Ghostty(g) => g.resolve_terminal_id(target).map(BridgeId::Ghostty),
            Self::Tmux(t) => t.resolve_pane_id(target).map(BridgeId::Tmux),
            Self::NoOp => None,
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self, Self::NoOp)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeId {
    Ghostty(String),
    Tmux(String),
}

impl BridgeId {
    pub fn focus(&self) -> anyhow::Result<()> {
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
        let (prefix, payload) = s
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("expected '<bridge>:<id>', got {s:?}"))?;
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
        for id in [
            BridgeId::Ghostty("abc-123".to_string()),
            BridgeId::Tmux("%5".to_string()),
        ] {
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
