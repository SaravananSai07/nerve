use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use crate::config::{Config, Prefs};
use crate::detect::worker::DiscoveryWorker;
use crate::log_info;
use crate::notify::NotifyBook;
use crate::notify::{self, Alert, Click, Notifier};
use crate::paths::Paths;
use crate::platform::Bridge;
use crate::state::registry::SessionRegistry;
use crate::state::session::{DiscoverySnapshot, SessionState};
use crate::terminal::ShutdownFlag;
use crate::tui::dialog::Dialog;
use crate::tui::model::{self, Band, Row, RowKey, Said, Status};
use crate::tui::theme::Theme;
use crate::tui::Page;
use crate::util::sanitize::Sanitised;

mod actions;
mod fleet;
mod input;
mod view;

/// Lower bound on iteration time. Defends the loop against runaway poll-true
/// situations (closed stdin returning POLLHUP, etc.) without introducing
/// noticeable input lag for human typing (50 ms ≈ 20 Hz cap).
const MIN_LOOP_INTERVAL: Duration = Duration::from_millis(50);
/// A spinner frame, and how often the screen redraws while one shows.
const FRAME: Duration = Duration::from_millis(100);
/// How long a message stays on the bottom line.
const TOAST: Duration = Duration::from_millis(3500);
/// How long a row that arrived or was landed on stays lit, in two beats.
const FLASH: Duration = Duration::from_millis(2200);

/// Detect the host terminal, leaving nerve's own tab id where
/// notification clicks find it.
fn detect_bridge(paths: &Paths) -> Bridge {
    let file = paths.focus_file();
    Bridge::detect(move |id| {
        let _ = std::fs::write(file, id.to_string());
    })
}

/// A question on the bottom line, answered with `y`.
enum Confirm {
    Stop {
        key: RowKey,
        label: String,
    },
    /// Capturing a screen briefly switches tabs; `d` stops asking.
    Capture {
        key: RowKey,
    },
    /// Stop syncing a folder on this device.
    StopSync {
        name: String,
        path: String,
    },
    /// Drop a device from every device.
    Forget {
        device: String,
    },
}

/// What is on screen and where the cursor is: everything a fresh app
/// starts empty.
#[derive(Default)]
struct Ui {
    page: Page,
    /// The session under the cursor, by identity.
    cur: Option<RowKey>,
    /// Its panel is open.
    open: bool,
    /// The prompt being typed into the panel.
    compose: Option<String>,
    find: Option<String>,
    finding: bool,
    toast: Option<(String, Instant)>,
    confirm: Option<Confirm>,
    flash: Option<(RowKey, Instant)>,
    dialog: Option<Dialog>,
    settings_cursor: usize,
    settings_col: u8,
    /// The folder under the cursor on the Sync page.
    sync_cursor: usize,
    /// Lines the panel is scrolled up, and the session it is for.
    scroll: Cell<usize>,
    scroll_for: Option<RowKey>,
    screen: Option<(RowKey, Vec<String>)>,
    /// An answer on its way: shown under the question until it resolves.
    sending: Option<(RowKey, String, Instant)>,
    /// A notification click waiting for its session to show up.
    landing: Option<(RowKey, Instant)>,
    /// A session start sent to a device, until it answers: (request id, device).
    starting: Option<(String, String)>,
    /// The open session last told to the sync agent: (device, session).
    viewing: Option<(String, String)>,
    /// Each row's group last frame, to light up rows that start needing you.
    seen_bands: HashMap<RowKey, Band>,
    /// The screen's width at the last draw, to place mouse events.
    width: u16,
    /// The window title last set, so it is only written on change.
    title: String,
}

