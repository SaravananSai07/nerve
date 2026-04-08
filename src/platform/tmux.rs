use super::SessionTarget;

struct Tab {
    id: String,
    title: String,
}

pub struct TmuxBridge;

impl TmuxBridge {
    pub fn capture_screen(&self, target: &SessionTarget) -> Option<String> {
        let cwd = &target.cwd;
        let tabs = self.enumerate_tabs().ok()?;
        let cwd_path = std::path::Path::new(cwd);

        let pane = tabs.iter().find(|tab| {
            tab.title == *cwd || std::path::Path::new(&tab.title) == cwd_path
        })?;

        let output = std::process::Command::new("tmux")
            .args(["capture-pane", "-t", &pane.id, "-p", "-S", "-100"])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            return None;
        }
        Some(text)
    }

    fn enumerate_tabs(&self) -> anyhow::Result<Vec<Tab>> {
        let output = std::process::Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{pane_id} #{pane_current_path}"])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let tabs: Vec<Tab> = stdout
            .lines()
            .filter_map(|line| {
                let (id, title) = line.split_once(' ')?;
                Some(Tab {
                    id: id.to_string(),
                    title: title.to_string(),
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

    pub fn go_to_session(&self, target: &SessionTarget) -> anyhow::Result<()> {
        let cwd = &target.cwd;
        let tabs = self.enumerate_tabs()?;
        let cwd_path = std::path::Path::new(cwd);

        for tab in &tabs {
            if tab.title == *cwd || std::path::Path::new(&tab.title) == cwd_path {
                return self.switch_to(tab);
            }
        }

        anyhow::bail!("no pane found for cwd {cwd}")
    }
}
