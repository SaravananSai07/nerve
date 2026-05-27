use std::time::Instant;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActivityHistory {
    buckets: [bool; 10],
    #[serde(skip)]
    last_update: Instant,
}

impl ActivityHistory {
    pub(crate) fn new() -> Self {
        Self {
            buckets: [false; 10],
            last_update: Instant::now(),
        }
    }

    pub(crate) fn record_activity(&mut self) {
        self.shift_if_needed();
        self.buckets[9] = true;
        self.last_update = Instant::now();
    }

    pub(crate) fn shift_if_needed(&mut self) {
        let elapsed = self.last_update.elapsed().as_secs();
        let shifts = (elapsed / 30).min(10) as usize;
        if shifts > 0 {
            self.buckets.rotate_left(shifts);
            for b in &mut self.buckets[(10 - shifts)..] {
                *b = false;
            }
            self.last_update = Instant::now();
        }
    }

    pub(crate) fn sparkline(&self) -> String {
        // U+2588 FULL BLOCK + U+2581 LOWER ONE-EIGHTH BLOCK reads as
        // a binary timeline on every monospace font. The prior ▓░
        // pair rendered with unevenly-spaced cells on Apple Terminal;
        // the middle-dot interim sat mid-line and broke the timeline's
        // baseline. `▁` sits flush at the baseline at the same width.
        self.buckets
            .iter()
            .map(|&active| if active { '█' } else { '▁' })
            .collect()
    }
}