pub(crate) struct App {
    paths: Paths,
    shutdown: ShutdownFlag,
    config: Config,
    registry: SessionRegistry,
    /// Discovery runs on its own thread; the UI never blocks on `ps -eo`
    /// forks or slow JSONL reads.
    discovery: DiscoveryWorker,
    /// While the host terminal is in the background nerve skips drawing
    /// and the worker keeps the registry warm.
    focused: bool,
    claude_installed: bool,
    themes: Vec<Theme>,
    theme_index: usize,
    bridge: Bridge,
    prefs: Prefs,
    /// Shows this device's notifications when no sync agent does.
    notifier: Notifier,
    /// Who hears about what, when there is no sync agent to keep it.
    book: NotifyBook,
    /// This device's name.
    me: String,
    ui: Ui,
    /// Local transcripts read so far, so a session opens on the last
    /// one at once while discovery reads it again.
    transcripts: HashMap<PathBuf, Vec<Said>>,
    /// The transcript the open session wants.
    transcript_path: Option<PathBuf>,
    rows: Vec<Row>,
    /// When the app started: the spinner turns by the clock.
    epoch: Instant,
    last_tick: Instant,
    last_stale_sweep: Instant,
    /// Set once the "discovery stopped" message has shown.
    discovery_warned: bool,
    /// Outcomes of slow external actions (`claude stop`, SIGTERM via a
    /// fresh `ps`), run off the UI thread.
    action_tx: std::sync::mpsc::Sender<String>,
    action_rx: std::sync::mpsc::Receiver<String>,
    should_quit: bool,
    /// Cross-device sync agent, once this device is connected.
    sync: Option<crate::sync::agent::AgentHandle>,
    /// The agent's view as of this tick.
    agent: crate::sync::agent::SyncView,
    /// A worker connecting this device, when one runs.
    conn: Option<std::sync::mpsc::Receiver<fleet::ConnEvent>>,
}

impl App {
    pub(crate) fn new(paths: Paths, shutdown: ShutdownFlag) -> Self {
        let config = Config::load(&paths);
        let prefs = Prefs::load(&paths);
        let themes = Theme::catalog(Some(&paths.themes_dir()));
        let wanted = prefs.theme.clone().unwrap_or_else(|| config.appearance.theme.clone());
        let theme_index = themes.iter().position(|t| t.name.as_str() == wanted).unwrap_or(0);
        let bridge = detect_bridge(&paths);
        crate::updater::maybe_check_in_background(&paths, config.updates.check_on_launch);
        let update = crate::updater::pending_update(&paths, env!("CARGO_PKG_VERSION"));
        let first_message = [config.load_error.clone(), prefs.load_error.clone()]
            .into_iter()
            .flatten()
            .next()
            .or_else(|| update.map(|v| format!("nerve {v} is out. Run nerve update to get it.")));

        let sync = crate::sync::start_for_tui(&paths, config.notifications.sound);
        let me = sync.as_ref().map(|h| h.snapshot().me).unwrap_or_else(crate::util::hostname);

        let (action_tx, action_rx) = std::sync::mpsc::channel();
        let claude_roots = crate::detect::claude_roots();
        let claude_installed = claude_roots.iter().any(|r| r.exists());
        let discovery = crate::detect::worker::spawn(
            Duration::from_millis(config.general.refresh_interval_ms),
            Duration::from_millis(config.general.process_scan_interval_ms),
            claude_roots.iter().map(|r| r.join("sessions")).collect(),
            paths.statusline_dir(),
            paths.land_file(),
            shutdown.clone(),
        )
        .expect("discovery worker must start");

        Self {
            notifier: Notifier::new(config.notifications.sound),
            book: NotifyBook::load(paths.config_dir()),
            ui: Ui { toast: first_message.map(|m| (m, Instant::now() + Duration::from_secs(5))), ..Ui::default() },
            paths,
            shutdown,
            config,
            registry: SessionRegistry::new(),
            discovery,
            focused: true,
            claude_installed,
            themes,
            theme_index,
            bridge,
            prefs,
            me,
            transcripts: HashMap::new(),
            transcript_path: None,
            rows: Vec::new(),
            epoch: Instant::now(),
            last_tick: Instant::now(),
            last_stale_sweep: Instant::now(),
            discovery_warned: false,
            action_tx,
            action_rx,
            should_quit: false,
            sync,
            agent: Default::default(),
            conn: None,
        }
    }

    fn theme(&self) -> &Theme {
        &self.themes[self.theme_index]
    }

    /// A message on the bottom line for a few seconds.
    fn say(&mut self, text: impl Into<String>) {
        self.ui.toast = Some((text.into(), Instant::now() + TOAST));
    }

    /// Run a slow external action on its own thread so a wedged CLI can't
    /// freeze the UI; its outcome comes back as a message.
    fn run_action(&mut self, pending: String, action: impl FnOnce() -> String + Send + 'static) {
        self.say(pending);
        let tx = self.action_tx.clone();
        let spawned = std::thread::Builder::new().name("nerve-action".into()).spawn(move || {
            let _ = tx.send(action());
        });
        if let Err(e) = spawned {
            self.say(format!("Could not start that: {e}"));
        }
    }

