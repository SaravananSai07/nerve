use crate::config::NotificationConfig;
use crate::state::session::SessionState;

pub struct Notifier {
    config: NotificationConfig,
    terminal_app: Option<String>,
}

impl Notifier {
    pub fn new(config: NotificationConfig, terminal_app: Option<String>) -> Self {
        Self { config, terminal_app }
    }

    pub fn maybe_notify(&self, session_name: &str, state: &SessionState, muted: bool) {
        if muted {
            return;
        }
        match state {
            SessionState::Idle if self.config.on_complete => {
                self.send("nerve", &format!("{session_name} is done"));
            }
            SessionState::WaitingForInput if self.config.on_waiting => {
                self.send("nerve", &format!("{session_name} needs input"));
            }
            SessionState::Error if self.config.on_error => {
                self.send("nerve", &format!("{session_name} hit an error"));
            }
            _ => {}
        }
    }

    fn send(&self, title: &str, body: &str) {
        #[cfg(target_os = "macos")]
        self.send_macos(title, body);

        #[cfg(target_os = "linux")]
        self.send_linux(title, body);

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (title, body);
        }
    }

    #[cfg(target_os = "macos")]
    fn send_macos(&self, title: &str, body: &str) {
        // Prefer terminal-notifier: supports click-to-activate the terminal app.
        if self.try_terminal_notifier(title, body) {
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
    fn try_terminal_notifier(&self, title: &str, body: &str) -> bool {
        let mut cmd = std::process::Command::new("terminal-notifier");
        cmd.args(["-title", title, "-message", body, "-group", "nerve"]);

        if let Some(bundle_id) = self.terminal_app.as_deref().and_then(terminal_bundle_id) {
            cmd.args(["-activate", bundle_id]);
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
