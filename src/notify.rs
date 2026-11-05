//! Desktop notifications, and who hears about what (`<config>/notify.toml`).
//!
//! Clicking a notification opens nerve on the session it is about
//! (`nerve --land`), raising nerve's own tab when one is running.
//!
//! Questions, permission prompts and failures notify every device unless
//! it is in `quiet`. A finished turn notifies the devices ticked under
//! `finished`, unless the session has its own choice (`n`); never the
//! device where nerve has that session open. Before sync is set up there
//! is one device, this one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::platform::BridgeId;

/// What a notification is about. The title names it and the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Alert {
    Question,
    Permission,
    Failed,
    Finished,
    /// A device or folder waiting for a person's yes.
    Request,
}

impl Alert {
    pub(crate) fn title(self, device: &str) -> String {
        let what = match self {
            Self::Question => "Question",
            Self::Permission => "Permission",
            Self::Failed => "Failed",
            Self::Finished => "Finished",
            Self::Request => "Request",
        };
        format!("{what} on {device}")
    }

    /// A session waiting for input: a permission prompt when Claude says
    /// so, otherwise a question.
    pub(crate) fn waiting(why: Option<&str>) -> Self {
        if why.is_some_and(|w| w.to_ascii_lowercase().contains("permission")) {
            Self::Permission
        } else {
            Self::Question
        }
    }
}

/// Where a click on a notification lands. Only macOS notifications run
/// a command on click.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Click {
    pub(crate) device: String,
    pub(crate) session: String,
    /// nerve's own tab or pane, when a TUI is running.
    pub(crate) focus: Option<BridgeId>,
}

pub(crate) struct Notifier {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    sound: bool,
}

impl Notifier {
    pub(crate) fn new(sound: bool) -> Self {
        Self { sound }
    }

    /// Fire a notification from a named thread: the commands can take a
    /// moment, the UI thread must not wait, and `.status()` reaps them.
    pub(crate) fn send(&self, title: &str, body: &str, click: &Click) {
        #[cfg(target_os = "macos")]
        let job = MacJob {
            sound: self.sound,
            ghostty: matches!(click.focus, Some(BridgeId::Ghostty(_))),
            execute: land_command(click),
        };
        #[cfg(not(target_os = "macos"))]
        let _ = click;
        let (title, body) = (title.to_string(), body.to_string());
        let spawned = std::thread::Builder::new().name("nerve-notify".into()).spawn(move || {
            #[cfg(target_os = "macos")]
            job.run(&title, &body);
            #[cfg(target_os = "linux")]
            send_linux(&title, &body);
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            let _ = (title, body);
        });
        if let Err(e) = spawned {
            crate::log_warn!("notify: cannot start thread: {e}");
        }
    }
}

#[cfg(target_os = "macos")]
struct MacJob {
    sound: bool,
    ghostty: bool,
    execute: Option<String>,
}

#[cfg(target_os = "macos")]
impl MacJob {
    fn run(&self, title: &str, body: &str) {
        // Prefer terminal-notifier: it runs a command on click.
        if self.try_terminal_notifier(title, body) {
            return;
        }
        // Everything user-controlled flows through `argv`. Nothing is
        // interpolated into the script body — eliminates AppleScript
        // injection via session names or cwds.
        const SCRIPT_SOUND: &str = r#"on run argv
    display notification (item 2 of argv) with title (item 1 of argv) sound name "Funk"
end run"#;
        const SCRIPT_SILENT: &str = r#"on run argv
    display notification (item 2 of argv) with title (item 1 of argv)
end run"#;
        let script = if self.sound { SCRIPT_SOUND } else { SCRIPT_SILENT };
        let _ = crate::platform::run_applescript(script, &[title, body]);
    }

