use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::config::Config;
use crate::detect::claude;
use crate::log_info;
use crate::notify::Notifier;
use crate::paths::Paths;
use crate::platform::{Bridge, SessionTarget};
use crate::signals::ShutdownFlag;
use crate::state::filtered_view::FilteredView;
use crate::state::session::{DiscoverySnapshot, Session, SessionId, SessionState};
use crate::state::prefs::Prefs;
use crate::state::registry::SessionRegistry;
use crate::tui::preview::PreviewSource;
use crate::tui::{cards, confirm_kill, confirm_preview, help, preview, rename};
use crate::tui::theme::Theme;
use crate::workers::discovery::DiscoveryWorker;

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

/// Free function so preview-overlay constructors don't have to
/// borrow `&mut self` just to read a transcript tail.
fn load_log_entries(
    jsonl_path: &Option<std::path::PathBuf>,
) -> Vec<crate::state::log_entry::LogEntry> {
    match jsonl_path {
        Some(jp) => claude::read_tail_entries(jp, 50),
        None => Vec::new(),
    }
}

enum Overlay {
    None,
    Help,
    Search,
    Rename(String),
    /// The variant owns its content so closing the overlay drops
    /// the captured buffer with it — nothing stale hangs around on
    /// `App` after the user dismisses preview.
    Preview {
        scroll: usize,
        source: PreviewSource,
    },
    ConfirmKill { name: String, id: SessionId },
    ConfirmPreview,
}

pub struct App {
    paths: Paths,
    shutdown: ShutdownFlag,
    config: Config,
    registry: SessionRegistry,
    filtered: FilteredView,
    /// Discovery runs on its own thread; the UI never blocks on
    /// `ps -eo` forks or slow JSONL reads. We drain the latest
    /// snapshot per tick via `try_recv`.
    discovery: DiscoveryWorker,
    /// True while the host terminal has focus. The render is the
    /// expensive part of a tick; when unfocused we skip it and let
    /// the discovery worker keep the registry warm for when we
    /// come back.
    focused: bool,
    claude_installed: bool,
    /// Built-ins followed by anything loaded from
    /// `~/.config/nerve/themes/*.toml`. Cycle order is deterministic.
    themes: Vec<Theme>,
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
    visited_session: Option<String>,
    update_banner: Option<String>,
    /// Eviction sweep cadence — `remove_stale` only does work once
    /// per minute (the grace window), so running it every tick at
    /// 200 sessions was clear waste.
    last_stale_sweep: std::time::Instant,
    /// Set once after surfacing the "discovery worker stopped" banner
    /// so we don't re-overwrite a fresh status message on every tick.
    discovery_warned: bool,
}

/// Notification queued by the discovery phase, dispatched after
/// the registry mutation has fully settled. Decoupling the two
/// stages means a slow `osascript` spawn can't interleave with a
/// mid-flight registry write.
struct PendingNotification {
    name: String,
    state: SessionState,
    target: SessionTarget,
}

impl App {
    pub fn new(paths: Paths, shutdown: ShutdownFlag) -> Self {
        let config = Config::load(&paths);
        let themes_dir = paths.themes_dir();
        let themes = Theme::catalog(Some(&themes_dir));
        let theme_name = &config.appearance.theme;
        let theme_index = themes
            .iter()
            .position(|t| t.name == *theme_name)
            .unwrap_or(0);
        let theme = themes[theme_index].clone();
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

        let claude_installed = paths.claude_root().exists();

        let refresh_interval =
            Duration::from_millis(config.general.refresh_interval_ms);
        let process_scan_interval =
            Duration::from_millis(config.general.process_scan_interval_ms);
        let discovery = crate::workers::discovery::spawn(
            refresh_interval,
            process_scan_interval,
            paths.sessions_dir(),
            shutdown.clone(),
        )
        .expect("discovery worker must start");

        Self {
            paths,
            shutdown,
            config,
            registry: SessionRegistry::new(),
            filtered: FilteredView::new(),
            discovery,
            focused: true,
            claude_installed,
            themes,
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
            visited_session: None,
            update_banner,
            last_stale_sweep: std::time::Instant::now(),
            discovery_warned: false,
        }
    }

    /// Floor each loop iteration to `MIN_LOOP_INTERVAL` so a runaway
    /// poll-true (closed stdin returning `POLLHUP`, etc.) can't burn
    /// CPU at MHz rates.
    fn floor_iteration(&self, loop_start: std::time::Instant) {
        let elapsed = loop_start.elapsed();
        if elapsed < MIN_LOOP_INTERVAL {
            std::thread::sleep(MIN_LOOP_INTERVAL - elapsed);
        }
    }

