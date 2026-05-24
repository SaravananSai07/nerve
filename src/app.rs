use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::config::Config;
use crate::detect::claude::{self, LogEntry};
use crate::log_info;
use crate::notify::Notifier;
use crate::paths::Paths;
use crate::platform::{Bridge, SessionTarget};
use crate::signals::ShutdownFlag;
use crate::state::session::{Session, SessionId, SessionState};
use crate::state::prefs::Prefs;
use crate::state::registry::SessionRegistry;
use crate::tui::{cards, confirm_kill, confirm_preview, help, preview, rename};
use crate::tui::theme::Theme;

/// Lower bound on iteration time. Defends the loop against runaway poll-true
/// situations (closed stdin returning POLLHUP, etc.) without introducing
/// noticeable input lag for human typing (50 ms ≈ 20 Hz cap).
const MIN_LOOP_INTERVAL: Duration = Duration::from_millis(50);

/// True when the given fd is attached to a controlling terminal. When Ghostty
/// (or any host terminal) tears the pty down without delivering SIGHUP, this
/// is the canary that lets us exit cleanly instead of orphaning.
fn is_controlling_tty<F: std::os::fd::AsFd>(fd: F) -> bool {
    nix::unistd::tcgetpgrp(fd.as_fd()).is_ok()
}

fn stdin_is_controlling_tty() -> bool {
    is_controlling_tty(std::io::stdin())
}

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn devnull_is_not_a_controlling_tty() {
        let dev_null = std::fs::File::open("/dev/null").expect("/dev/null open");
        assert!(!is_controlling_tty(&dev_null));
    }
}

enum Overlay {
    None,
    Help,
    Search,
    Rename(String),
    Preview,
    ConfirmKill { name: String, id: SessionId },
    ConfirmPreview,
}

pub struct App {
    paths: Paths,
    shutdown: ShutdownFlag,
    config: Config,
    registry: SessionRegistry,
    theme: Theme,
    theme_index: usize,
    bridge: Bridge,
    selected: usize,
    cols: usize,
    overlay: Overlay,
    search_query: Option<String>,
    status_message: Option<String>,
    should_quit: bool,
    notifier: Notifier,
    prefs: Prefs,
    preview_scroll: usize,
    preview_entries: Vec<LogEntry>,
    preview_lines: Vec<String>,
    has_terminal_capture: bool,
    visited_session: Option<String>,
    update_banner: Option<String>,
}

