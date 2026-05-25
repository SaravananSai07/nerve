use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::detect::claude::discover_sessions_with;
use crate::detect::git::BranchCache;
use crate::detect::jsonl_cache::JsonlCache;
use crate::detect::process::ProcessTable;
use crate::signals::ShutdownFlag;
use crate::state::session::DiscoverySnapshot;
use crate::{log_err, log_warn};

/// Handle to a background discovery worker. Drop the handle to ask the
/// worker to exit (the dropped receiver makes the worker's next `send`
/// fail, which it interprets as "consumer is gone, exit").
pub struct DiscoveryWorker {
    pub rx: Receiver<Vec<DiscoverySnapshot>>,
    /// Sender for the wake-channel. Kept alive so the worker's
    /// `recv_timeout(refresh_interval)` keeps timing out instead of
    /// hitting `Disconnected` and exiting. The FS watcher clones this
    /// sender and pushes to it on every event under `sessions_dir`.
    _wake: Sender<()>,
    /// Held so the watcher's background thread stays alive for
    /// the worker's lifetime. `None` when the directory didn't
    /// exist at spawn time or the platform watcher couldn't
    /// initialise — the worker then falls back to its timer.
    _watcher: Option<RecommendedWatcher>,
    _join: Option<JoinHandle<()>>,
    /// Set to `false` if the worker thread caught a panic on its way
    /// down. The UI surfaces a status banner so a silently-frozen
    /// dashboard isn't mistaken for a working one.
    alive: Arc<AtomicBool>,
}

/// Spawn a discovery worker.
///   1. Refresh the `ProcessTable` if older than `process_scan_interval`.
///   2. Run `discover_sessions_with` to build a `Vec<Session>`.
///   3. Send the snapshot through the channel.
///   4. Sleep until either `refresh_interval` elapses or the FS
///      watcher kicks the wake-channel.
///
/// Discovery off the UI thread keeps a slow disk from freezing the
/// TUI. The FS watcher means an idle nerve sleeps indefinitely —
/// the FS event drives wakes rather than a poll.
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

    let alive = Arc::new(AtomicBool::new(true));
    let alive_for_thread = alive.clone();

    let join = Builder::new()
        .name("nerve-discovery".into())
        .spawn(move || {
            // Catch any panic that escapes the scan/parse/I/O — a
            // hostile JSONL or a future regression shouldn't be
            // allowed to silently freeze the UI behind a closed
            // channel.
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(move || {
                let mut table = ProcessTable::refreshed();
                let mut cache = JsonlCache::new();
                let mut branch_cache = BranchCache::new();
                loop {
                    if shutdown.requested() {
                        return;
                    }

                    table.refresh_if_stale(process_scan_interval);
                    let scan_start = std::time::Instant::now();
                    let sessions = discover_sessions_with(&table, &mut cache, &mut branch_cache);
                    let scan_elapsed = scan_start.elapsed();
                    // A full scan should take ~10-50 ms on local disk;
                    // persistent overruns indicate disk or network-
                    // mount slowness and warrant a trace.
                    if scan_elapsed > Duration::from_millis(750) {
                        log_warn!(
                            "discovery: scan took {} ms ({} sessions)",
                            scan_elapsed.as_millis(),
                            sessions.len()
                        );
                    }
                    if tx.send(sessions).is_err() {
                        // UI dropped the receiver — exit quietly.
                        return;
                    }

                    // Wait either for the next refresh tick or for an FS
                    // event wake. Disconnected → graceful exit (the wake
                    // sender lives on the handle that's about to drop).
                    match wake_rx.recv_timeout(refresh_interval) {
                        Ok(_) => {
                            // Coalesce a burst: notify fires multiple
                            // events for one logical change (write →
                            // close → flush → rename), so drain every
                            // pending wake before scanning again.
                            while wake_rx.try_recv().is_ok() {}
                            continue;
                        }
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => {
                            // Shouldn't happen — the `_wake` sender is
                            // held by the handle for the worker's
                            // lifetime — but log so a regression isn't
                            // silent.
                            log_warn!("discovery: wake channel disconnected unexpectedly");
                            return;
                        }
                    }
                }
            }));
            if let Err(payload) = outcome {
                let msg = panic_payload_str(&payload);
                log_err!("discovery worker panicked, exiting: {msg}");
                alive_for_thread.store(false, Ordering::Release);
            }
        })?;

    Ok(DiscoveryWorker {
        rx,
        _wake: wake_tx,
        _watcher: watcher,
        _join: Some(join),
        alive,
    })
}

fn panic_payload_str(payload: &Box<dyn std::any::Any + Send>) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
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
    pub fn latest_snapshot(&self) -> Option<Vec<DiscoverySnapshot>> {
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
    pub fn next_snapshot_blocking(&self, timeout: Duration) -> Option<Vec<DiscoverySnapshot>> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// False once the worker thread caught a panic on its way down.
    /// The UI uses this to surface a status banner — without it, a
    /// dead worker just looks like an idle one.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

// Drop is implicit: the receiver's drop causes the worker's next
// `tx.send` to fail, which it interprets as "exit". We don't `join`
// the thread because the worker may still be inside a discovery scan;
// the OS will reap it when the process exits.
