use crate::config::NotificationConfig;
use crate::platform::{Bridge, SessionTarget};
use crate::state::session::SessionState;

pub struct Notifier {
    config: NotificationConfig,
    terminal_app: Option<String>,
}

impl Notifier {
    pub fn new(config: NotificationConfig, terminal_app: Option<String>) -> Self {
        Self { config, terminal_app }
    }

    pub fn maybe_notify(
        &self,
        session_name: &str,
        state: &SessionState,
        target: &SessionTarget,
        bridge: Option<&Bridge>,
        muted: bool,
    ) {
        if muted {
            return;
        }
        let body = match state {
            SessionState::Idle if self.config.on_complete => {
                format!("{session_name} is done")
            }
            SessionState::WaitingForInput if self.config.on_waiting => {
                format!("{session_name} needs input")
            }
            SessionState::Error if self.config.on_error => {
                format!("{session_name} hit an error")
            }
            _ => return,
        };
        self.send("nerve", &body, bridge, target);
    }

    fn send(&self, title: &str, body: &str, bridge: Option<&Bridge>, target: &SessionTarget) {
        #[cfg(target_os = "macos")]
        self.send_macos(title, body, bridge, target);

        #[cfg(target_os = "linux")]
        {
            let _ = (bridge, target);
            self.send_linux(title, body);
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (title, body, bridge, target);
        }
    }

    #[cfg(target_os = "macos")]
    fn send_macos(&self, title: &str, body: &str, bridge: Option<&Bridge>, target: &SessionTarget) {
        // Prefer terminal-notifier: supports click-to-focus via -execute.
        if self.try_terminal_notifier(title, body, bridge, target) {
            return;
        }

        let escaped_title = title.replace('\\', "\\\\").replace('"', "\\\"");
        let escaped_body = body.replace('\\', "\\\\").replace('"', "\\\"");

        let sound_clause = if self.config.sound {
            " sound name \"Funk\""
        } else {
            ""
        };

        let script = format!(
            "display notification \"{escaped_body}\" with title \"{escaped_title}\"{sound_clause}"
        );

        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    #[cfg(target_os = "macos")]
    fn try_terminal_notifier(
        &self,
        title: &str,
        body: &str,
        bridge: Option<&Bridge>,
        target: &SessionTarget,
    ) -> bool {
        let mut cmd = std::process::Command::new("terminal-notifier");
        cmd.args(["-title", title, "-message", body, "-group", "nerve"]);

        // On macOS Tahoe, terminal-notifier's -execute appears to need an
        // -activate companion to register its click handler. We always pass
        // -activate when we know the terminal bundle and additionally pass
        // -execute when we can resolve a tab/split id. The two compose: the
        // click fires both, focus_terminal raises Ghostty and selects the
        // exact split, -activate is a redundant safety net.
        if let Some(bundle_id) = self.terminal_app.as_deref().and_then(terminal_bundle_id) {
            cmd.args(["-activate", bundle_id]);
        }
        if let Some(execute) = focus_command(bridge, target) {
            cmd.args(["-execute", &execute]);
        }
        if self.config.sound {
            cmd.args(["-sound", "Funk"]);
        }

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    }

    #[cfg(target_os = "linux")]
    fn send_linux(&self, title: &str, body: &str) {
        let _ = std::process::Command::new("notify-send")
            .args([title, body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(target_os = "macos")]
fn focus_command(bridge: Option<&Bridge>, target: &SessionTarget) -> Option<String> {
    let id = bridge?.resolve_id(target)?;
    let exe = std::env::current_exe().ok()?;
    let exe_str = exe.to_str()?;
    Some(format!("{} --focus {}", shell_quote(exe_str), id))
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(target_os = "macos")]
fn terminal_bundle_id(app: &str) -> Option<&'static str> {
    match app {
        "Ghostty" => Some("com.mitchellh.ghostty"),
        "Terminal" => Some("com.apple.Terminal"),
        "iTerm2" => Some("com.googlecode.iterm2"),
        "Alacritty" => Some("org.alacritty"),
        "kitty" => Some("net.kovidgoyal.kitty"),
        "WezTerm" => Some("com.github.wez.wezterm"),
        _ => None,
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_wraps_simple_path() {
        assert_eq!(shell_quote("/usr/local/bin/nerve"), "'/usr/local/bin/nerve'");
    }

    #[test]
    fn shell_quote_handles_embedded_quote() {
        // bash idiom: 'foo'\''bar' is the safe single-quoted form of foo'bar
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn shell_quote_preserves_spaces() {
        assert_eq!(shell_quote("/Users/jane doe/bin/nerve"), "'/Users/jane doe/bin/nerve'");
    }
}
