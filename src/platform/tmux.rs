use std::process::Command;

use super::SessionTarget;

struct Pane {
    id: String,
    tty: String,
    cwd: String,
}

pub struct TmuxBridge;

impl TmuxBridge {
    pub fn capture_screen(&self, target: &SessionTarget) -> Option<String> {
        let pane = self.find_pane(target)?;

        let output = Command::new("tmux")
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

    fn enumerate_panes(&self) -> anyhow::Result<Vec<Pane>> {
        let output = Command::new("tmux")
            .args([
                "list-panes",
                "-a",
                "-F",
                "#{pane_id} #{pane_tty} #{pane_current_path}",
            ])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let panes: Vec<Pane> = stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, ' ');
                let id = parts.next()?.to_string();
                let tty = parts.next()?.to_string();
                let cwd = parts.next()?.to_string();
                Some(Pane { id, tty, cwd })
            })
            .collect();

        Ok(panes)
    }

    fn find_pane(&self, target: &SessionTarget) -> Option<Pane> {
        let panes = self.enumerate_panes().ok()?;
        let cwd_path = std::path::Path::new(&target.cwd);

        if let Some(tty) = target.tty.as_deref() {
            if let Some(p) = panes.iter().find(|p| p.tty == tty) {
                return Some(Pane {
                    id: p.id.clone(),
                    tty: p.tty.clone(),
                    cwd: p.cwd.clone(),
                });
            }
        }

        panes
            .into_iter()
            .find(|p| p.cwd == target.cwd || std::path::Path::new(&p.cwd) == cwd_path)
    }

    pub fn go_to_session(&self, target: &SessionTarget) -> anyhow::Result<()> {
        let pane = self
            .find_pane(target)
            .ok_or_else(|| anyhow::anyhow!("no pane found for cwd {}", target.cwd))?;
        focus_pane(&pane.id)
    }

    pub fn resolve_pane_id(&self, target: &SessionTarget) -> Option<String> {
        self.find_pane(target).map(|p| p.id)
    }
}

pub(crate) fn focus_pane(pane_id: &str) -> anyhow::Result<()> {
    let status = Command::new("tmux")
        .args(["select-pane", "-t", pane_id])
        .status()?;
    if !status.success() {
        anyhow::bail!("tmux select-pane failed for {pane_id}");
    }
    Ok(())
}
