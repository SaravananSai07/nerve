/// Generic N-tick confirmation state machine. Proposed transitions only
/// commit after `threshold` consecutive confirmations — defends against
/// flicker in upstream signals (CPU spikes, transient file rewrites).
///
/// Intended to be wired into `Session` to debounce SessionState
/// transitions (closes A14). The integration is deferred to a follow-up
/// pass — the module and its tests stand alone so it can be wired in
/// without touching the logic.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StateMachine<T: PartialEq + Clone> {
    current: T,
    pending: Option<T>,
    count: u8,
    threshold: u8,
}

#[allow(dead_code)]
impl<T: PartialEq + Clone> StateMachine<T> {
    pub fn new(initial: T, threshold: u8) -> Self {
        debug_assert!(threshold > 0, "threshold must be at least 1");
        Self {
            current: initial,
            pending: None,
            count: 0,
            threshold,
        }
    }

    pub fn current(&self) -> &T {
        &self.current
    }

    /// Propose a transition to `next`. Returns true when the proposal has
    /// been confirmed `threshold` times in a row and the current value has
    /// been updated. A different proposal resets the counter.
    pub fn propose(&mut self, next: T) -> bool {
        if next == self.current {
            self.pending = None;
            self.count = 0;
            return false;
        }
        if self.pending.as_ref() == Some(&next) {
            self.count += 1;
        } else {
            self.pending = Some(next);
            self.count = 1;
        }
        if self.count >= self.threshold {
            self.current = self.pending.take().expect("pending is set when count > 0");
            self.count = 0;
            return true;
        }
        false
    }

    /// Force an immediate transition without confirmations. Resets pending
    /// state. Used when authority comes from a side channel (e.g., session
    /// vanished from discovery → mark Stale).
    pub fn set(&mut self, next: T) {
        if self.current != next {
            self.current = next;
        }
        self.pending = None;
        self.count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_threshold_confirmations() {
        let mut m = StateMachine::new("a", 3);
        assert!(!m.propose("b"));
        assert!(!m.propose("b"));
        assert!(m.propose("b"));
        assert_eq!(m.current(), &"b");
    }

    #[test]
    fn different_proposal_resets_counter() {
        let mut m = StateMachine::new("a", 3);
        assert!(!m.propose("b"));
        assert!(!m.propose("b"));
        // Switch proposal → counter resets
        assert!(!m.propose("c"));
        assert!(!m.propose("c"));
        assert!(m.propose("c"));
        assert_eq!(m.current(), &"c");
    }

    #[test]
    fn proposing_current_value_clears_pending() {
        let mut m = StateMachine::new("a", 3);
        m.propose("b");
        m.propose("b");
        // Step back to current → discards the pending transition.
        assert!(!m.propose("a"));
        // Next proposal starts fresh.
        assert!(!m.propose("b"));
    }

    #[test]
    fn set_forces_immediate_transition() {
        let mut m = StateMachine::new("a", 3);
        m.propose("b");
        m.set("c");
        assert_eq!(m.current(), &"c");
        // Pending is cleared too.
        assert!(!m.propose("b"));
    }

    #[test]
    fn threshold_of_one_transitions_immediately() {
        let mut m = StateMachine::new("a", 1);
        assert!(m.propose("b"));
        assert_eq!(m.current(), &"b");
    }
}
