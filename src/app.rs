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
use crate::state::session::{Session, SessionId, SessionKind};
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

/// True once the far side of the pty has closed. When a tmux server or
/// terminal dies abruptly, macOS keeps the slave as our controlling tty
/// (`tcgetpgrp` still succeeds) but `poll` reports the hang-up.
fn is_hung_up<F: std::os::fd::AsFd>(fd: F) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
    poll(&mut fds, PollTimeout::ZERO).is_ok()
        && fds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLHUP))
}

fn stdin_tty_alive() -> bool {
    let stdin = std::io::stdin();
    is_controlling_tty(&stdin) && !is_hung_up(&stdin)
}

/// How long the main loop gets to notice a lost tty on its own before the
/// watchdog force-exits.
const WATCHDOG_GRACE: Duration = Duration::from_secs(2);

/// Exit the process when the tty goes away even if the main loop is stuck.
///
/// crossterm 0.28/0.29 `try_read` spins forever when `read(2)` on a hung-up
/// tty returns `Ok(0)`, so `event::poll` never returns and the loop's own
/// tty check is never reached — the orphaned process (reparented to
/// launchd) pins a core indefinitely. This thread doesn't depend on
/// crossterm returning. It acts only on tty loss: with the tty gone there's
/// no terminal state worth restoring, whereas exiting on a mere shutdown
/// signal could leave a live terminal in raw mode. The flock is released by
/// the kernel on exit.
fn spawn_tty_watchdog() {
    let spawned = std::thread::Builder::new()
        .name("nerve-tty-watchdog".into())
        .spawn(|| {
            let mut lost_since: Option<std::time::Instant> = None;
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if stdin_tty_alive() {
                    lost_since = None;
                    continue;
                }
                let since = *lost_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= WATCHDOG_GRACE {
                    log_info!("app: main loop unresponsive after losing tty; forcing exit");
                    std::process::exit(0);
                }
            }
        });
    if let Err(e) = spawned {
        crate::log_warn!("app: tty watchdog failed to start: {e}");
    }
}

/// Best-effort clipboard write. Tries each platform tool in turn
/// (Wayland, then X11 on Linux) and returns false if none worked.
fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    const TOOLS: &[&[&str]] = &[&["pbcopy"]];
    #[cfg(not(target_os = "macos"))]
    const TOOLS: &[&[&str]] = &[
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
        &["xsel", "--clipboard", "--input"],
    ];
    TOOLS.iter().any(|argv| pipe_to(argv, text))
}

