use crate::platform::SessionTarget;
use crate::state::session::{DiscoverySnapshot, Session, SessionState};
use crate::tui::preview::PreviewSource;

use super::{App, Overlay};

/// Notification queued by the discovery phase, dispatched after
/// the registry mutation has fully settled. Decoupling the two
/// stages means a slow `osascript` spawn can't interleave with a
/// mid-flight registry write.
pub(super) struct PendingNotification {
    pub(super) name: String,
    pub(super) state: SessionState,
    pub(super) target: SessionTarget,
}

impl App {
    pub(super) fn tick(&mut self) {
        self.refresh_sessions();
        // The eviction sweep only does anything once per minute (the
        // grace window), so polling it at the inner-loop cadence
        // (~50–250 ms) was clones-and-hashes for no result. Cap to
        // ~5 s and never miss a grace expiry by more than that.
        const STALE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
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
            self.set_status_error("discovery worker stopped — see ~/.config/nerve/nerve.log");
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
            let entries = super::load_log_entries(&jsonl_path);
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
    pub(super) fn apply_discovery(
        &mut self,
        discovered: Vec<DiscoverySnapshot>,
    ) -> Vec<PendingNotification> {
        let active_ids: std::collections::HashSet<&str> =
            discovered.iter().map(|s| s.id.as_str()).collect();

        let stale_ids: Vec<crate::state::session::SessionId> = self
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
                            target: SessionTarget::from(&*existing),
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
    pub(super) fn dispatch_notifications(&self, batch: Vec<PendingNotification>) {
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
