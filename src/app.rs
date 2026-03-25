use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::config::Config;
use crate::detect::claude;
use crate::platform::Bridge;
use crate::state::registry::SessionRegistry;
use crate::state::session::SessionState;
use crate::tui::{cards, help, rename};
use crate::tui::theme::Theme;

enum Overlay {
    None,
    Help,
    Rename(String),
}

pub struct App {
    config: Config,
    registry: SessionRegistry,
    theme: Theme,
    theme_index: usize,
    bridge: Option<Bridge>,
    selected: usize,
    cols: usize,
    overlay: Overlay,
    status_message: Option<String>,
    should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        let config = Config::load();
        let theme_name = &config.appearance.theme;
        let theme_index = crate::tui::theme::THEME_NAMES
            .iter()
            .position(|&n| n == theme_name)
            .unwrap_or(0);
        let theme = Theme::by_name(theme_name);
        let bridge = Bridge::auto_detect();
        Self {
            config,
            registry: SessionRegistry::new(),
            theme,
            theme_index,
            bridge,
            selected: 0,
            cols: 2,
            overlay: Overlay::None,
            status_message: None,
            should_quit: false,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        self.refresh_sessions();

        while !self.should_quit {
            terminal.draw(|frame| {
                let area = frame.area();
                self.cols = if area.width >= 80 { 2 } else { 1 };
                cards::render(
                    frame,
                    area,
                    &self.registry,
                    self.selected,
                    &self.theme,
                    self.status_message.as_deref(),
                );
                match &self.overlay {
                    Overlay::Help => help::render(frame, &self.theme),
                    Overlay::Rename(buf) => rename::render(frame, &self.theme, buf),
                    Overlay::None => {}
                }
            })?;

            if event::poll(Duration::from_millis(self.config.general.refresh_interval_ms))? {
                if let Event::Key(key) = event::read()? {
                    self.handle_key(key.code, key.modifiers);
                }
            } else {
                self.tick();
            }
        }

        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match &self.overlay {
            Overlay::Help => {
                match code {
                    KeyCode::Char('?') | KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Char('q') => self.should_quit = true,
                    _ => {}
                }
                return;
            }
            Overlay::Rename(_) => {
                self.handle_rename_key(code);
                return;
            }
            Overlay::None => {}
        }

        self.status_message = None;
        let session_count = self.registry.len();

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let next = self.selected + self.cols;
                if session_count > 0 && next < session_count {
                    self.selected = next;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected >= self.cols {
                    self.selected -= self.cols;
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let col = self.selected % self.cols;
                if col > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let col = self.selected % self.cols;
                if col + 1 < self.cols && self.selected + 1 < session_count {
                    self.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('g') => {
                self.go_to_selected_tab();
            }
            KeyCode::Char('s') => {
                self.registry.cycle_sort();
            }
            KeyCode::Char('t') => {
                self.cycle_theme();
            }
            KeyCode::Char('n') => {
                self.start_rename();
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as usize) - ('1' as usize);
                if idx < session_count {
                    self.selected = idx;
                }
            }
            _ => {}
        }
    }

    fn start_rename(&mut self) {
        let sessions = self.registry.sorted_sessions();
        if let Some(session) = sessions.get(self.selected) {
            self.overlay = Overlay::Rename(session.name.clone());
        }
    }

    fn handle_rename_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => {
                if let Overlay::Rename(buf) = std::mem::replace(&mut self.overlay, Overlay::None) {
                    let trimmed = buf.trim().to_string();
                    if !trimmed.is_empty() {
                        self.commit_rename(trimmed);
                    }
                }
            }
            KeyCode::Esc => {
                self.overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                if let Overlay::Rename(ref mut buf) = self.overlay {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Overlay::Rename(ref mut buf) = self.overlay {
                    if buf.len() < 48 {
                        buf.push(c);
                    }
                }
            }
            _ => {}
        }
    }

    fn commit_rename(&mut self, new_name: String) {
        let sessions = self.registry.sorted_sessions();
        let Some(id) = sessions.get(self.selected).map(|s| s.id.clone()) else {
            return;
        };
        if let Some(session) = self.registry.get_mut(&id) {
            session.name = new_name;
            session.renamed = true;
        }
    }

    fn cycle_theme(&mut self) {
        let names = crate::tui::theme::THEME_NAMES;
        self.theme_index = (self.theme_index + 1) % names.len();
        self.theme = Theme::by_name(names[self.theme_index]);
    }

    fn go_to_selected_tab(&mut self) {
        let sessions = self.registry.sorted_sessions();
        let Some(session) = sessions.get(self.selected) else {
            return;
        };

        let cwd = session.cwd.to_string_lossy().into_owned();
        let dir_name = session
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = session.name.clone();

        if let Some(ref bridge) = self.bridge {
            if let Err(e) = bridge.go_to_session(&cwd, &name, &dir_name) {
                self.status_message = Some(e.to_string());
            }
        } else {
            self.status_message = Some("no terminal bridge detected (try Ghostty or tmux)".into());
        }
    }

    fn tick(&mut self) {
        self.refresh_sessions();
        self.registry.remove_stale(60);
    }

    fn refresh_sessions(&mut self) {
        let discovered = claude::discover_sessions();
        let active_ids: std::collections::HashSet<&str> =
            discovered.iter().map(|s| s.id.as_str()).collect();

        let stale_ids: Vec<String> = self
            .registry
            .ids()
            .iter()
            .filter(|id| !active_ids.contains(id.as_str()))
            .cloned()
            .collect();
        for id in stale_ids {
            self.registry.mark_stale(&id);
        }

        for session in discovered {
            let detected_state = session.state.clone();
            let id = session.id.clone();
            let cwd_str = session.cwd.to_string_lossy().into_owned();

            if let Some(existing) = self.registry.get_mut(&id) {
                existing.cpu_percent = session.cpu_percent;
                existing.tty = session.tty;
                existing.branch = session.branch;

                if !existing.renamed {
                    if let Some(override_name) = self.config.session_name_for(&cwd_str) {
                        existing.name = override_name.clone();
                    }
                }

                if detected_state == SessionState::Processing
                    || matches!(detected_state, SessionState::ToolRunning(_))
                {
                    existing.activity.record_activity();
                }

                if let SessionState::ToolRunning(ref tool) = detected_state {
                    existing.current_tool = Some(tool.clone());
                }

                existing.propose_state(detected_state);
            } else {
                let mut new_session = session;
                if let Some(override_name) = self.config.session_name_for(&cwd_str) {
                    new_session.name = override_name.clone();
                }
                self.registry.upsert(new_session);
            }
        }

        self.registry.shift_all_activity();

        let count = self.registry.len();
        if count > 0 && self.selected >= count {
            self.selected = count - 1;
        }
    }
}