    /// Unfocused tick: drain pending events so focus/quit/signal
    /// flags stay fresh and pump discovery once, but skip the entire
    /// draw path. With the FS watcher driving the worker, idle CPU
    /// drops to near zero.
    fn tick_unfocused(&mut self) -> std::io::Result<()> {
        // Drain any queued events so a FocusGained or quit keystroke
        // arrives promptly when the user comes back. Block up to a
        // beat so we don't busy-poll while hidden.
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => self.handle_key(key.code, key.modifiers),
                Event::FocusGained => self.focused = true,
                Event::FocusLost => self.focused = false,
                _ => {}
            }
        }
        self.tick();
        Ok(())
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        // Seed the registry from the worker's first snapshot so the
        // initial paint isn't blank. Generous 500 ms cap — in practice
        // the worker produces its first snapshot in ~50-100 ms on a
        // warm laptop. Past this point all reads are non-blocking.
        if let Some(snap) = self
            .discovery
            .next_snapshot_blocking(Duration::from_millis(500))
        {
            let pending = self.apply_discovery(snap);
            self.dispatch_notifications(pending);
        }

        while !self.should_quit && !self.shutdown.requested() && stdin_is_controlling_tty() {
            let loop_start = std::time::Instant::now();
            // Hoist preview scroll so the draw closure can take `&mut`
            // without conflicting with the `&self.overlay` borrow it
            // also needs. The new value is written back after draw.
            let mut preview_scroll = match &self.overlay {
                Overlay::Preview { scroll, .. } => *scroll,
                _ => 0,
            };

            // Skip the draw when unfocused — the terminal is showing
            // whatever's underneath us, and the worker keeps the
            // registry warm for when we come back.
            if !self.focused {
                self.tick_unfocused()?;
                self.floor_iteration(loop_start);
                continue;
            }

            // Refresh the cached FilteredView before drawing. On
            // steady state (registry version + query unchanged)
            // this is one comparison; only mutations or query
            // keystrokes trigger a rebuild.
            self.filtered
                .refresh(&self.registry, self.search_query.as_deref());

            terminal.draw(|frame| {
                let area = frame.area();
                self.cols = if area.width >= 80 { 2 } else { 1 };

                let visible: Vec<&Session> = self.filtered.iter(&self.registry).collect();

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
                    self.claude_installed,
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
                    Overlay::Preview { source, .. } => {
                        if let Some(session) = visible.get(self.selected) {
                            preview::render(
                                frame,
                                &self.theme,
                                session,
                                source,
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

            // Write back the (possibly clamped) scroll position.
            if let Overlay::Preview { scroll, .. } = &mut self.overlay {
                *scroll = preview_scroll;
            }

            if event::poll(Duration::from_millis(self.config.general.refresh_interval_ms))? {
                match event::read()? {
                    Event::Key(key) => self.handle_key(key.code, key.modifiers),
                    Event::FocusGained => {
                        self.focused = true;
                        if let Some(name) = self.visited_session.take() {
                            self.status_message = Some(format!("returned from '{name}'"));
                        }
                        // If we started with NoOp (TERM_PROGRAM
                        // wasn't recognised at launch) but the user
                        // has since moved nerve into a supported
                        // terminal, pick it up on focus-gain rather
                        // than requiring a relaunch.
                        if !self.bridge.is_active() {
                            self.bridge = Bridge::auto_detect();
                        }
                    }
                    Event::FocusLost => {
                        self.focused = false;
                    }
                    Event::Resize(_, _) => {
                        // Coalesce burst Resize events that fire
                        // during a window-edge drag — drain every
                        // queued resize so we render once per drag
                        // tick, not once per pixel.
                        while event::poll(Duration::from_millis(0))? {
                            if !matches!(event::read()?, Event::Resize(_, _)) {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                self.tick();
            }

            self.floor_iteration(loop_start);
        }

        if !stdin_is_controlling_tty() {
            log_info!("app: lost controlling tty; exiting cleanly");
        } else if self.shutdown.requested() {
            log_info!("app: shutdown signal received; exiting cleanly");
        }

        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Ctrl+C always quits, regardless of overlay. This is the one
        // hard exit; `q` from inside an overlay closes the overlay.
        if let KeyCode::Char('c') = code {
            if modifiers.contains(KeyModifiers::CONTROL) {
                self.should_quit = true;
                return;
            }
        }
        match &self.overlay {
            Overlay::Help => {
                match code {
                    KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc => {
                        self.overlay = Overlay::None;
                    }
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
            Overlay::Preview { .. } => {
                match code {
                    KeyCode::Char('p')
                    | KeyCode::Char('P')
                    | KeyCode::Char('q')
                    | KeyCode::Esc => {
                        self.overlay = Overlay::None;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let Overlay::Preview { scroll, .. } = &mut self.overlay {
                            *scroll = scroll.saturating_add(1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let Overlay::Preview { scroll, .. } = &mut self.overlay {
                            *scroll = scroll.saturating_sub(1);
                        }
                    }
                    _ => {}
                }
                return;
            }
            Overlay::ConfirmKill { .. } => {
                match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if let Overlay::ConfirmKill { name, id } =
                            std::mem::replace(&mut self.overlay, Overlay::None)
                        {
                            self.execute_kill(&name, id.as_str());
                        }
                    }
                    // Enter / n / Esc / q all cancel — kill is
                    // destructive, so the "punch through with Enter"
                    // reflex resolves to the safe choice.
                    KeyCode::Char('n')
                    | KeyCode::Char('N')
                    | KeyCode::Char('q')
                    | KeyCode::Enter
                    | KeyCode::Esc => self.overlay = Overlay::None,
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
            // Enter is the only "switch to that session's tab" key.
            // Plain `g` previously aliased here, but vim users hitting
            // `g` (expecting `gg` = top, or to-of-list navigation)
            // would teleport into a session with no return path.
            KeyCode::Enter => {
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
            name: session.name().to_string(),
            dir_name: session
                .cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            tty: session.tty.clone(),
        };
        let jsonl_path = session.jsonl_path.clone();

        let source = match self.bridge.capture_screen(&target) {
            Some(text) => PreviewSource::TerminalCapture(
                // Symmetric with the LogEntries path: every line of capture
                // crossing into the host terminal gets sanitised. Bridge
                // text is usually pre-rendered, but tmux/ghostty don't
                // guarantee the absence of every C0/C1 byte.
                text.lines()
                    .map(|l| crate::util::sanitize::strip_ansi(l).into_owned())
                    .collect(),
            ),
            None => PreviewSource::LogEntries(load_log_entries(&jsonl_path)),
        };
        self.overlay = Overlay::Preview {
            scroll: usize::MAX,
            source,
        };
    }

    fn open_log_preview(&mut self) {
        let entries = match self.nth_filtered(self.selected) {
            Some(s) => load_log_entries(&s.jsonl_path),
            None => Vec::new(),
        };
        self.overlay = Overlay::Preview {
            scroll: usize::MAX,
            source: PreviewSource::LogEntries(entries),
        };
    }


    fn start_kill(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };
        if session.state().is_terminal() {
            self.status_message = Some("session is already gone or dormant".into());
            return;
        }
        let name = session.name().to_string();
        let id = session.id.clone();
        self.overlay = Overlay::ConfirmKill { name, id };
    }

    fn execute_kill(&mut self, name: &str, id: &str) {
        // Single function handles resolve + re-validate + signal so
        // a recycled pid can't slip through between the lookup and
        // the kill.
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
        self.overlay = Overlay::Rename(session.name().to_string());
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
            session.rename_to(new_name);
        }
        // Name change affects both filter (matches_query) and Name-sort
        // ordering, so the FilteredView cache must rebuild.
        self.registry.bump_version();
        self.apply_filter();
    }

    fn cycle_theme(&mut self) {
        if self.themes.is_empty() {
            return;
        }
        self.theme_index = (self.theme_index + 1) % self.themes.len();
        self.theme = self.themes[self.theme_index].clone();
    }

    fn go_to_selected_tab(&mut self) {
        let Some(session) = self.nth_filtered(self.selected) else {
            return;
        };

        let target = SessionTarget {
            cwd: session.cwd.to_string_lossy().into_owned(),
            name: session.name().to_string(),
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

    /// Refresh the cached `FilteredView` against the current registry +
    /// search query, then clamp `selected` to the new visible count.
    /// Call whenever the filter result may have shrunk: keystroke, sort,
    /// rename, refresh.
    fn apply_filter(&mut self) {
        self.filtered
            .refresh(&self.registry, self.search_query.as_deref());
        let count = self.filtered.len();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    fn num_filtered(&self) -> usize {
        self.filtered.len()
    }

    fn nth_filtered(&self, index: usize) -> Option<&Session> {
        self.filtered.get(&self.registry, index)
    }

    fn tick(&mut self) {
        self.refresh_sessions();
        // The eviction sweep only does anything once per minute (the
        // grace window), so polling it at the inner-loop cadence
        // (~50–250 ms) was clones-and-hashes for no result. Cap to
        // ~5 s and never miss a grace expiry by more than that.
        const STALE_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
        if self.last_stale_sweep.elapsed() >= STALE_SWEEP_INTERVAL {
            self.registry.remove_stale(60);
            self.last_stale_sweep = std::time::Instant::now();
        }
        // Surface a banner if the worker thread died on us. Without
        // this, a frozen worker is indistinguishable from a quiet one.
        // Don't clobber a fresh action message (e.g. a kill confirmation
        // the user just triggered) — the banner can wait for the slot
        // to come free on the next tick.
        if !self.discovery.is_alive()
            && !self.discovery_warned
            && self.status_message.is_none()
        {
            self.status_message =
                Some("discovery worker stopped — see ~/.config/nerve/nerve.log".into());
            self.discovery_warned = true;
        }
        self.apply_filter();

        // Keep the log-preview content fresh while the overlay is open
        // — but only when we're showing parsed entries (a captured
        // terminal buffer is a point-in-time snapshot and shouldn't be
        // silently replaced).
        let needs_refresh = matches!(
            &self.overlay,
            Overlay::Preview { source: PreviewSource::LogEntries(_), .. }
        );
        if needs_refresh {
            let jsonl_path = self.nth_filtered(self.selected).and_then(|s| s.jsonl_path.clone());
            let entries = load_log_entries(&jsonl_path);
            if let Overlay::Preview { source, .. } = &mut self.overlay {
                *source = PreviewSource::LogEntries(entries);
            }
        }
    }

    /// UI-thread tick. Does no I/O of its own; the discovery worker
    /// owns that. Three stages:
    ///   1. Drain the latest snapshot from the worker channel
    ///      (coalescing any backlog to the freshest one).
    ///   2. Apply it to the registry, collecting pending
    ///      notifications without dispatching them yet.
    ///   3. Dispatch the notification batch. Splitting (2) from (3)
    ///      means a slow `osascript` spawn can't interleave with a
    ///      mid-flight registry mutation.
    fn refresh_sessions(&mut self) {
        if let Some(discovered) = self.discovery.latest_snapshot() {
            let pending = self.apply_discovery(discovered);
            self.dispatch_notifications(pending);
        }
        // No snapshot this tick: keep the previous registry state. The
        // FilteredView cache is unaffected (no version bump), so the
        // paint is essentially free.

        let count = self.registry.len();
        if count > 0 && self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Stage 2 — pure registry mutation. Returns the batch of
    /// notifications that should fire once the mutation is complete.
    fn apply_discovery(
        &mut self,
        discovered: Vec<DiscoverySnapshot>,
    ) -> Vec<PendingNotification> {
        let active_ids: std::collections::HashSet<&str> =
            discovered.iter().map(|s| s.id.as_str()).collect();

        let stale_ids: Vec<SessionId> = self
            .registry
            .ids()
            .iter()
            .filter(|id| !active_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let any_marked_stale = !stale_ids.is_empty();
        for id in stale_ids {
            self.registry.mark_stale(id.as_str());
        }

        let mut pending = Vec::new();
        let mut any_transition = false;
        let mut any_membership_change = any_marked_stale;

        for mut snap in discovered {
            let detected_state = snap.detected_state.clone();
            let id = snap.id.clone();
            let cwd_str = snap.cwd.to_string_lossy().into_owned();
            let name_override = self
                .config
                .session_name_for(&cwd_str)
                .map(String::as_str);

            if let Some(existing) = self.registry.get_mut(id.as_str()) {
                existing.merge_snapshot(snap, name_override);
                existing.record_activity_if_busy(&detected_state);

                if existing.propose_state(detected_state) {
                    any_transition = true;
                    if let Some(state) = existing.take_pending_notification() {
                        pending.push(PendingNotification {
                            name: existing.name().to_string(),
                            state,
                            target: SessionTarget {
                                cwd: existing.cwd.to_string_lossy().into_owned(),
                                name: existing.name().to_string(),
                                dir_name: existing
                                    .cwd
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                tty: existing.tty.clone(),
                            },
                        });
                    }
                }
            } else {
                if let Some(over) = name_override {
                    snap.name = over.to_string();
                }
                // `from_snapshot` seeds the state machine with the
                // detected state directly — no Processing-flash while
                // the proposal counter climbs from the default.
                self.registry.upsert(Session::from_snapshot(snap));
                any_membership_change = true;
            }
        }

        // Disambiguation is only meaningful when membership
        // changed; an existing-session update can't introduce a new
        // name clash. Skipping the O(n²)-ish scan on idle ticks.
        if any_membership_change {
            self.registry.re_disambiguate_names();
        }
        // The activity ring buffer has two write paths by design —
        // `record_activity` (event-driven, called inline above) and
        // `shift_if_needed` (time-driven, here). Their inputs are
        // different and fusing them would either drop the event
        // signal or recompute elapsed-time shifts per session.
        self.registry.shift_all_activity();

        if any_transition {
            self.registry.bump_version();
        }

        pending
    }

    /// Stage 3 — fire each pending notification. Registry is fully
    /// settled before any of these run, so a slow `osascript` spawn
    /// can no longer freeze a mid-refresh mutation.
    fn dispatch_notifications(&self, batch: Vec<PendingNotification>) {
        for note in batch {
            self.notifier.maybe_notify(
                &note.name,
                &note.state,
                &note.target,
                &self.bridge,
                self.prefs.notifications_muted,
            );
        }
    }
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
