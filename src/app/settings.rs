//! The Settings page, end to end: its keys, view, footer hints, theme and
//! notification columns, and the device actions (forget, connect to more).

use crossterm::event::KeyCode;

use super::dialog::toggle;
use super::fleet::{age, online};
use super::App;
use crate::sync::agent::FleetReq;
use crate::tui::dialog::{Dialog, WhoMode};
use crate::tui::model::short_age;
use crate::tui::settings::{DeviceLine, Item, NotifyLine, PrivateLine, SettingsView};
use crate::tui::Page;

impl App {
    pub(super) fn settings_key(&mut self, code: KeyCode) {
        let view = self.settings_view();
        let n = view.items().len();
        // Another device under the cursor: l connects it to more, f forgets it.
        if let (Some(Item::Device(i)), KeyCode::Char(c @ ('f' | 'l'))) = (view.item(), code) {
            if let Some(d) = view.devices.get(i).map(|d| d.name.clone()).filter(|d| *d != self.me) {
                return if c == 'f' { self.ask_forget(d) } else { self.start_link(d) };
            }
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.ui.settings_cursor = (self.ui.settings_cursor + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => self.ui.settings_cursor = self.ui.settings_cursor.saturating_sub(1),
            KeyCode::Esc => self.ui.page = Page::Sessions,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                let forward = matches!(code, KeyCode::Right | KeyCode::Char('l'));
                match view.item() {
                    Some(Item::Theme) => self.cycle_theme(forward),
                    Some(Item::Notify(_)) => self.ui.settings_col = u8::from(forward),
                    _ => {}
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => match view.item() {
                Some(Item::Notify(i)) => self.toggle_notify_pref(&view.notify[i].device),
                Some(Item::Theme) => self.cycle_theme(true),
                Some(Item::Connect) => self.start_connect(),
                _ => {}
            },
            KeyCode::Char('u') => {
                if let Some(Item::Private(i)) = view.item() {
                    self.share_again(i);
                }
            }
            KeyCode::Char('c') => self.start_connect(),
            _ => {}
        }
    }

    pub(super) fn settings_view(&self) -> SettingsView {
        let prefs = self.notify_prefs();
        let notify = self
            .devices()
            .into_iter()
            .map(|(device, note)| NotifyLine {
                attention: prefs.attention_on(&device),
                finished: prefs.finished.contains(&device),
                device,
                note,
            })
            .collect();
        let mut v = SettingsView {
            me: self.me.clone(),
            notify,
            theme: self.theme().name.as_str().to_string(),
            col: self.ui.settings_col,
            ..Default::default()
        };
        self.fill_fleet_settings(&mut v);
        v.cursor = self.ui.settings_cursor.min(v.items().len().saturating_sub(1));
        v
    }

    fn fill_fleet_settings(&self, v: &mut SettingsView) {
        v.devices.push(DeviceLine {
            name: self.me.clone(),
            kind: crate::util::os_name(std::env::consts::OS),
            online: true,
            status: "This device".into(),
        });
        if self.sync.is_none() {
            return;
        }
        v.sync = true;
        for p in &self.agent.peers {
            let age = age(p.state_generated_at);
            let online = online(p);
            v.devices.push(DeviceLine {
                name: p.name.clone(),
                kind: p
                    .state
                    .as_ref()
                    .map(|s| crate::util::os_name(crate::util::sanitize::Sanitised::new(&s.os).as_str()))
                    .unwrap_or_default(),
                online,
                status: match age {
                    _ if online => "Online".into(),
                    Some(a) => format!("Last seen {} ago", short_age(a)),
                    None => "Not seen yet".into(),
                },
            });
        }
        v.private = self
            .agent
            .private
            .iter()
            .map(|f| PrivateLine { label: crate::paths::tilde(f), folder: true })
            .chain(
                self.agent
                    .private_sessions
                    .iter()
                    .map(|id| PrivateLine { label: self.session_label(id), folder: false }),
            )
            .collect();
        let up = self.agent.relays.iter().filter(|r| r.connected).count();
        let mut c = match &self.agent.error {
            Some(e) => format!("Sync can't connect: {e}. Trying again."),
            None => format!(
                "{up} of {} relays connected. Only your connected devices can read what is sent.",
                self.agent.relays.len()
            ),
        };
        // Hand-offs on their way, in either direction.
        for (title, done, total) in &self.agent.transfers {
            c.push_str(&format!(
                " {}: {done} of {total} parts.",
                crate::util::sanitize::Sanitised::new(title.as_str())
            ));
        }
        v.connection = Some(c);
    }

    pub(super) fn cycle_theme(&mut self, forward: bool) {
        let n = self.themes.len();
        self.theme_index = if forward { (self.theme_index + 1) % n } else { (self.theme_index + n - 1) % n };
        self.prefs.theme = Some(self.themes[self.theme_index].name.as_str().to_string());
        self.prefs.save(&self.paths);
    }

    /// Flip the focused column of a device's row on the Settings page.
    pub(super) fn toggle_notify_pref(&mut self, device: &str) {
        let mut prefs = self.notify_prefs();
        if self.ui.settings_col == 0 {
            toggle(&mut prefs.quiet, device);
        } else {
            toggle(&mut prefs.finished, device);
        }
        self.set_notify_prefs(prefs);
    }
    /// `f` on a device in Settings.
    pub(super) fn ask_forget(&mut self, device: String) {
        self.ui.confirm = Some(super::Confirm::Forget { device });
    }

    pub(super) fn forget(&mut self, device: String) {
        if self.fleet_request(FleetReq::Forget { device: device.clone() }) {
            self.say(format!("Forgot {device}. None of your devices trust it now."));
        }
    }

    /// `l` on a device in Settings: connect it to more of your devices.
    pub(super) fn start_link(&mut self, device: String) {
        let devices: Vec<String> = self.peer_names().into_iter().filter(|n| *n != device).collect();
        self.ui.dialog = Some(Dialog::Who {
            link: Some(device),
            set: devices.iter().cloned().collect(),
            devices,
            mode: WhoMode::AllLater,
            i: 0,
        });
    }

    pub(super) fn link(&mut self, device: String, with: Vec<String>) {
        let all = with.is_empty();
        if self.fleet_request(FleetReq::Link { device: device.clone(), with }) {
            self.say(if all {
                format!("Connecting {device} to all your devices, and ones you add later.")
            } else {
                format!("Connecting {device}.")
            });
        }
    }

    /// The footer's hints on the Settings page.
    pub(super) fn settings_hints(&self, view: &SettingsView) -> Vec<(&'static str, String)> {
        let h = |k: &'static str, v: &str| (k, v.to_string());
        use crate::tui::settings::Item;
        let mut v = match view.item() {
            Some(Item::Notify(_)) => vec![h("← →", "Column"), h("Space", "Toggle"), h("Tab", "Next page")],
            Some(Item::Private(_)) => vec![h("u", "Share again"), h("Tab", "Next page")],
            Some(Item::Device(i)) if view.devices.get(i).is_some_and(|d| d.name != self.me) => {
                vec![h("l", "Connect to more"), h("f", "Forget"), h("c", "Connect a device"), h("Tab", "Next page")]
            }
            Some(Item::Device(_)) => vec![h("c", "Connect a device"), h("Tab", "Next page")],
            Some(Item::Connect) => vec![h("↵", "Connect a device"), h("Tab", "Next page")],
            _ => vec![h("← →", "Theme"), h("Tab", "Next page")],
        };
        v.push(h("?", "Keys"));
        v
    }
}
