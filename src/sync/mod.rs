//! Cross-device sync (`--features sync`). See docs/SYNC.md for the
//! design and research/sync/ for the experiments behind it.
//!
//! - `protocol`: wire format (pure)
//! - `fleet`: device identity, peers, capabilities
//! - `transport`: Nostr + NIP-44
//! - `agent`: the per-device worker thread
//! - `mailbox`: Claude hook ⇄ agent rendezvous for remote answers
//! - `inject`: typing into a session's tmux pane / Ghostty tab
//! - `handoff`: pack/unpack/resume a session on another device
//! - `jobs`: headless capability-routed jobs
//! - `runs`: allowlisted plain-command runs
//! - `places`: files and git refs into named places
//! - `bins`: executable lookup that survives a service's bare PATH
//! - `cli`: `nerve sync ...`

pub(crate) mod agent;
pub(crate) mod bins;
pub(crate) mod chunks;
pub(crate) mod cli;
pub(crate) mod fleet;
pub(crate) mod handoff;
pub(crate) mod inject;
pub(crate) mod jobs;
pub(crate) mod mailbox;
pub(crate) mod modhub;
pub(crate) mod pairing;
pub(crate) mod places;
pub(crate) mod protocol;
pub(crate) mod runs;
pub(crate) mod setup;
pub(crate) mod spool;
pub(crate) mod transport;

/// Start the embedded agent for the TUI when this device is set up.
/// If `nerve sync agent` already runs for this config, the TUI gets a
/// viewer (reads state, sends requests) and leaves the work to it.
pub(crate) fn start_for_tui(paths: &crate::paths::Paths) -> Option<agent::AgentHandle> {
    let dir = fleet::sync_dir(paths);
    let fleet = fleet::Fleet::load(&dir)?;
    let long_lived = !agent::agent_alive(&dir);
    agent::spawn(agent::Ctx { sync_dir: dir, fleet, statusline_dir: Some(paths.statusline_dir()), long_lived }).ok()
}
