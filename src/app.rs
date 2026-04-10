use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers, EnableFocusChange, DisableFocusChange};
use crossterm::execute;
use ratatui::DefaultTerminal;

use crate::config::Config;
use crate::detect::claude::{self, LogEntry};
use crate::notify::Notifier;
use crate::platform::{Bridge, SessionTarget};
use crate::state::prefs::Prefs;
use crate::state::registry::SessionRegistry;
use crate::state::session::SessionState;
use crate::tui::{cards, confirm_kill, confirm_preview, help, preview, rename};
use crate::tui::theme::Theme;

enum Overlay {
    None,
    Help,
    Rename(String),
    Preview,
    ConfirmKill { name: String, id: String },
    ConfirmPreview,
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
    notifier: Notifier,
    prefs: Prefs,
    preview_scroll: usize,
    preview_entries: Vec<LogEntry>,
    preview_lines: Vec<String>,
    has_terminal_capture: bool,
    visited_session: Option<String>,
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
        let terminal_app = match &bridge {
            #[cfg(target_os = "macos")]
            Some(Bridge::Ghostty(_)) => Some("Ghostty".to_string()),
            _ => None,
        };
        let notifier = Notifier::new(config.notifications.clone(), terminal_app);
        let prefs = Prefs::load();
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
            notifier,
            prefs,
            preview_scroll: 0,
            preview_entries: Vec::new(),
            preview_lines: Vec::new(),
            has_terminal_capture: false,
            visited_session: None,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        execute!(std::io::stdout(), EnableFocusChange)?;
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
                    self.prefs.notifications_muted,
                );
                match &self.overlay {
                    Overlay::Help => help::render(frame, &self.theme),
                    Overlay::Rename(buf) => rename::render(frame, &self.theme, buf),
                    Overlay::Preview => {
                        let sessions = self.registry.sorted_sessions();
                        if let Some(session) = sessions.get(self.selected) {
                            preview::render(
                                frame,
                                &self.theme,
                                session,
                                &self.preview_entries,
                                &self.preview_lines,
                                self.has_terminal_capture,
                                &mut self.preview_scroll,
                            );
                        }
                    }
                    Overlay::ConfirmKill { ref name, .. } => {
                        confirm_kill::render(frame, &self.theme, name);
                    }
                    Overlay::ConfirmPreview => {
                        confirm_preview::render(frame, &self.theme);
                    }
                    Overlay::None => {}
                }
            })?;

            if event::poll(Duration::from_millis(self.config.general.refresh_interval_ms))? {
                match event::read()? {
                    Event::Key(key) => self.handle_key(key.code, key.modifiers),
                    Event::FocusGained => {
                        if let Some(name) = self.visited_session.take() {
                            self.status_message = Some(format!("returned from '{name}'"));
                        }
                    }
                    _ => {}
                }
            } else {
                self.tick();
            }
        }

        execute!(std::io::stdout(), DisableFocusChange)?;
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
            Overlay::Preview => {
                match code {
                    KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Char('q') => self.should_quit = true,
                    KeyCode::Char('j') | KeyCode::Down => self.preview_scroll += 1,
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.preview_scroll = self.preview_scroll.saturating_sub(1);
                    }
                    _ => {}
                }
                return;
            }
            Overlay::ConfirmKill { .. } => {
                match code {
                    KeyCode::Char('y') => {
                        if let Overlay::ConfirmKill { name, id } =
                            std::mem::replace(&mut self.overlay, Overlay::None)
                        {
                            self.execute_kill(&name, &id);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Esc => self.overlay = Overlay::None,
                    _ => {}
                }
                return;
            }
            Overlay::ConfirmPreview => {
                self.handle_confirm_preview_key(code);
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
            KeyCode::Char('p') => {
                self.open_log_preview();
            }
            KeyCode::Char('P') => {
                self.open_preview();
            }
            KeyCode::Char('x') => {
                self.start_kill();
            }
            KeyCode::Char('m') => {
                self.prefs.notifications_muted = !self.prefs.notifications_muted;
                self.prefs.save();
                self.status_message = Some(if self.prefs.notifications_muted {
                    "notifications muted".into()
                } else {
                    "notifications unmuted".into()
                });
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

    fn open_preview(&mut self) {
        if self.bridge.is_none() {
            self.open_log_preview();
            return;
        }
        if self.prefs.preview_flicker_accepted {
            self.execute_preview_capture();
            return;
        }
        self.overlay = Overlay::ConfirmPreview;
    }

    fn handle_confirm_preview_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') => {
                self.overlay = Overlay::None;
                self.execute_preview_capture();
            }
            KeyCode::Char('d') => {
                self.prefs.preview_flicker_accepted = true;
                self.prefs.save();
                self.overlay = Overlay::None;
                self.execute_preview_capture();
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.overlay = Overlay::None;
                self.open_log_preview();
            }
            _ => {}
        }
    }

    fn execute_preview_capture(&mut self) {
        let sessions = self.registry.sorted_sessions();
        let Some(session) = sessions.get(self.selected) else {
            return;
        };

        let target = SessionTarget {
            cwd: session.cwd.to_string_lossy().into_owned(),
            name: session.name.clone(),
            dir_name: session
                .cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            tty: session.tty.clone(),
        };
        let jsonl_path = session.jsonl_path.clone();

        if let Some(ref bridge) = self.bridge {
            if let Some(text) = bridge.capture_screen(&target) {
                self.preview_lines = text.lines().map(|l| l.to_string()).collect();
                self.preview_entries = Vec::new();
                self.has_terminal_capture = true;
                self.preview_scroll = usize::MAX;
                self.overlay = Overlay::Preview;
                return;
            }
        }

        self.load_log_entries(&jsonl_path);
        self.has_terminal_capture = false;
        self.preview_scroll = usize::MAX;
        self.overlay = Overlay::Preview;
    }

    fn open_log_preview(&mut self) {
        let sessions = self.registry.sorted_sessions();
        if let Some(session) = sessions.get(self.selected) {
            let jsonl_path = session.jsonl_path.clone();
            self.load_log_entries(&jsonl_path);
        } else {
            self.preview_entries = Vec::new();
        }
        self.preview_lines = Vec::new();
        self.has_terminal_capture = false;
        self.preview_scroll = usize::MAX;
        self.overlay = Overlay::Preview;
    }

    fn load_log_entries(&mut self, jsonl_path: &Option<std::path::PathBuf>) {
        if let Some(ref jp) = jsonl_path {
            self.preview_entries = claude::read_tail_entries(jp, 50);
        } else {
            self.preview_entries = Vec::new();
        }
    }

    fn start_kill(&mut self) {
        let sessions = self.registry.sorted_sessions();
        if let Some(session) = sessions.get(self.selected) {
            if session.state == SessionState::Stale {
                self.status_message = Some("session is already stale".into());
                return;
            }
            let name = session.name.clone();
            let id = session.id.clone();
            self.overlay = Overlay::ConfirmKill { name, id };
        }
    }

    fn execute_kill(&mut self, name: &str, id: &str) {
        let pid = match claude::read_session_pid(id) {
            Some(p) => p,
            None => {
                self.status_message = Some(format!("'{name}': pid not found or already dead"));
                return;
            }
        };
        match claude::kill_session(pid) {
            Ok(()) => self.status_message = Some(format!("sent SIGTERM to '{name}' (pid {pid})")),
            Err(e) => self.status_message = Some(e),
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
        if self.registry.name_taken(&new_name, &id) {
            self.status_message = Some(format!("name '{}' is already taken", new_name));
            return;
        }
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

        let target = SessionTarget {
            cwd: session.cwd.to_string_lossy().into_owned(),
            name: session.name.clone(),
            dir_name: session
                .cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            tty: session.tty.clone(),
        };

        if let Some(ref bridge) = self.bridge {
            match bridge.go_to_session(&target) {
                Ok(()) => self.visited_session = Some(target.name),
                Err(e) => self.status_message = Some(e.to_string()),
            }
        } else {
            self.status_message = Some("no terminal bridge detected (try Ghostty or tmux)".into());
        }
    }

    fn tick(&mut self) {
        self.refresh_sessions();
        self.registry.remove_stale(60);

        if matches!(self.overlay, Overlay::Preview) && !self.has_terminal_capture {
            let sessions = self.registry.sorted_sessions();
            if let Some(session) = sessions.get(self.selected) {
                if let Some(ref jp) = session.jsonl_path {
                    self.preview_entries = claude::read_tail_entries(jp, 50);
                }
            }
        }
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
                existing.pid = session.pid;

                if !existing.renamed {
                    if let Some(override_name) = self.config.session_name_for(&cwd_str) {
                        existing.name = override_name.clone();
                    } else {
                        existing.name = session.name.clone();
                    }
                }

                if detected_state == SessionState::Processing
                    || matches!(detected_state, SessionState::ToolRunning(_))
                {
                    existing.activity.record_activity();
                    existing.last_notified_state = None;
                }

                if let SessionState::ToolRunning(ref tool) = detected_state {
                    existing.current_tool = Some(tool.clone());
                }

                if let Some(ref jp) = session.jsonl_path {
                    let offset = existing.usage.last_file_offset;
                    let (delta, new_offset) = claude::parse_token_usage(jp, offset);
                    existing.usage.input_tokens += delta.input_tokens;
                    existing.usage.output_tokens += delta.output_tokens;
                    existing.usage.cache_read_tokens += delta.cache_read_tokens;
                    existing.usage.cache_creation_tokens += delta.cache_creation_tokens;
                    existing.usage.cost_usd += delta.cost_usd;
                    existing.usage.last_file_offset = new_offset;
                }
                if existing.jsonl_path.is_none() {
                    existing.jsonl_path = session.jsonl_path;
                }

                let transitioned = existing.propose_state(detected_state);
                if transitioned {
                    let current = existing.state.clone();
                    if existing.last_notified_state.as_ref() != Some(&current) {
                        self.notifier.maybe_notify(
                            &existing.name,
                            &current,
                            self.prefs.notifications_muted,
                        );
                        existing.last_notified_state = Some(current);
                    }
                }
            } else {
                let mut new_session = session;
                if let Some(override_name) = self.config.session_name_for(&cwd_str) {
                    new_session.name = override_name.clone();
                }
                if let Some(ref jp) = new_session.jsonl_path {
                    let (usage, offset) = claude::parse_token_usage(jp, 0);
                    new_session.usage = usage;
                    new_session.usage.last_file_offset = offset;
                }
                self.registry.upsert(new_session);
            }
        }

        self.registry.re_disambiguate_names();
        self.registry.shift_all_activity();

        let count = self.registry.len();
        if count > 0 && self.selected >= count {
            self.selected = count - 1;
        }
    }
}