    fn try_terminal_notifier(&self, title: &str, body: &str) -> bool {
        let mut cmd = std::process::Command::new("terminal-notifier");
        cmd.args(["-title", title, "-message", body, "-group", "nerve"]);
        // On macOS Tahoe, terminal-notifier's -execute needs an -activate
        // companion to register its click handler.
        if self.ghostty {
            cmd.args(["-activate", "com.mitchellh.ghostty"]);
        }
        if let Some(execute) = &self.execute {
            cmd.args(["-execute", execute]);
        }
        if self.sound {
            cmd.args(["-sound", "Funk"]);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

#[cfg(target_os = "linux")]
fn send_linux(title: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args([title, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// `nerve --land <device>/<session> [--focus <id>]`. Every part is checked
/// against a strict charset before it reaches a shell: device names come
/// from fleet.toml, which a peer's name ends up in.
#[cfg(target_os = "macos")]
fn land_command(click: &Click) -> Option<String> {
    let safe = crate::util::is_safe;
    if !safe(&click.device) || !safe(&click.session) {
        crate::log_warn!("notify: refusing click payload {}/{}", click.device, click.session);
        return None;
    }
    let land = format!("{}/{}", click.device, click.session);
    let exe = std::env::current_exe().ok()?;
    let quote = crate::util::text::shell_quote;
    let mut cmd = format!("{} --land {}", quote(exe.to_str()?), quote(&land));
    if let Some(id) = click.focus.as_ref().map(ToString::to_string).filter(|id| safe(id)) {
        cmd.push_str(&format!(" --focus {}", quote(&id)));
    }
    Some(cmd)
}

const BOOK_MAX_BYTES: u64 = 64 * 1024;

/// The fleet-wide part. Every device keeps a copy; once synced, the copy
/// with the newest `updated_at` wins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct NotifyPrefs {
    /// Devices that don't show questions, permission prompts and failures.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) quiet: BTreeSet<String>,
    /// Devices told whenever a session finishes a turn, unless the session
    /// has its own choice.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) finished: BTreeSet<String>,
    /// Unix seconds of the last change.
    pub(crate) updated_at: u64,
}

impl NotifyPrefs {
    pub(crate) fn attention_on(&self, device: &str) -> bool {
        !self.quiet.contains(device)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct NotifyBook {
    pub(crate) prefs: NotifyPrefs,
    /// Sessions running on this device with their own choice of devices to
    /// notify when a turn ends; empty switches the session off. The rest
    /// follow `prefs.finished`. A book from before Settings did kept only
    /// sessions switched on, which carry on as they were.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) sessions: BTreeMap<String, BTreeSet<String>>,
}

impl NotifyBook {
    pub(crate) fn file(config_dir: &Path) -> PathBuf {
        crate::paths::Paths::at(config_dir.to_path_buf()).notify_file()
    }

    /// A missing or unreadable file is an empty book: everything notifies
    /// on attention, nothing on finish.
    pub(crate) fn load(config_dir: &Path) -> Self {
        crate::util::fs::load_toml(&Self::file(config_dir), BOOK_MAX_BYTES)
            .unwrap_or_else(|e| {
                crate::log_warn!("notify.toml: {e}");
                None
            })
            .unwrap_or_default()
    }

    pub(crate) fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir)?;
        let body = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = config_dir.join("notify.toml.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, Self::file(config_dir))
    }

    /// Who hears when `session` finishes a turn; empty turns it off. The
    /// devices ticked in Settings drop the session's own choice, so it
    /// follows Settings again.
    pub(crate) fn set_session(&mut self, session: &str, devices: BTreeSet<String>) {
        if devices == self.prefs.finished {
            self.sessions.remove(session);
        } else {
            self.sessions.insert(session.to_string(), devices);
        }
    }

    /// Who hears when `session` finishes a turn: its own choice, else Settings.
    pub(crate) fn finished_to(&self, session: &str) -> &BTreeSet<String> {
        self.sessions.get(session).unwrap_or(&self.prefs.finished)
    }
}

/// Does `me` show that a session finished a turn? `to`: who it notifies
/// (`NotifyBook::finished_to`). Not while `me` has the session open.
pub(crate) fn tell_finished<'a>(to: impl IntoIterator<Item = &'a String>, me: &str, open_here: bool) -> bool {
    !open_here && to.into_iter().any(|d| d == me)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_name_the_alert_and_device() {
        assert_eq!(Alert::Finished.title("server"), "Finished on server");
        assert_eq!(Alert::waiting(Some("Permission prompt")), Alert::Permission);
        assert_eq!(Alert::waiting(Some("Answer 3 questions")), Alert::Question);
        assert_eq!(Alert::waiting(None), Alert::Question);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn land_command_refuses_shell_metacharacters() {
        let click = |device: &str| Click { device: device.into(), session: "ab12".into(), focus: None };
        assert!(land_command(&click("laptop")).is_some_and(|c| c.contains("--land 'laptop/ab12'")));
        assert!(land_command(&click("lap'top")).is_none());
        assert!(land_command(&click("a;b")).is_none());
    }

    #[test]
    fn attention_is_on_unless_quiet() {
        let mut p = NotifyPrefs::default();
        assert!(p.attention_on("laptop"));
        p.quiet.insert("laptop".into());
        assert!(!p.attention_on("laptop"));
        assert!(p.attention_on("studio"));
    }

    #[test]
    fn finished_follows_settings_unless_the_session_chose() {
        let me = || BTreeSet::from(["laptop".to_string()]);
        for ticked in [false, true] {
            for own in [None, Some(BTreeSet::new()), Some(me())] {
                for open_here in [false, true] {
                    let mut b = NotifyBook::default();
                    if ticked {
                        b.prefs.finished = me();
                    }
                    if let Some(own) = own.clone() {
                        b.sessions.insert("s1".into(), own);
                    }
                    let want = !open_here && own.as_ref().map_or(ticked, |o| !o.is_empty());
                    let got = tell_finished(b.finished_to("s1"), "laptop", open_here);
                    assert_eq!(got, want, "ticked {ticked}, own {:?}, open {open_here}", b.sessions.get("s1"));
                }
            }
        }
        assert!(!tell_finished(&me(), "studio", false), "only the devices listed");
    }

    #[test]
    fn a_choice_matching_settings_follows_settings() {
        let mut b = NotifyBook::default();
        b.prefs.finished = BTreeSet::from(["laptop".to_string()]);
        b.set_session("s1", BTreeSet::new());
        assert!(b.finished_to("s1").is_empty(), "off overrides Settings");
        b.set_session("s1", BTreeSet::from(["laptop".to_string()]));
        assert!(b.sessions.is_empty());
        b.prefs.finished.insert("studio".into());
        assert_eq!(b.finished_to("s1").len(), 2, "and follows later changes");
    }

    #[test]
    fn round_trips_and_keeps_old_books_notifying() {
        let dir = tempfile::tempdir().unwrap();
        // A book from before: only sessions switched on, no Settings tick.
        std::fs::write(NotifyBook::file(dir.path()), "[prefs]\nupdated_at = 7\n\n[sessions]\ns1 = [\"laptop\"]\n")
            .unwrap();
        let mut b = NotifyBook::load(dir.path());
        assert!(tell_finished(b.finished_to("s1"), "laptop", false));
        assert!(!tell_finished(b.finished_to("s2"), "laptop", false));
        b.prefs.finished.insert("laptop".into());
        b.set_session("s2", BTreeSet::new());
        b.save(dir.path()).unwrap();
        assert_eq!(NotifyBook::load(dir.path()), b);
        assert_eq!(NotifyBook::load(&dir.path().join("missing")), NotifyBook::default());
    }
}
