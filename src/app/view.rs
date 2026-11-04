//! Turning the app into a frame: the page shown, its footer, and the
//! views the Sync and Settings pages draw from.

use std::time::Instant;

use ratatui::DefaultTerminal;

use super::fleet::{age, online};
use super::{App, Confirm, FLASH, FRAME};
use crate::tui::model::{self, short_age, Band, Status};
use crate::tui::settings::{DeviceLine, NotifyLine, PrivateLine, SettingsView};
use crate::tui::sync::SyncPage;
use crate::tui::{Body, DetailView, Footer, Page, View};
use crate::util::now_secs;

impl App {
    /// Draw a frame. Until nerve has found its own Ghostty tab, the window
    /// title is the probe's marker.
    pub(super) fn draw(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        self.ui.width = terminal.size()?.width;
        let need = self.rows.iter().filter(|r| r.status.band() == Band::Need).count();
        let title = match self.bridge.probe_title() {
            Some(marker) => marker.to_string(),
            None if need > 0 => format!("({need}) nerve"),
            None => "nerve".to_string(),
        };
        if title != self.ui.title {
            crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(&title))?;
            if self.bridge.probe_title().is_some() {
                self.bridge.title_shown();
            }
            self.ui.title = title;
        }
        let bands = model::arrange(&self.rows, self.ui.find.as_deref());
        let body = self.body();
        let footer = self.footer(&body);
        let now = Instant::now();
        let flash = self
            .ui
            .flash
            .as_ref()
            .filter(|(_, at)| now.duration_since(*at) < FLASH)
            .map(|(k, at)| (k, now.duration_since(*at) > FLASH / 2));
        let screen =
            self.ui.screen.as_ref().filter(|(k, _)| Some(k) == self.ui.cur.as_ref()).map(|(_, l)| l.as_slice());
        let sending =
            self.ui.sending.as_ref().filter(|(k, ..)| Some(k) == self.ui.cur.as_ref()).map(|(_, s, _)| s.as_str());
        let view = View {
            theme: self.theme(),
            page: self.ui.page,
            me: &self.me,
            others_online: self.others_online(),
            rows: &self.rows,
            bands: &bands,
            cur: self.ui.cur.as_ref(),
            open: self.ui.open,
            find: self.ui.find.as_deref(),
            flash,
            spin: (self.epoch.elapsed().as_millis() / FRAME.as_millis()) as usize,
            detail: DetailView {
                transcript: match &self.transcript_path {
                    Some(p) => self.transcripts.get(p).map(Vec::as_slice),
                    None => Some(&[]),
                },
                scroll: &self.ui.scroll,
                screen,
                compose: self.ui.compose.as_deref(),
                sending,
                can_compose: self.can_compose(),
            },
            body,
            footer,
            dialog: self.ui.dialog.as_ref(),
        };
        terminal.draw(|frame| crate::tui::draw(frame, &view))?;
        Ok(())
    }

    /// The page being shown, built from the app's state.
    pub(super) fn body(&self) -> Body<'_> {
        match self.ui.page {
            Page::Sessions => Body::Sessions,
            Page::Sync => Body::Sync(self.sync_page()),
            Page::Settings => Body::Settings(self.settings_view()),
        }
    }

    pub(super) fn footer(&self, body: &Body) -> Footer {
        if let Some(c) = &self.ui.confirm {
            return match c {
                Confirm::Stop { label, .. } => Footer::Confirm {
                    text: format!("Stop {label}?"),
                    hints: vec![("y", "Stop".into()), ("Any other key", "Cancel".into())],
                },
                Confirm::Capture { .. } => Footer::Confirm {
                    text: "Capturing switches to its tab for a moment.".into(),
                    hints: vec![
                        ("y", "Capture".into()),
                        ("d", "Don't ask again".into()),
                        ("Any other key", "Cancel".into()),
                    ],
                },
                Confirm::StopSync { path, .. } => Footer::Confirm {
                    text: format!("Stop syncing {path}?"),
                    hints: vec![("y", "Stop syncing".into()), ("Any other key", "Cancel".into())],
                },
                Confirm::Forget { device } => Footer::Confirm {
                    text: format!("Forget {device}? None of your devices will trust it."),
                    hints: vec![("y", "Forget".into()), ("Any other key", "Cancel".into())],
                },
            };
        }
        if self.ui.finding {
            return Footer::Find(self.ui.find.clone().unwrap_or_default());
        }
        if let Some((t, _)) = &self.ui.toast {
            return Footer::Toast(t.clone());
        }
        if self.ui.dialog.is_some() {
            return Footer::Hints(Vec::new());
        }
        let h = |k: &'static str, v: &str| (k, v.to_string());
        let hints = match body {
            Body::Sync(page) => {
                let mut v = vec![h("j k", "Move"), h("s", "Sync now"), h("a", "Sync a folder")];
                if !page.running {
                    v = vec![h("c", "Connect a device")];
                } else if page.folders.is_empty() {
                    v.drain(..2);
                } else {
                    v.extend([h("o", "Other folders"), h("v", "Versions"), h("x", "Stop syncing")]);
                }
                v.extend([h("Tab", "Next page"), h("?", "Keys")]);
                v
            }
            Body::Settings(view) => {
                use crate::tui::settings::Item;
                let mut v = match view.item() {
                    Some(Item::Notify(_)) => vec![h("← →", "Column"), h("Space", "Toggle"), h("Tab", "Next page")],
                    Some(Item::Private(_)) => vec![h("u", "Share again"), h("Tab", "Next page")],
                    Some(Item::Device(i)) if view.devices.get(i).is_some_and(|d| d.name != self.me) => vec![
                        h("l", "Connect to more"),
                        h("f", "Forget"),
                        h("c", "Connect a device"),
                        h("Tab", "Next page"),
                    ],
                    Some(Item::Device(_)) => vec![h("c", "Connect a device"), h("Tab", "Next page")],
                    Some(Item::Connect) => vec![h("↵", "Connect a device"), h("Tab", "Next page")],
                    _ => vec![h("← →", "Theme"), h("Tab", "Next page")],
                };
                v.push(h("?", "Keys"));
                v
            }
            Body::Sessions => {
                if self.ui.compose.is_some() {
                    let working = self.cur_row().is_some_and(|r| r.status == Status::Working);
                    return Footer::Hints(vec![h("↵", if working { "Queue" } else { "Send" }), h("Esc", "Cancel")]);
                }
                let Some(row) = self.cur_row() else {
                    let mut v = if self.claude_installed { vec![h("/", "Find")] } else { Vec::new() };
                    if self.can_compose() {
                        v.push(h("s", "Start a session"));
                    }
                    v.extend([h("Tab", "Next page"), h("?", "Keys"), h("q", "Quit")]);
                    return Footer::Hints(v);
                };
                if row.status == Status::Request {
                    return Footer::Hints(vec![
                        h("y", "Allow"),
                        h("↵", "Review"),
                        h("n", "Refuse"),
                        h("/", "Find"),
                        h("?", "Keys"),
                    ]);
                }
                let mut v = Vec::new();
                if let Some(ask) = row.ask.as_ref().filter(|_| row.status.band() == Band::Need) {
                    let n = ask.options.len();
                    v.push((if n > 1 { "1–9" } else { "1" }, "Answer".to_string()));
                }
                if !self.ui.open {
                    v.push(h("↵", "Open"));
                } else if self.can_compose() && row.can_send {
                    v.push(h("i", "Reply"));
                }
                v.push(h("n", if row.notify { "Notifying" } else { "Notify" }));
                if row.is_local() && self.can_private() {
                    v.push(h("p", if row.private { "Share" } else { "Private" }));
                }
                if row.is_local() {
                    v.push(h("t", "Terminal"));
                }
                if self.ui.open && row.status == Status::Working {
                    v.push(h("x", "Stop"));
                }
                v.push(if self.ui.open || self.ui.screen.is_some() {
                    h("Esc", "Back")
                } else if self.ui.find.is_some() {
                    h("Esc", "Clear")
                } else {
                    h("/", "Find")
                });
                v.extend([h("Tab", "Next page"), h("?", "Keys")]);
                v
            }
        };
        Footer::Hints(hints)
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

    fn sync_page(&self) -> SyncPage<'_> {
        let folders = self.folders();
        SyncPage {
            me: &self.me,
            running: self.sync.is_some() && self.agent.running,
            folders,
            cursor: self.ui.sync_cursor.min(folders.len().saturating_sub(1)),
            bulk_over: self.agent.bulk_over,
            peers: self.peer_ages(),
            now: now_secs(),
            waiting: self.folder_waits(),
        }
    }
}