impl App {
    pub fn new(paths: Paths, shutdown: ShutdownFlag) -> Self {
        let config = Config::load(&paths);
        let theme_name = &config.appearance.theme;
        let theme_index = crate::tui::theme::THEME_NAMES
            .iter()
            .position(|&n| n == theme_name)
            .unwrap_or(0);
        let theme = Theme::by_name(theme_name);
        let bridge = Bridge::auto_detect();
        let terminal_app = match &bridge {
            #[cfg(target_os = "macos")]
            Bridge::Ghostty(_) => Some("Ghostty".to_string()),
            _ => None,
        };
        let notifier = Notifier::new(config.notifications.clone(), terminal_app);
        let prefs = Prefs::load(&paths);

        crate::updater::maybe_check_in_background(&paths, config.updates.check_on_launch);
        let update_banner = crate::updater::pending_update(&paths, env!("CARGO_PKG_VERSION"));
        let status_message = config.load_error.clone();

        Self {
            paths,
            shutdown,
            config,
            registry: SessionRegistry::new(),
            theme,
            theme_index,
            bridge,
            selected: 0,
            cols: 2,
            overlay: Overlay::None,
            search_query: None,
            status_message,
            should_quit: false,
            notifier,
            prefs,
            preview_scroll: 0,
            preview_entries: Vec::new(),
            preview_lines: Vec::new(),
            has_terminal_capture: false,
            visited_session: None,
            update_banner,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        self.refresh_sessions();

        while !self.should_quit && !self.shutdown.requested() && stdin_is_controlling_tty() {
            let loop_start = std::time::Instant::now();
            // preview_scroll is mutated by the preview overlay; hoist to avoid a
            // self-aliasing borrow with the immutable reads elsewhere in the closure
            let mut preview_scroll = self.preview_scroll;

            terminal.draw(|frame| {
                let area = frame.area();
                self.cols = if area.width >= 80 { 2 } else { 1 };

                let all = self.registry.sorted_sessions();
                let visible: Vec<&Session> = match self.search_query.as_deref() {
                    Some(q) if !q.is_empty() => {
                        let q_lower = q.to_ascii_lowercase();
                        all.into_iter().filter(|s| s.matches_query(&q_lower)).collect()
                    }
                    _ => all,
                };

                cards::render(
                    frame,
                    area,
                    &self.registry,
                    &visible,
                    self.selected,
                    &self.theme,
                    self.status_message.as_deref(),
                    self.prefs.notifications_muted,
                    self.update_banner.as_deref(),
                    self.search_query.as_deref(),
                );
                match &self.overlay {
                    Overlay::Help => help::render(frame, &self.theme),
                    Overlay::Rename(buf) => rename::render(frame, &self.theme, buf),
                    Overlay::Search => {
                        let query = self.search_query.as_deref().unwrap_or("");
                        let search_area = crate::tui::centered(frame.area(), 60, 3);
                        frame.render_widget(ratatui::widgets::Clear, search_area);
                        let block = ratatui::widgets::Block::default()
                            .title(ratatui::text::Span::styled(
                                format!(" search  ({} matches) ", visible.len()),
                                ratatui::style::Style::default()
                                    .fg(self.theme.processing)
                                    .add_modifier(ratatui::style::Modifier::BOLD),
                            ))
                            .borders(ratatui::widgets::Borders::ALL)
                            .border_type(ratatui::widgets::BorderType::Rounded)
                            .border_style(ratatui::style::Style::default().fg(self.theme.processing));
                        let inner = block.inner(search_area);
                        frame.render_widget(block, search_area);
                        let display = if query.is_empty() {
                            " type to search (name, dir, path)... "
                        } else {
                            query
                        };
                        let para = ratatui::widgets::Paragraph::new(ratatui::text::Line::from(
                            ratatui::text::Span::styled(
                                format!("/{display}"),
                                ratatui::style::Style::default().fg(self.theme.text),
                            ),
                        ));
                        frame.render_widget(para, inner);
                    }
                    Overlay::Preview => {
                        if let Some(session) = visible.get(self.selected) {
                            preview::render(
                                frame,
                                &self.theme,
                                session,
                                &self.preview_entries,
                                &self.preview_lines,
                                self.has_terminal_capture,
                                &mut preview_scroll,
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

            self.preview_scroll = preview_scroll;

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

            // Floor the iteration time. Under normal operation `event::poll`
            // consumes the refresh interval naturally; this floor only matters
            // when the event source misbehaves (closed tty returning POLLHUP
            // immediately, etc.) and stops the loop from spinning at 100% CPU.
            let elapsed = loop_start.elapsed();
            if elapsed < MIN_LOOP_INTERVAL {
                std::thread::sleep(MIN_LOOP_INTERVAL - elapsed);
            }
        }

        if !stdin_is_controlling_tty() {
            log_info!("app: lost controlling tty; exiting cleanly");
        } else if self.shutdown.requested() {
            log_info!("app: shutdown signal received; exiting cleanly");
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
            Overlay::Search => {
                self.handle_search_key(code);
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
                            self.execute_kill(&name, id.as_str());
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

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let count = self.num_filtered();
                let next = self.selected + self.cols;
                if count > 0 && next < count {
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
                let count = self.num_filtered();
                let col = self.selected % self.cols;
                if col + 1 < self.cols && self.selected + 1 < count {
                    self.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('g') => {
                self.go_to_selected_tab();
            }
            KeyCode::Char('s') => {
                self.registry.cycle_sort();
                self.apply_filter();
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
                self.prefs.save(&self.paths);
                self.status_message = Some(if self.prefs.notifications_muted {
                    "notifications muted".into()
                } else {
                    "notifications unmuted".into()
                });
            }
            KeyCode::Char('/') => {
                self.search_query.get_or_insert_with(String::new);
                self.overlay = Overlay::Search;
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
            }
            KeyCode::Esc => {
                if self.search_query.is_some() {
                    self.search_query = None;
                    self.apply_filter();
                    self.status_message = None;
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as usize) - ('1' as usize);
                let count = self.num_filtered();
                if idx < count {
                    self.selected = idx;
                }
            }
            _ => {}
        }
    }

    fn open_preview(&mut self) {
        if !self.bridge.is_active() {
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
                self.prefs.save(&self.paths);
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
        let Some(session) = self.nth_filtered(self.selected) else {
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

        if let Some(text) = self.bridge.capture_screen(&target) {
            self.preview_lines = text.lines().map(|l| l.to_string()).collect();
            self.preview_entries = Vec::new();
            self.has_terminal_capture = true;
            self.preview_scroll = usize::MAX;
            self.overlay = Overlay::Preview;
            return;
        }

        self.load_log_entries(&jsonl_path);
        self.has_terminal_capture = false;
        self.preview_scroll = usize::MAX;
        self.overlay = Overlay::Preview;
    }

    fn open_log_preview(&mut self) {
        let session = self.nth_filtered(self.selected);
        if let Some(session) = session {
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
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        if session.state == SessionState::Stale {
            self.status_message = Some("session is already stale".into());
            return;
        }
        let name = session.name.clone();
        let id = session.id.clone();
        self.overlay = Overlay::ConfirmKill { name, id };
    }

    fn execute_kill(&mut self, name: &str, id: &str) {
        // `kill_by_session_id` resolves + validates + signals as one
        // operation so a recycled pid can't slip through (S4 / L19).
        match claude::kill_by_session_id(id) {
            Ok(pid) => {
                self.status_message = Some(format!("sent SIGTERM to '{name}' (pid {pid})"));
            }
            Err(e) => {
                self.status_message = Some(format!("'{name}': {e}"));
            }
        }
    }

    fn start_rename(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        self.overlay = Overlay::Rename(session.name.clone());
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

    fn handle_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => {
                self.overlay = Overlay::None;
                match self.search_query.as_deref() {
                    Some(q) if !q.is_empty() => {
                        let count = self.num_filtered();
                        self.status_message = Some(format!("search: /{q}  ({count})"));
                    }
                    _ => self.search_query = None,
                }
            }
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                self.search_query = None;
                self.apply_filter();
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.search_query.as_mut() {
                    buf.pop();
                }
                self.apply_filter();
            }
            KeyCode::Char(c) => {
                self.search_query.get_or_insert_with(String::new).push(c);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn commit_rename(&mut self, new_name: String) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        let id = session.id.clone();
        if self.registry.name_taken(&new_name, id.as_str()) {
            self.status_message = Some(format!("name '{}' is already taken", new_name));
            return;
        }
        if let Some(session) = self.registry.get_mut(id.as_str()) {
            session.name = new_name;
            session.renamed = true;
        }
        self.apply_filter();
    }

    fn cycle_theme(&mut self) {
        let names = crate::tui::theme::THEME_NAMES;
        self.theme_index = (self.theme_index + 1) % names.len();
        self.theme = Theme::by_name(names[self.theme_index]);
    }

    fn go_to_selected_tab(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
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

        match self.bridge.go_to_session(&target) {
            Ok(()) => self.visited_session = Some(target.name),
            Err(e) => self.status_message = Some(e.to_string()),
        }
    }

    /// Clamp `selected` to the current visible count. Call whenever the
    /// filter result may have shrunk: keystroke, sort, rename, refresh.
    fn apply_filter(&mut self) {
        let count = self.num_filtered();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    fn num_filtered(&self) -> usize {
        match self.search_query.as_deref() {
            Some(q) if !q.is_empty() => {
                let q_lower = q.to_ascii_lowercase();
                self.registry
                    .sorted_sessions()
                    .iter()
                    .filter(|s| s.matches_query(&q_lower))
                    .count()
            }
            _ => self.registry.len(),
        }
    }

    fn nth_filtered(&self, index: usize) -> Option<&Session> {
        let all = self.registry.sorted_sessions();
        match self.search_query.as_deref() {
            Some(q) if !q.is_empty() => {
                let q_lower = q.to_ascii_lowercase();
                all.into_iter()
                    .filter(|s| s.matches_query(&q_lower))
                    .nth(index)
            }
            _ => all.into_iter().nth(index),
        }
    }

    fn tick(&mut self) {
        self.refresh_sessions();
        self.registry.remove_stale(60);
        self.apply_filter();

        if matches!(self.overlay, Overlay::Preview) && !self.has_terminal_capture {
            let Some(session) = self.nth_filtered(self.selected) else {
                return;
            };
            if let Some(ref jp) = session.jsonl_path {
                self.preview_entries = claude::read_tail_entries(jp, 50);
            }
        }
    }

    fn refresh_sessions(&mut self) {
        let discovered = claude::discover_sessions();
        let active_ids: std::collections::HashSet<&str> =
            discovered.iter().map(|s| s.id.as_str()).collect();

        let stale_ids: Vec<SessionId> = self
            .registry
            .ids()
            .iter()
            .filter(|id| !active_ids.contains(id.as_str()))
            .cloned()
            .collect();
        for id in stale_ids {
            self.registry.mark_stale(id.as_str());
        }

        for session in discovered {
            let detected_state = session.state.clone();
            let id = session.id.clone();
            let cwd_str = session.cwd.to_string_lossy().into_owned();

            if let Some(existing) = self.registry.get_mut(id.as_str()) {
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
                        let target = SessionTarget {
                            cwd: existing.cwd.to_string_lossy().into_owned(),
                            name: existing.name.clone(),
                            dir_name: existing
                                .cwd
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            tty: existing.tty.clone(),
                        };
                        self.notifier.maybe_notify(
                            &existing.name,
                            &current,
                            &target,
                            &self.bridge,
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