    /// Floor each loop iteration to `MIN_LOOP_INTERVAL` so a runaway
    /// poll-true (closed stdin returning `POLLHUP`, etc.) can't burn
    /// CPU at MHz rates.
    fn floor_iteration(&self, loop_start: Instant) {
        let elapsed = loop_start.elapsed();
        if elapsed < MIN_LOOP_INTERVAL {
            std::thread::sleep(MIN_LOOP_INTERVAL - elapsed);
        }
    }

    pub(crate) fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        crate::terminal::spawn_tty_watchdog();
        // Seed from the worker's first snapshot so the first frame isn't
        // blank; it usually arrives within 50–100 ms.
        if let Some(scan) = self.discovery.first_sessions(Duration::from_millis(500)) {
            self.take_scan(scan);
        }
        self.tick();

        while !self.should_quit && !self.shutdown.requested() && crate::terminal::tty_alive() {
            let loop_start = Instant::now();
            if self.focused {
                self.draw(terminal)?;
            }
            let refresh = Duration::from_millis(self.config.general.refresh_interval_ms);
            let wait = match (self.focused, self.animating()) {
                (false, _) => Duration::from_millis(250),
                (true, true) => FRAME.min(refresh),
                (true, false) => refresh,
            };
            if event::poll(wait)? {
                match event::read()? {
                    ev @ (Event::Key(_) | Event::Mouse(_)) => {
                        self.handle_event(ev);
                        // A paste arrives as many keys at once, a wheel
                        // turn as many scrolls: take them all before
                        // drawing again.
                        while event::poll(Duration::ZERO)? {
                            self.handle_event(event::read()?);
                        }
                        self.rebuild();
                    }
                    Event::FocusGained => {
                        self.focused = true;
                        // Started outside a supported terminal and moved
                        // into one since: pick it up now.
                        if !self.bridge.is_active() {
                            self.bridge = detect_bridge(&self.paths);
                        }
                    }
                    Event::FocusLost => self.focused = false,
                    Event::Resize(_, _) => {
                        // Coalesce a window-edge drag into one redraw.
                        while event::poll(Duration::from_millis(0))? {
                            if !matches!(event::read()?, Event::Resize(_, _)) {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Discovery is drained every pass, so a transcript shows the
            // moment it is read; the rest waits for the refresh tick.
            if self.last_tick.elapsed() >= refresh {
                self.tick();
            } else if self.drain() {
                self.rebuild();
            }
            self.share_viewing();
            self.floor_iteration(loop_start);
        }

        // Straight to the spool: a viewing TUI's agent dies with it before
        // it could pass the request on.
        if self.sync.is_some() && self.ui.viewing.is_some() {
            let (dir, id) = (crate::sync::fleet::sync_dir(&self.paths), crate::sync::protocol::new_id());
            let _ = crate::sync::ipc::put(&dir, &id, &crate::sync::agent::Request::Viewing(None));
        }
        let _ = std::fs::remove_file(self.paths.focus_file());
        if !crate::terminal::tty_alive() {
            log_info!("app: lost controlling tty; exiting cleanly");
        } else if self.shutdown.requested() {
            log_info!("app: shutdown signal received; exiting cleanly");
        }
        Ok(())
    }

    /// Something on screen moves: a working session's spinner, or the
    /// panel waiting for its transcript.
    fn animating(&self) -> bool {
        self.ui.page == Page::Sessions
            && (self.rows.iter().any(|r| r.status == Status::Working)
                || self.transcript_path.as_ref().is_some_and(|p| !self.transcripts.contains_key(p)))
    }

    /// Background work for one quiet tick: discovery, the agent's view,
    /// outcomes of actions, expired messages.
    fn tick(&mut self) {
        self.last_tick = Instant::now();
        self.drain();
        if self.last_stale_sweep.elapsed() >= Duration::from_secs(5) {
            self.registry.remove_stale(60);
            self.last_stale_sweep = Instant::now();
            // Keep only the transcripts of sessions still here.
            let live: Vec<&PathBuf> = self.registry.sessions().filter_map(|s| s.jsonl_path.as_ref()).collect();
            self.transcripts.retain(|p, _| live.contains(&p));
        }
        if !self.discovery.is_alive() && !self.discovery_warned {
            self.say(format!("Session discovery stopped. See {}", self.paths.log_file().display()));
            self.discovery_warned = true;
        }
        if let Some(h) = &self.sync {
            self.agent = h.snapshot();
        }
        self.follow_connect();
        self.follow_start();
        let now = Instant::now();
        if self.ui.toast.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.ui.toast = None;
        }
        self.rebuild();
    }

    /// Take what discovery and finished actions sent; true if anything
    /// arrived. Channel reads only, so it runs every pass.
    fn drain(&mut self) -> bool {
        let drained = self.discovery.drain();
        let mut any = drained.sessions.is_some() || drained.transcript.is_some() || drained.land.is_some();
        if let Some(scan) = drained.sessions {
            self.take_scan(scan);
        }
        if let Some((path, entries)) = drained.transcript {
            self.transcripts.insert(path, model::conversation(&entries));
        }
        if let Some(land) = drained.land {
            self.land(&land);
        }
        while let Ok(msg) = self.action_rx.try_recv() {
            self.say(msg);
            any = true;
        }
        any
    }

    /// Apply a discovery scan and show this device's notifications,
    /// unless a running sync agent does: it knows every device and must
    /// not double them. One still connecting, or retrying an unreachable
    /// relay, notifies nobody, so this device's own still show here.
    fn take_scan(&mut self, scan: Vec<DiscoverySnapshot>) {
        let pending = self.registry.apply(scan, &self.config.session_names);
        if self.sync.is_some() && self.agent.running {
            return;
        }
        let book = self.notify_book();
        let prefs = &book.prefs;
        for note in pending {
            let why = note.waiting_for.as_ref().map(Sanitised::as_str);
            let (alert, detail) = match note.state {
                SessionState::WaitingForInput if prefs.attention_on(&self.me) => {
                    (Alert::waiting(why), why.unwrap_or("Needs input").to_string())
                }
                SessionState::Error if prefs.attention_on(&self.me) => {
                    (Alert::Failed, why.unwrap_or("Stopped with an error").to_string())
                }
                SessionState::Idle
                    if notify::tell_finished(
                        book.finished_to(&note.id),
                        &self.me,
                        self.open_here().is_some_and(|k| k.device.is_none() && k.id == note.id),
                    ) =>
                {
                    (Alert::Finished, note.name.to_string())
                }
                _ => continue,
            };
            let click = Click { device: self.me.clone(), session: note.id, focus: self.bridge.own_tab() };
            self.notifier.send(&alert.title(&self.me), &format!("{}: {detail}", note.project), &click);
        }
    }

    /// The session open in nerve while its terminal has focus. Its finished
    /// turns don't notify this device: you are reading it.
    fn open_here(&self) -> Option<&RowKey> {
        self.ui.cur.as_ref().filter(|_| self.focused && self.ui.open && self.ui.page == Page::Sessions)
    }

    /// Tell the sync agent what `open_here` became, on change only. It may
    /// be another process (`nerve sync agent`), so the agent notifies.
    fn share_viewing(&mut self) {
        let now = self.open_here().map(|k| (k.device.clone().unwrap_or_else(|| self.me.clone()), k.id.clone()));
        if now == self.ui.viewing {
            return;
        }
        if let Some(h) = &self.sync {
            h.request(crate::sync::agent::Request::Viewing(now.clone()));
        }
        self.ui.viewing = now;
    }

    /// Rebuild the rows from discovery and the agent's view, keep the
    /// cursor on its session, and follow the open session's transcript.
    fn rebuild(&mut self) {
        let rows: Vec<Row> = {
            let facts = self.local_facts();
            self.registry.sessions().map(|s| model::local_row(s, facts.as_ref())).collect()
        };
        let rows = [rows, self.peer_rows(), self.approval_rows()].concat();
        // Light up a row the moment it starts needing you.
        let first = self.ui.seen_bands.is_empty();
        for r in &rows {
            let band = r.status.band();
            let before = self.ui.seen_bands.insert(r.key.clone(), band);
            if !first && band == Band::Need && before.is_some_and(|b| b != Band::Need) {
                self.ui.flash = Some((r.key.clone(), Instant::now()));
            }
        }
        self.ui.seen_bands.retain(|k, _| rows.iter().any(|r| r.key == *k));
        // An answer resolves once the question is gone.
        if let Some((key, _, at)) = &self.ui.sending {
            let still_asking = rows.iter().any(|r| r.key == *key && r.ask.is_some());
            if (!still_asking && at.elapsed() > Duration::from_secs(2)) || at.elapsed() > Duration::from_secs(20) {
                self.ui.sending = None;
            }
        }
        self.rows = rows;
        if let Some((key, at)) = self.ui.landing.take() {
            if let Some(r) = self.rows.iter().find(|r| r.key == key) {
                self.ui.open = r.status != Status::Request;
                self.ui.cur = Some(key.clone());
                self.ui.flash = Some((key, Instant::now()));
            } else if at.elapsed() < Duration::from_secs(10) {
                self.ui.landing = Some((key, at));
            }
        }
        let order = self.order();
        if !self.ui.cur.as_ref().is_some_and(|k| order.contains(k)) {
            self.ui.cur = order.first().cloned();
            self.ui.compose = None;
        }
        let want = match (&self.ui.cur, self.ui.open && self.ui.page == Page::Sessions) {
            (Some(k), true) if k.device.is_none() => self.registry.get(&k.id).and_then(|s| s.jsonl_path.clone()),
            _ => None,
        };
        if want != self.transcript_path {
            self.discovery.want_transcript(want.clone());
            self.transcript_path = want;
        }
        // Another session starts at its newest lines.
        if self.ui.scroll_for != self.ui.cur {
            self.ui.scroll.set(0);
            self.ui.scroll_for = self.ui.cur.clone();
        }
    }

    /// Row keys in screen order, after the find filter.
    fn order(&self) -> Vec<RowKey> {
        model::arrange(&self.rows, self.ui.find.as_deref())
            .into_iter()
            .flat_map(|(_, rows)| rows.into_iter().map(|r| r.key.clone()))
            .collect()
    }

    fn row(&self, key: &RowKey) -> Option<&Row> {
        self.rows.iter().find(|r| r.key == *key)
    }

    fn cur_row(&self) -> Option<&Row> {
        self.ui.cur.as_ref().and_then(|k| self.row(k))
    }

    /// A notification click: `<device>/<session>`.
    fn land(&mut self, land: &str) {
        let Some((device, id)) = land.split_once('/') else { return };
        let key = RowKey { device: (device != self.me).then(|| device.to_string()), id: id.to_string() };
        self.ui.page = Page::Sessions;
        self.ui.dialog = None;
        self.ui.confirm = None;
        self.ui.find = None;
        self.ui.finding = false;
        self.ui.landing = Some((key, Instant::now()));
    }
}

#[cfg(test)]
impl App {
    /// App with no worker thread, update check, or terminal detection,
    /// seeded through the registry as production is, without notifying.
    fn for_test(bridge: Bridge, snaps: Vec<DiscoverySnapshot>) -> Self {
        let dir = tempfile::tempdir().unwrap().keep();
        let (action_tx, action_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            paths: Paths::at(dir),
            shutdown: ShutdownFlag::for_test(),
            config: Config::default(),
            registry: SessionRegistry::new(),
            discovery: DiscoveryWorker::idle(),
            ui: Ui::default(),
            focused: true,
            claude_installed: true,
            themes: Theme::catalog(None),
            theme_index: 0,
            bridge,
            prefs: Prefs::default(),
            notifier: Notifier::new(false),
            book: NotifyBook::default(),
            me: "laptop".into(),
            transcripts: HashMap::new(),
            transcript_path: None,
            rows: Vec::new(),
            epoch: Instant::now(),
            last_tick: Instant::now(),
            last_stale_sweep: Instant::now(),
            discovery_warned: false,
            action_tx,
            action_rx,
            should_quit: false,
            sync: None,
            agent: Default::default(),
            conn: None,
        };
        app.scan(snaps);
        app
    }

    /// Apply a scan without notifying, then rebuild.
    fn scan(&mut self, snaps: Vec<DiscoverySnapshot>) {
        self.registry.apply(snaps, &self.config.session_names);
        self.rebuild();
    }

    fn toast_text(&self) -> &str {
        self.ui.toast.as_ref().map(|(t, _)| t.as_str()).unwrap_or("")
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;
    use crate::state::session::SessionKind;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn snap(id: &str, kind: SessionKind, state: SessionState) -> DiscoverySnapshot {
        let mut s = DiscoverySnapshot::for_test(id, state);
        s.kind = kind;
        s.state_authoritative = true;
        s
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(code, KeyModifiers::NONE);
        app.rebuild();
    }

    // Killing the desktop app's embedded claude from outside leaves the
    // app showing a dead session; nerve must refuse before asking.
    #[test]
    fn stop_refuses_a_desktop_session() {
        let mut app = App::for_test(Bridge::NoOp, vec![snap("d1", SessionKind::Desktop, SessionState::Processing)]);
        press(&mut app, KeyCode::Char('x'));
        assert!(app.ui.confirm.is_none());
        assert!(app.toast_text().contains("Claude app"));
    }

    // `y` stops; Enter is never the confirming key.
    #[test]
    fn stop_asks_first_and_enter_cancels() {
        for kind in [SessionKind::Background, SessionKind::Terminal] {
            let mut app = App::for_test(Bridge::NoOp, vec![snap("s1", kind, SessionState::Processing)]);
            press(&mut app, KeyCode::Char('x'));
            assert!(matches!(app.ui.confirm, Some(Confirm::Stop { .. })), "no question for {kind:?}");
            press(&mut app, KeyCode::Enter);
            assert!(app.ui.confirm.is_none());
        }
    }

    // A session left for days shows as Dormant but is still alive: a
    // parked job in the daemon, or a claude at a prompt. `x` stops both.
    #[test]
    fn a_dormant_session_can_still_be_stopped() {
        for kind in [SessionKind::Background, SessionKind::Terminal] {
            let mut app = App::for_test(Bridge::NoOp, vec![snap("s1", kind, SessionState::Dormant)]);
            press(&mut app, KeyCode::Char('x'));
            assert!(app.ui.confirm.is_some(), "{kind:?}");
        }
    }

    // The cursor holds a session, not a line: when another session starts
    // waiting and moves above it, the cursor stays on its own.
    #[test]
    fn cursor_follows_its_session_across_reorders() {
        let mut app = App::for_test(
            Bridge::NoOp,
            vec![
                snap("s1", SessionKind::Terminal, SessionState::Idle),
                snap("s2", SessionKind::Terminal, SessionState::Idle),
            ],
        );
        press(&mut app, KeyCode::Char('j'));
        let held = app.ui.cur.clone().unwrap();
        press(&mut app, KeyCode::Enter);
        assert!(app.ui.open);
        let other = if held.id == "s1" { "s2" } else { "s1" };
        app.scan(vec![
            snap(other, SessionKind::Terminal, SessionState::WaitingForInput),
            snap(&held.id, SessionKind::Terminal, SessionState::Idle),
        ]);
        assert_eq!(app.ui.cur.as_ref(), Some(&held));
        assert_eq!(app.order()[0].id, other, "the waiting session sorts first");
        assert!(app.ui.flash.as_ref().is_some_and(|(k, _)| k.id == other), "and lights up as it arrives");
    }

    #[test]
    fn a_click_lands_on_its_session_with_the_panel_open() {
        let mut app = App::for_test(
            Bridge::NoOp,
            vec![
                snap("s1", SessionKind::Terminal, SessionState::Idle),
                snap("s2", SessionKind::Terminal, SessionState::Idle),
            ],
        );
        app.ui.page = Page::Settings;
        app.land("laptop/s2");
        app.rebuild();
        assert_eq!(
            (app.ui.page, app.ui.open, app.ui.cur.as_ref().map(|k| k.id.as_str())),
            (Page::Sessions, true, Some("s2"))
        );
    }

    #[test]
    fn the_wheel_scrolls_the_transcript_under_it_and_moves_the_list_beside_it() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let snaps = (0..30).map(|i| snap(&format!("s{i:02}"), SessionKind::Terminal, SessionState::Idle)).collect();
        let mut app = App::for_test(Bridge::NoOp, snaps);
        app.ui.width = 140;
        let wheel = |app: &mut App, kind, column| {
            app.handle_event(Event::Mouse(MouseEvent { kind, column, row: 10, modifiers: KeyModifiers::NONE }));
            app.rebuild();
        };
        press(&mut app, KeyCode::Enter);
        let first = app.ui.cur.clone();
        wheel(&mut app, MouseEventKind::ScrollUp, 90);
        wheel(&mut app, MouseEventKind::ScrollUp, 90);
        assert_eq!(
            (app.ui.scroll.get(), &app.ui.cur),
            (6, &first),
            "over the panel: the transcript scrolls, the cursor stays"
        );
        wheel(&mut app, MouseEventKind::ScrollDown, 10);
        assert_ne!(app.ui.cur, first, "over the list: the cursor moves");
        assert_eq!(app.ui.scroll.get(), 0, "another session starts at its newest lines");
        // Every session can be reached and left, both ways.
        for _ in 0..40 {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(app.ui.cur.as_ref().map(|k| k.id.as_str()), app.order().last().map(|k| k.id.as_str()));
        for _ in 0..40 {
            press(&mut app, KeyCode::Up);
        }
        assert_eq!(app.ui.cur, app.order().first().cloned());
    }

    #[test]
    fn a_session_opens_on_its_cached_transcript() {
        let mut app = App::for_test(Bridge::NoOp, vec![snap("s1", SessionKind::Terminal, SessionState::Idle)]);
        let path = PathBuf::from("/t/s1.jsonl");
        app.transcript_path = Some(path.clone());
        assert!(app.animating(), "the panel waits for its transcript");
        app.transcripts.insert(path, vec![Said::You("hi".into())]);
        assert!(!app.animating());
    }

    #[test]
    fn find_filters_and_esc_clears() {
        let mut app = App::for_test(Bridge::NoOp, vec![snap("s1", SessionKind::Terminal, SessionState::Idle)]);
        press(&mut app, KeyCode::Char('/'));
        for c in "zz".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.order().is_empty());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.order().len(), 1);
    }

    fn keys(app: &App) -> Vec<&'static str> {
        match app.footer(&app.body()) {
            crate::tui::Footer::Hints(h) => h.into_iter().map(|(k, _)| k).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn add_folder_steps_forward_and_esc_goes_back() {
        use crate::tui::dialog::{Dialog, FolderStep};
        let mut app = App::for_test(Bridge::NoOp, Vec::new());
        app.ui.dialog = Some(Dialog::AddFolder {
            step: FolderStep::Folder,
            cands: vec![(PathBuf::from("/w/site"), "~/w/site".into(), "site".into())],
            pick: 0,
            devices: vec![("laptop".into(), "This device"), ("studio".into(), "")],
            set: std::collections::BTreeSet::from(["laptop".to_string()]),
            when: 0,
            i: 0,
        });
        let at = |app: &App| match &app.ui.dialog {
            Some(Dialog::AddFolder { step, i, .. }) => Some((*step, *i)),
            _ => None,
        };
        press(&mut app, KeyCode::Enter);
        assert_eq!(at(&app), Some((FolderStep::Devices, 0)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(at(&app), Some((FolderStep::Devices, 0)), "needs a second device");
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        assert_eq!(at(&app), Some((FolderStep::When, 0)));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(at(&app), Some((FolderStep::Rules, 0)));
        press(&mut app, KeyCode::Esc);
        assert_eq!(at(&app), Some((FolderStep::When, 1)), "back to when, on the choice made");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert_eq!(at(&app), Some((FolderStep::Folder, 0)));
        press(&mut app, KeyCode::Esc);
        assert!(app.ui.dialog.is_none());
    }

    #[test]
    fn who_modes_pick_all_now_or_chosen_devices() {
        use crate::tui::dialog::{Dialog, WhoMode};
        let mut app = App::for_test(Bridge::NoOp, Vec::new());
        app.ui.dialog = Some(Dialog::Who {
            link: Some("phone".into()),
            devices: vec!["studio".into()],
            set: std::collections::BTreeSet::new(),
            mode: WhoMode::AllLater,
            i: 0,
        });
        let at = |app: &App| match &app.ui.dialog {
            Some(Dialog::Who { mode, i, set, .. }) => Some((*mode, *i, set.len())),
            _ => None,
        };
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(at(&app), Some((WhoMode::AllNow, 1, 0)));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(at(&app), Some((WhoMode::Picked, 2, 1)), "a device picks the third mode");
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(at(&app), Some((WhoMode::Picked, 2, 0)), "and toggles off");
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(at(&app), Some((WhoMode::AllLater, 0, 0)));
    }

    #[test]
    fn review_types_a_folder_but_jk_move_elsewhere() {
        use crate::tui::dialog::{Dialog, Landing};
        let mut app = App::for_test(Bridge::NoOp, Vec::new());
        app.ui.dialog = Some(Dialog::Review {
            key: RowKey { device: None, id: "a1".into() },
            title: "t".into(),
            detail: String::new(),
            words: None,
            folder: Some(Landing { lands: "~/code/app".into(), other: String::new() }),
            step: crate::tui::dialog::ReviewStep::Where,
            i: 0,
        });
        let at = |app: &App| match &app.ui.dialog {
            Some(Dialog::Review { i, folder: Some(f), .. }) => Some((*i, f.other.clone())),
            _ => None,
        };
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(at(&app), Some((1, String::new())), "j moves off the first choice");
        for c in "jkn".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(at(&app), Some((1, "jkn".into())), "on the typed choice they are letters");
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Up);
        assert_eq!(at(&app), Some((0, "jk".into())));
    }

    #[test]
    fn the_footer_follows_the_page_and_state() {
        let mut app = App::for_test(Bridge::NoOp, Vec::new());
        assert_eq!(keys(&app), ["/", "Tab", "?", "q"]);
        app.claude_installed = false;
        assert_eq!(keys(&app), ["Tab", "?", "q"]);
        app.scan(vec![snap("s1", SessionKind::Terminal, SessionState::Idle)]);
        assert_eq!(keys(&app), ["↵", "n", "t", "/", "Tab", "?"]);
        app.ui.open = true;
        assert_eq!(keys(&app), ["n", "t", "Esc", "Tab", "?"]);
        app.ui.page = Page::Sync;
        assert_eq!(keys(&app), ["c", "Tab", "?"], "sync not running");
        app.ui.page = Page::Settings;
        assert_eq!(keys(&app), ["c", "Tab", "?"], "this device");
        app.ui.settings_cursor = 1;
        assert_eq!(keys(&app), ["↵", "Tab", "?"], "Connect a device");
        app.ui.settings_cursor = 99;
        assert_eq!(keys(&app), ["← →", "Tab", "?"], "clamped to the theme");
        app.ui.page = Page::Sessions;
        app.ui.dialog = Some(crate::tui::dialog::Dialog::Keys);
        assert!(keys(&app).is_empty());
        app.ui.dialog = None;
        app.ui.finding = true;
        assert!(matches!(app.footer(&app.body()), crate::tui::Footer::Find(_)));
        app.ui.finding = false;
        app.ui.confirm = Some(Confirm::Forget { device: "studio".into() });
        assert!(matches!(app.footer(&app.body()), crate::tui::Footer::Confirm { .. }));
        app.ui.confirm = None;
        app.say("Hello.");
        assert!(matches!(app.footer(&app.body()), crate::tui::Footer::Toast(t) if t == "Hello."));
    }

    #[test]
    fn the_settings_cursor_stays_on_an_item() {
        let mut app = App::for_test(Bridge::NoOp, Vec::new());
        app.ui.page = Page::Settings;
        let last = app.settings_view().items().len() - 1;
        for _ in 0..20 {
            press(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(app.ui.settings_cursor, last);
        for _ in 0..20 {
            press(&mut app, KeyCode::Char('k'));
        }
        assert_eq!(app.ui.settings_cursor, 0);
        app.ui.settings_cursor = 99;
        assert_eq!(app.settings_view().cursor, last, "a stale cursor reads as the last item");
    }

    #[test]
    fn sync_move_stays_within_the_folders() {
        use crate::sync::protocol::{DeviceState, FolderStatus};
        let mut app = App::for_test(Bridge::NoOp, Vec::new());
        app.sync_move(1);
        app.sync_move(-1);
        assert_eq!(app.ui.sync_cursor, 0, "no folders");
        app.agent.local = Some(DeviceState { folders: vec![FolderStatus::default(); 3], ..Default::default() });
        for _ in 0..5 {
            app.sync_move(1);
        }
        assert_eq!(app.ui.sync_cursor, 2);
        app.sync_move(-1);
        assert_eq!(app.ui.sync_cursor, 1);
        app.ui.sync_cursor = 9;
        app.sync_move(-1);
        assert_eq!(app.ui.sync_cursor, 1, "a stale cursor counts from the last folder");
    }
}
