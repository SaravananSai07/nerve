use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use crate::detect::claude::discover_sessions_with;
use crate::detect::process::ProcessTable;
use crate::signals::ShutdownFlag;
use crate::state::session::Session;

/// Handle to a background discovery worker. Drop the handle to ask the
/// worker to exit (the dropped receiver makes the worker's next `send`
/// fail, which it interprets as "consumer is gone, exit").
pub struct DiscoveryWorker {
    pub rx: Receiver<Vec<Session>>,
    /// Sender for the wake-channel. Kept alive so the worker's
    /// `recv_timeout(refresh_interval)` keeps timing out instead of
    /// hitting `Disconnected` and exiting. Phase 3F-2 will surface a
    /// `wake()` method on top of this for FS-event-driven refreshes.
    _wake: std::sync::mpsc::Sender<()>,
    _join: Option<JoinHandle<()>>,
}

/// Spawn a discovery worker. The worker repeatedly:
///   1. Refreshes its `ProcessTable` if older than `process_scan_interval`.
///   2. Calls `discover_sessions_with` to build a `Vec<Session>`.
///   3. Sends the snapshot through the channel.
///   4. Sleeps for `refresh_interval` (interruptible by shutdown).
///
/// Moving this off the UI thread (closes A20) means a slow disk or a
/// hung osascript fork can no longer freeze the TUI: the worker takes
/// the hit and the UI paints the last snapshot while it waits.
pub fn spawn(
    refresh_interval: Duration,
    process_scan_interval: Duration,
    shutdown: ShutdownFlag,
) -> std::io::Result<DiscoveryWorker> {
    let (tx, rx) = channel();
    // Side-channel for "wake up now" requests. The worker waits on a
    // `Receiver<()>` with a timeout equal to `refresh_interval`, which
    // means a future FS-event handler (Phase 3F-2) can interrupt the
    // sleep early. Keep the sender on the handle so the channel stays
    // alive even though no callers wake it yet.
    let (wake_tx, wake_rx) = channel::<()>();

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

                // Wait either for the next refresh tick or for a wake
                // signal. Treat Disconnected the same as a graceful
                // shutdown (the sender side is owned by us, so it can
                // only disconnect if something has gone very wrong).
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
        _join: Some(join),
    })
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