fn pipe_to(argv: &[&str], text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && wrote
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
#[cfg(test)]
use crate::util::sanitize::Sanitised;

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
    ConfirmKill { name: String, id: SessionId, kind: SessionKind, root: std::path::PathBuf },
    ConfirmPreview,
    /// Full details of one session, read live each frame. Held by id so a
    /// re-sort can't swap in another session under the user.
    Details { id: SessionId },
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
    /// Outcomes of slow external actions (`claude stop`, SIGTERM via a
    /// fresh `ps`) run off the UI thread; drained into the status bar.
    action_tx: std::sync::mpsc::Sender<StatusMessage>,
    action_rx: std::sync::mpsc::Receiver<StatusMessage>,
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

        let (action_tx, action_rx) = std::sync::mpsc::channel();
        let claude_roots = crate::detect::claude_roots();
        let claude_installed = claude_roots.iter().any(|r| r.exists());

        let refresh_interval =
            Duration::from_millis(config.general.refresh_interval_ms);
        let process_scan_interval =
            Duration::from_millis(config.general.process_scan_interval_ms);
        let discovery = crate::workers::discovery::spawn(
            refresh_interval,
            process_scan_interval,
            claude_roots.iter().map(|r| r.join("sessions")).collect(),
            paths.statusline_dir(),
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
            action_tx,
            action_rx,
        }
    }

    /// Run a slow external action on its own thread so a wedged CLI can't
    /// freeze the UI. `pending` shows until the outcome arrives.
    fn run_action(&mut self, pending: String, action: impl FnOnce() -> StatusMessage + Send + 'static) {
        self.set_status_info(pending);
        let tx = self.action_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("nerve-action".into())
            .spawn(move || {
                let _ = tx.send(action());
            });
        if let Err(e) = spawned {
            self.set_status_error(format!("could not start action: {e}"));
        }
    }

    fn drain_action_results(&mut self) {
        while let Ok(msg) = self.action_rx.try_recv() {
            self.status_message = Some(msg);
        }
    }

    fn set_status_info(&mut self, text: impl Into<String>) {
        self.status_message = Some(StatusMessage::info(text));
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
        spawn_tty_watchdog();

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

        while !self.should_quit && !self.shutdown.requested() && stdin_tty_alive() {
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
                    Overlay::ConfirmKill { ref name, kind, .. } => {
                        confirm_kill::render(frame, &self.theme, name, *kind);
                    }
                    Overlay::ConfirmPreview => {
                        confirm_preview::render(frame, &self.theme);
                    }
                    Overlay::Details { id } => {
                        if let Some(session) = visible.iter().find(|s| s.id == *id) {
                            crate::tui::details::render(frame, &self.theme, session);
                        }
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

        if !stdin_tty_alive() {
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

        // Every per-kind decision for "go to session" lives here; bridges
        // only ever see terminal sessions.
        match session.kind {
            SessionKind::Terminal => {
                let target = SessionTarget::from(session);
                match self.bridge.go_to_session(&target) {
                    Ok(()) => self.visited_session = Some(target.name),
                    Err(e) => self.set_status_error(e.to_string()),
                }
            }
            SessionKind::Desktop => {
                if let Err(e) = crate::platform::focus_desktop_app() {
                    self.set_status_error(e.to_string());
                }
            }
            SessionKind::Background => {
                let (id, root) = (session.id.clone(), session.claude_root.clone());
                self.attach_background(id.as_str(), &root);
            }
        }
    }

    /// Background sessions have no tab to jump to: open `claude attach`
    /// in a split where the terminal allows it, else hand the user the
    /// command on the clipboard.
    fn attach_background(&mut self, session_id: &str, root: &std::path::Path) {
        // One shell-quoted string: tmux hands a single argument to the
        // shell, and the same text is what the user would paste.
        let cmd = crate::detect::claude::attach_command(session_id, root);
        let job = crate::detect::claude::job_id(session_id);
        match self.bridge.open_command(&[&cmd]) {
            Ok(true) => self.set_status_info(format!("attached to background job {job}")),
            Ok(false) => {
                if copy_to_clipboard(&cmd) {
                    self.set_status_info(format!("copied `{cmd}` — paste it in any terminal"));
                } else {
                    self.set_status_info(format!("run `{cmd}` in any terminal"));
                }
            }
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
impl App {
    /// App with no worker thread, update check, or terminal detection,
    /// seeded through the same `apply_discovery` path production uses.
    fn for_test(bridge: Bridge, snaps: Vec<crate::state::session::DiscoverySnapshot>) -> Self {
        let themes = Theme::catalog(None);
        let (action_tx, action_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            paths: Paths::for_test(tempfile::tempdir().unwrap().keep()),
            shutdown: ShutdownFlag::for_test(),
            config: Config::default(),
            registry: SessionRegistry::new(),
            filtered: FilteredView::new(),
            discovery: DiscoveryWorker::idle(),
            focused: true,
            claude_installed: true,
            theme: themes[0].clone(),
            themes,
            theme_index: 0,
            bridge,
            selected: 0,
            cols: 2,
            overlay: Overlay::None,
            search_query: None,
            status_message: None,
            should_quit: false,
            notifier: Notifier::new(Default::default(), None),
            prefs: Prefs::default(),
            visited_session: None,
            update_banner: None,
            last_stale_sweep: std::time::Instant::now(),
            discovery_warned: false,
            action_tx,
            action_rx,
        };
        app.apply_discovery(snaps);
        app.apply_filter();
        app
    }

    fn status_text(&self) -> &str {
        self.status_message.as_ref().map(|m| m.text.as_str()).unwrap_or("")
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;
    use crate::state::session::{DiscoverySnapshot, SessionState};
    use crossterm::event::{KeyCode, KeyModifiers};

    fn snap(id: &str, kind: SessionKind, state: SessionState) -> DiscoverySnapshot {
        let mut s = DiscoverySnapshot::for_test(id, state);
        s.kind = kind;
        s
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(code, KeyModifiers::NONE);
    }

    #[test]
    fn devnull_is_not_a_controlling_tty() {
        let dev_null = std::fs::File::open("/dev/null").expect("/dev/null open");
        assert!(!is_controlling_tty(&dev_null));
    }

    // Regression: SIGKILLing a tmux server orphaned nerve at 100% CPU.
    // The slave stayed nerve's controlling tty, so the watchdog's
    // `tcgetpgrp` check never fired; only `poll` sees the hang-up.
    #[test]
    fn closed_pty_master_reads_as_hung_up() {
        use nix::fcntl::OFlag;
        use nix::pty::{grantpt, posix_openpt, ptsname, unlockpt};
        let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let slave_path = unsafe { ptsname(&master) }.unwrap();
        let slave = std::fs::OpenOptions::new().read(true).write(true).open(slave_path).unwrap();
        assert!(!is_hung_up(&slave));
        drop(master);
        assert!(is_hung_up(&slave));
    }

    // Killing the desktop app's embedded claude from outside leaves the
    // app showing a dead session; nerve must refuse before any dialog.
    #[test]
    fn kill_refuses_desktop_session() {
        let mut app = App::for_test(
            Bridge::NoOp,
            vec![snap("d1", SessionKind::Desktop, SessionState::Processing)],
        );
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status_text().contains("desktop session"));
    }

    // The confirm dialog must carry the kind so `y` routes a background
    // job to `claude stop` (resumable) rather than SIGTERM.
    #[test]
    fn kill_dialog_carries_session_kind() {
        for kind in [SessionKind::Background, SessionKind::Terminal] {
            let mut app = App::for_test(Bridge::NoOp, vec![snap("s1", kind, SessionState::Processing)]);
            press(&mut app, KeyCode::Char('x'));
            match &app.overlay {
                Overlay::ConfirmKill { kind: k, .. } => assert_eq!(*k, kind),
                _ => panic!("no confirm dialog for {kind:?}"),
            }
            // Enter is cancel, never confirm.
            press(&mut app, KeyCode::Enter);
            assert!(matches!(app.overlay, Overlay::None));
        }
    }

    // P on a non-terminal session must not start a tab-switching screen
    // capture (or its confirm prompt): there is no pane, and the tmux/
    // Ghostty cwd fallback would capture an unrelated one.
    #[test]
    fn live_preview_never_captures_for_non_terminal_sessions() {
        for kind in [SessionKind::Background, SessionKind::Desktop] {
            let mut app = App::for_test(
                Bridge::Tmux(crate::platform::tmux::TmuxBridge),
                vec![snap("s1", kind, SessionState::Idle)],
            );
            press(&mut app, KeyCode::Char('P'));
            assert!(
                matches!(app.overlay, Overlay::Preview { source: PreviewSource::LogEntries(_), .. }),
                "{kind:?} opened something other than the log view"
            );
        }
    }

    // Claude's own status needs no debounce: the first observation
    // must commit and notify. Heuristic states still wait 3 ticks.
    #[test]
    fn authoritative_state_notifies_on_first_tick() {
        let mut app = App::for_test(
            Bridge::NoOp,
            vec![snap("s1", SessionKind::Terminal, SessionState::Processing)],
        );
        let mut waiting = snap("s1", SessionKind::Terminal, SessionState::WaitingForInput);
        waiting.state_authoritative = true;
        waiting.waiting_for = Some(Sanitised::new("permission prompt"));
        let pending = app.apply_discovery(vec![waiting]);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, SessionState::WaitingForInput);
        assert_eq!(pending[0].waiting_for.as_ref().map(Sanitised::as_str), Some("permission prompt"));

        let mut app = App::for_test(
            Bridge::NoOp,
            vec![snap("s1", SessionKind::Terminal, SessionState::Processing)],
        );
        let heuristic = snap("s1", SessionKind::Terminal, SessionState::WaitingForInput);
        assert!(app.apply_discovery(vec![heuristic]).is_empty());
    }

    // A job blocked for days shows as Dormant but still exists in the
    // daemon; `x` must still stop it. A dormant terminal session can't.
    #[test]
    fn dormant_background_job_can_still_be_stopped() {
        let mut app = App::for_test(Bridge::NoOp, vec![snap("b1", SessionKind::Background, SessionState::Dormant)]);
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(app.overlay, Overlay::ConfirmKill { .. }));

        let mut app = App::for_test(Bridge::NoOp, vec![snap("t1", SessionKind::Terminal, SessionState::Dormant)]);
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(app.overlay, Overlay::None));
    }

    // Regression: details was keyed by list index, so a re-sort swapped
    // in another session and `c` / Enter acted on it.
    #[test]
    fn details_follow_their_session_across_resorts() {
        let idle = |id| {
            let mut s = snap(id, SessionKind::Terminal, SessionState::Idle);
            s.state_authoritative = true;
            s
        };
        let mut app = App::for_test(Bridge::NoOp, vec![idle("s1"), idle("s2")]);
        let shown = app.nth_filtered(app.selected).unwrap().id.clone();
        press(&mut app, KeyCode::Char('i'));
        assert!(matches!(&app.overlay, Overlay::Details { id } if *id == shown));

        // The other session starts waiting and sorts above ours.
        let other = if shown.as_str() == "s1" { "s2" } else { "s1" };
        let mut waiting = idle(other);
        waiting.detected_state = SessionState::WaitingForInput;
        let mine = idle(shown.as_str());
        app.apply_discovery(vec![waiting, mine]);
        app.follow_details_session();
        assert_eq!(app.nth_filtered(app.selected).unwrap().id, shown);

        // Gone from the list: the overlay closes rather than swallowing keys.
        app.overlay = Overlay::Details { id: SessionId::new("nope".to_string()) };
        app.follow_details_session();
        assert!(matches!(app.overlay, Overlay::None));
    }
}
