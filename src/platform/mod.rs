pub mod ghostty;
pub mod tmux;

pub struct Tab {
    pub id: String,
    pub index: usize,
    pub title: String,
    pub tty: Option<String>,
}

pub trait TerminalBridge: Send + Sync {
    fn detect() -> bool where Self: Sized;
    fn enumerate_tabs(&self) -> anyhow::Result<Vec<Tab>>;
    fn switch_to(&self, tab: &Tab) -> anyhow::Result<()>;
    fn go_to_session(&self, cwd: &str, session_name: &str, dir_name: &str) -> anyhow::Result<()>;
}

pub enum Bridge {
    Ghostty(ghostty::GhosttyBridge),
    Tmux(tmux::TmuxBridge),
}

impl Bridge {
    pub fn auto_detect() -> Option<Self> {
        let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
        match term.as_str() {
            "ghostty" => Some(Self::Ghostty(ghostty::GhosttyBridge::new())),
            "tmux" => Some(Self::Tmux(tmux::TmuxBridge)),
            _ => {
                if std::env::var("TMUX").is_ok() {
                    Some(Self::Tmux(tmux::TmuxBridge))
                } else {
                    None
                }
            }
        }
    }

    pub fn go_to_session(&self, cwd: &str, session_name: &str, dir_name: &str) -> anyhow::Result<()> {
        match self {
            Self::Ghostty(g) => g.go_to_session(cwd, session_name, dir_name),
            Self::Tmux(t) => t.go_to_session(cwd, session_name, dir_name),
        }
    }
}
