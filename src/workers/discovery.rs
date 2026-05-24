use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::detect::claude::discover_sessions_with;
use crate::detect::process::ProcessTable;
use crate::log_warn;
use crate::signals::ShutdownFlag;
use crate::state::session::Session;

/// Handle to a background discovery worker. Drop the handle to ask the
/// worker to exit (the dropped receiver makes the worker's next `send`
/// fail, which it interprets as "consumer is gone, exit").
pub struct DiscoveryWorker {
    pub rx: Receiver<Vec<Session>>,
    /// Sender for the wake-channel. Kept alive so the worker's
    /// `recv_timeout(refresh_interval)` keeps timing out instead of
    /// hitting `Disconnected` and exiting. The FS watcher clones this
    /// sender and pushes to it on every event under `sessions_dir`.
    _wake: Sender<()>,
    /// File-system watcher on `~/.claude/sessions/`. Held by the
    /// handle so the watcher's background thread stays alive for the
    /// lifetime of the worker (closes A12 + L10). May be None when
    /// the directory didn't exist at spawn time or the platform
    /// watcher couldn't initialise.
    _watcher: Option<RecommendedWatcher>,
    _join: Option<JoinHandle<()>>,
}

/// Spawn a discovery worker. The worker repeatedly:
///   1. Refreshes its `ProcessTable` if older than `process_scan_interval`.
///   2. Calls `discover_sessions_with` to build a `Vec<Session>`.
///   3. Sends the snapshot through the channel.
///   4. Sleeps until `refresh_interval` elapses *or* the FS watcher
///      kicks the wake-channel because something under `sessions_dir`
///      changed.
///
/// Moving this off the UI thread (closes A20) means a slow disk or a
/// hung osascript fork can no longer freeze the TUI. Pairing it with
/// the FS watcher (closes A12 + L10) means an idle nerve sleeps
/// indefinitely — the FS event drives the wakes rather than a poll.
pub fn spawn(
    refresh_interval: Duration,
    process_scan_interval: Duration,
    sessions_dir: PathBuf,
    shutdown: ShutdownFlag,
) -> std::io::Result<DiscoveryWorker> {
    let (tx, rx) = channel();
    let (wake_tx, wake_rx) = channel::<()>();

    // Best-effort FS watcher. If the directory doesn't exist yet
    // (first-time setup) or the platform watcher refuses to start, we
    // fall back to pure-polling at `refresh_interval`.
    let watcher = install_fs_watcher(&sessions_dir, wake_tx.clone());

    let join = Builder::new()
        .name("nerve-discovery".into())
        .spawn(move || {
            let mut table = ProcessTable::refreshed();
            loop {
                if shutdown.requested() {
                    return;
                }

                table.refresh_if_stale(process_scan_interval);
                let sessions = discover_sessions_with(&table);
                if tx.send(sessions).is_err() {
                    // UI dropped the receiver — exit quietly.
                    return;
                }

                // Wait either for the next refresh tick or for an FS
                // event wake. Disconnected → graceful exit (the wake
                // sender lives on the handle that's about to drop).
                match wake_rx.recv_timeout(refresh_interval) {
                    Ok(_) => continue,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })?;

    Ok(DiscoveryWorker {
        rx,
        _wake: wake_tx,
        _watcher: watcher,
        _join: Some(join),
    })
}

/// Watch `sessions_dir` for changes and forward every relevant event
/// to `wake`. Returns `None` if the directory doesn't exist or the
/// watcher can't be created — the worker just falls back to its
/// timer-driven poll in that case.
fn install_fs_watcher(
    sessions_dir: &std::path::Path,
    wake: Sender<()>,
) -> Option<RecommendedWatcher> {
    if !sessions_dir.exists() {
        return None;
    }
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            // Coalesce: any create/remove/modify under ~/.claude/sessions/
            // is interesting. Access-only events are ignored.
            let interesting = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
            );
            if interesting {
                let _ = wake.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            log_warn!("discovery: FS watcher init failed ({e}); falling back to poll");
            return None;
        }
    };

    if let Err(e) = watcher.watch(sessions_dir, RecursiveMode::NonRecursive) {
        log_warn!(
            "discovery: cannot watch {}: {e}; falling back to poll",
            sessions_dir.display()
        );
        return None;
    }
    Some(watcher)
}

impl DiscoveryWorker {
    /// Drain all queued snapshots and return the most recent (the
    /// intermediate ones would only ever be displayed for one frame
    /// each, so coalescing to "latest wins" is correct). Returns
    /// `None` if the channel is empty.
    pub fn latest_snapshot(&self) -> Option<Vec<Session>> {
        let mut latest = None;
        while let Ok(snap) = self.rx.try_recv() {
            latest = Some(snap);
        }
        latest
    }

    /// Block (up to `timeout`) for the next discovery snapshot. Used
    /// only by `App::run` to seed the very first frame so the UI
    /// doesn't paint an empty screen for a second while waiting for
    /// the worker's first cycle. All subsequent reads use
    /// `latest_snapshot` (non-blocking).
    pub fn next_snapshot_blocking(&self, timeout: Duration) -> Option<Vec<Session>> {
        self.rx.recv_timeout(timeout).ok()
    }
}

// Drop is implicit: the receiver's drop causes the worker's next
// `tx.send` to fail, which it interprets as "exit". We don't `join`
// the thread because the worker may still be inside a discovery scan;
// the OS will reap it when the process exits.
