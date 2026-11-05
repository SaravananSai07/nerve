//! Cross-device sync. See docs/ARCHITECTURE.md for the design.
//!
//! - `protocol`: wire format (pure)
//! - `fleet`: device identity, peers, capabilities
//! - `transport`: Nostr + NIP-44, the seen-set, and the fleet's own relay
//! - `agent`: the per-device worker thread (`agent/` holds its
//!   approvals, synced folders, state publishing and transfers)
//! - `ipc`: CLI ⇄ agent: the spool, outcome lines, following a request
//! - `mailbox`: Claude hook ⇄ agent rendezvous for remote answers
//! - `modhub`: the nerve mod's Unix-socket bridge to the agent
//! - `inject`: typing into a session's tmux pane / Ghostty tab
//! - `chunks`: content-defined chunking and the chunk store
//! - `handoff`: pack/unpack/resume a session on another device, and
//!   start a new one there; both run `claude` in a detached tmux session
//! - `runs`: allowlisted plain-command runs
//! - `places`: files and git refs into named places
//! - `folders`: folders kept the same on several devices
//! - `versions`: what syncs replaced, kept for a while to undo
//! - `bins`: executable lookup that survives a service's bare PATH
//! - `connect`: `nerve connect`, the join handshake and invite codes
//! - `install`: the Claude mod or hooks, and the background service
//! - `cli`: `nerve sync ...`

pub(crate) mod agent;
pub(crate) mod bins;
pub(crate) mod chunks;
pub(crate) mod cli;
pub(crate) mod connect;
pub(crate) mod fleet;
pub(crate) mod folders;
pub(crate) mod handoff;
pub(crate) mod inject;
pub(crate) mod install;
pub(crate) mod ipc;
pub(crate) mod mailbox;
pub(crate) mod modhub;
pub(crate) mod places;
pub(crate) mod protocol;
pub(crate) mod runs;
pub(crate) mod transport;
pub(crate) mod versions;

/// After the TUI set this device up: give the new background agent a
/// moment to start, then attach to it (or run one, where none started).
pub(crate) fn start_after_setup(paths: &crate::paths::Paths, sound: bool) -> Option<agent::AgentHandle> {
    let dir = fleet::sync_dir(paths);
    for _ in 0..40 {
        if agent::agent_alive(&dir) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    start_for_tui(paths, sound)
}

/// Start the embedded agent for the TUI when this device is set up.
/// If another agent holds `agent.lock` (`nerve sync agent`, the
/// background service), the TUI gets a viewer (reads state, sends
/// requests) and leaves the work to it.
pub(crate) fn start_for_tui(paths: &crate::paths::Paths, sound: bool) -> Option<agent::AgentHandle> {
    let dir = fleet::sync_dir(paths);
    let fleet = fleet::Fleet::load(&dir)?;
    let lock = match agent::take_lock(&dir) {
        Ok(l) => Some(l),
        Err(crate::lockfile::LockError::AlreadyHeld { .. }) => None,
        Err(crate::lockfile::LockError::Io(e)) => {
            crate::log_warn!("sync: agent.lock: {e}; viewing only");
            None
        }
    };
    // Only the agent doing the work shows notifications, so a viewing
    // TUI next to `nerve sync agent` doesn't double them.
    let notifier = lock.is_some().then(|| crate::notify::Notifier::new(sound));
    agent::spawn(agent::Ctx {
        paths: paths.clone(),
        sync_dir: dir,
        fleet,
        statusline_dir: Some(paths.statusline_dir()),
        lock,
        notifier,
    })
    .ok()
}
