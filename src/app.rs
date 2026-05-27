use std::time::Duration;

use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use crate::config::Config;
use crate::detect::jsonl;
use crate::log_info;
use crate::notify::Notifier;
use crate::paths::Paths;
use crate::platform::{Bridge, SessionTarget};
use crate::signals::ShutdownFlag;
use crate::state::filtered_view::FilteredView;
use crate::state::session::{Session, SessionId};
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
        Some(jp) => jsonl::read_tail_entries(jp, 50),
        None => Vec::new(),
    }
}

use crate::tui::status::StatusMessage;

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

mod discovery;
mod input;
mod overlays;

pub(crate) struct App {
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
    status_message: Option<StatusMessage>,
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

impl App {
    pub(crate) fn new(paths: Paths, shutdown: ShutdownFlag) -> Self {
        let config = Config::load(&paths);
        let themes_dir = paths.themes_dir();
        let themes = Theme::catalog(Some(&themes_dir));
        let theme_name = &config.appearance.theme;
        let theme_index = themes
            .iter()
            .position(|t| t.name.as_str() == theme_name.as_str())
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
        // Banner suppression uses `is_newer`'s dot-separated u32
        // compare (not strict semver): a yanked release that reverts
        // `pending_update` to a version older than the one the user
        // dismissed shouldn't re-open the banner.
        let update_banner = crate::updater::pending_update(&paths, env!("CARGO_PKG_VERSION"))
            .filter(|v| match prefs.dismissed_update_version.as_deref() {
                Some(dismissed) => crate::updater::is_newer(v, dismissed),
                None => true,
            });
        // Merge config + prefs load errors so neither gets clobbered if
        // both files are malformed. Joined with " | " — the status bar
        // is one line, so two short messages on one line beats losing one.
        let load_errors: Vec<&str> = [&config.load_error, &prefs.load_error]
            .into_iter()
            .filter_map(|o| o.as_deref())
            .collect();
        let status_message = if load_errors.is_empty() {
            None
        } else {
            Some(StatusMessage::error(load_errors.join(" | ")))
        };

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

    fn set_status_info(&mut self, text: impl Into<String>) {
        self.status_message = Some(StatusMessage::info(text));
    }

    fn set_status_success(&mut self, text: impl Into<String>) {
        self.status_message = Some(StatusMessage::success(text));
    }

    fn set_status_error(&mut self, text: impl Into<String>) {
        self.status_message = Some(StatusMessage::error(text));
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

    pub(crate) fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
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
                // Mirror cards::render_cards thresholds so h/l
                // navigation matches the rendered layout.
                self.cols = match area.width {
                    0..80 => 1,
                    80..160 => 2,
                    _ => 3,
                };

                let visible: Vec<&Session> = self.filtered.iter(&self.registry).collect();

                cards::render(
                    frame,
                    area,
                    cards::RenderContext {
                        registry: &self.registry,
                        sessions: &visible,
                        selected: self.selected,
                        theme: &self.theme,
                        status_message: self.status_message.as_ref(),
                        notifications_muted: self.prefs.notifications_muted,
                        update_banner: self.update_banner.as_deref(),
                        search_query: self.search_query.as_deref(),
                        claude_installed: self.claude_installed,
                    },
                );
                match &self.overlay {
                    Overlay::Help => help::render(frame, &self.theme),
                    Overlay::Rename(buf) => rename::render(frame, &self.theme, buf),
                    Overlay::Search => {
                        let query = self.search_query.as_deref().unwrap_or("");
                        let search_area = crate::tui::centered(frame.area(), 60, 4);
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
                        // Make the match semantics visible — users wondered
                        // whether the match was substring vs fuzzy and whether
                        // case mattered. Now they can see.
                        let lines = vec![
                            ratatui::text::Line::from(ratatui::text::Span::styled(
                                format!("/{display}"),
                                ratatui::style::Style::default().fg(self.theme.text),
                            )),
                            ratatui::text::Line::from(vec![
                                ratatui::text::Span::styled(
                                    " fuzzy · case-insensitive  ",
                                    ratatui::style::Style::default()
                                        .fg(self.theme.muted)
                                        .add_modifier(ratatui::style::Modifier::DIM),
                                ),
                                ratatui::text::Span::styled(
                                    "[Enter] keep  [Esc] clear",
                                    ratatui::style::Style::default().fg(self.theme.muted),
                                ),
                            ]),
                        ];
                        frame.render_widget(ratatui::widgets::Paragraph::new(lines), inner);
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
                            self.set_status_info(format!("returned from '{name}'"));
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

        let target = SessionTarget::from(session);

        match self.bridge.go_to_session(&target) {
            Ok(()) => self.visited_session = Some(target.name),
            Err(e) => self.set_status_error(e.to_string()),
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
