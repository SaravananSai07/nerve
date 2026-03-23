use super::{Tab, TerminalBridge};

pub struct TmuxBridge;

impl TerminalBridge for TmuxBridge {
    fn detect() -> bool {
        std::env::var("TMUX").is_ok()
    }

    fn enumerate_tabs(&self) -> anyhow::Result<Vec<Tab>> {
        let output = std::process::Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{pane_pid} #{pane_id} #{pane_tty} #{pane_current_path}"])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let tabs: Vec<Tab> = stdout
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let parts: Vec<&str> = line.splitn(4, ' ').collect();
                if parts.len() < 4 {
                    return None;
                }
                Some(Tab {
                    id: parts[1].to_string(),
                    index: i,
                    title: parts[3].to_string(),
                    tty: Some(parts[2].to_string()),
                })
            })
            .collect();

        Ok(tabs)
    }

    fn switch_to(&self, tab: &Tab) -> anyhow::Result<()> {
        std::process::Command::new("tmux")
            .args(["select-pane", "-t", &tab.id])
            .output()?;
        Ok(())
    }

    fn go_to_session(&self, cwd: &str, _session_name: &str, _dir_name: &str) -> anyhow::Result<()> {
        let tabs = self.enumerate_tabs()?;
        let cwd_path = std::path::Path::new(cwd);

        for tab in &tabs {
            if tab.title == cwd || std::path::Path::new(&tab.title) == cwd_path {
                return self.switch_to(tab);
            }
        }

        anyhow::bail!("no pane found for cwd {cwd}")
    }
}
