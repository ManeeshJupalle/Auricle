/// Gap-aware sample clock for one channel.
///
/// Timestamps are `anchor + cumulative_sample_position / rate`, where the
/// anchor is set from the monotonic wall clock at stream start. When the wall
/// clock runs ahead of the sample clock by more than the gap threshold, the
/// clock re-anchors so the current batch ends "now". That happens for two
/// reasons, told apart by how long the batch took to arrive: the loopback
/// stream delivered nothing while the system was silent (a stall), or the
/// device's clock runs slow and the difference built up on a flowing stream
/// (drift). Timestamps never come from "samples received so far" alone, and
/// they never move backwards.
#[derive(Debug)]
pub struct Timeline {
    rate_hz: u32,
    gap_threshold_ms: u64,
    anchor_ms: f64,
    pos_samples: u64,
    started: bool,
    last_batch_wall_ms: u64,
}

/// Timing for one drained batch of samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatchTiming {
    /// Session-relative time of the batch's first sample.
    pub t_start_ms: u64,
    /// True when the clock jumped forward just before this batch, for either
    /// reason. Anything laid out on this clock has to jump with it.
    pub gap_before: bool,
    /// True when the jump was a stall: audio stopped arriving, so this batch
    /// isn't contiguous with the last. Drift leaves it clear.
    pub stalled: bool,
}

impl Timeline {
    pub fn new(rate_hz: u32, gap_threshold_ms: u64) -> Self {
        Timeline {
            rate_hz,
            gap_threshold_ms,
            anchor_ms: 0.0,
            pos_samples: 0,
            started: false,
            last_batch_wall_ms: 0,
        }
    }

    fn samples_to_ms(&self, samples: u64) -> f64 {
        samples as f64 * 1000.0 / self.rate_hz as f64
    }

    /// Record a batch of `n_samples` drained at wall time `wall_ms`
    /// (monotonic, session-relative) and return its timing.
    pub fn on_batch(&mut self, n_samples: usize, wall_ms: u64) -> BatchTiming {
        let n = n_samples as u64;
        let batch_ms = self.samples_to_ms(n);
        let waited_ms = wall_ms.saturating_sub(self.last_batch_wall_ms) as f64;
        self.last_batch_wall_ms = wall_ms;

        if !self.started {
            self.started = true;
            self.anchor_ms = (wall_ms as f64 - batch_ms).max(0.0);
            self.pos_samples = n;
            return BatchTiming {
                t_start_ms: self.anchor_ms as u64,
                gap_before: false,
                stalled: false,
            };
        }

        let expected_end_ms = self.anchor_ms + self.samples_to_ms(self.pos_samples + n);
        let gap_before = wall_ms as f64 > expected_end_ms + self.gap_threshold_ms as f64;
        // A late drain carries its backlog, so only a wait this batch's own
        // audio doesn't account for means the stream stopped.
        let stalled = gap_before && waited_ms > batch_ms + self.gap_threshold_ms as f64;
        if gap_before {
            // Re-anchor so this batch ends "now".
            // (Wall behind sample clock — a ring-buffer catch-up burst — is
            // NOT a gap; sample-derived timestamps stay authoritative.)
            self.anchor_ms = wall_ms as f64 - self.samples_to_ms(self.pos_samples + n);
        }

        let t_start_ms = (self.anchor_ms + self.samples_to_ms(self.pos_samples)) as u64;
        self.pos_samples += n;
        BatchTiming {
            t_start_ms,
            gap_before,
            stalled,
        }
    }

    /// True once no batch has arrived for more than the gap threshold.
    /// Measured from the last arrival rather than the sample clock, which a
    /// slow device leaves behind the wall clock while audio flows normally.
    pub fn is_stalled(&self, wall_ms: u64) -> bool {
        self.started && wall_ms.saturating_sub(self.last_batch_wall_ms) > self.gap_threshold_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 ms batches at 16 kHz.
    const BATCH: usize = 160;

    #[test]
    fn stall_is_measured_from_the_last_batch() {
        let mut tl = Timeline::new(16_000, 250);
        assert!(!tl.is_stalled(60_000), "nothing delivered yet");
        tl.on_batch(BATCH, 10);
        tl.on_batch(BATCH, 20);
        assert!(!tl.is_stalled(270), "exactly at the threshold");
        assert!(tl.is_stalled(271));
    }

    #[test]
    fn a_slow_device_clock_reanchors_without_reading_as_a_stall() {
        // 0.6% slow: 159 samples per 10 ms of wall time.
        let mut tl = Timeline::new(16_000, 250);
        let mut jumps = 0;
        for i in 1..=12_000 {
            let wall_ms = i * 10;
            let timing = tl.on_batch(159, wall_ms);
            assert!(!timing.stalled, "at {wall_ms}");
            assert!(!tl.is_stalled(wall_ms + 9), "at {wall_ms}");
            assert!(wall_ms - timing.t_start_ms <= 270, "at {wall_ms}");
            jumps += timing.gap_before as u32;
        }
        assert!(jumps >= 2, "the clock must still follow the wall: {jumps}");
    }

    #[test]
    fn a_real_stall_reads_as_one_on_a_drifted_clock_too() {
        let mut tl = Timeline::new(16_000, 250);
        tl.on_batch(BATCH, 10);
        tl.on_batch(159, 240); // 230 ms late: under the threshold, clock behind
        let t = tl.on_batch(BATCH, 5_000);
        assert!(t.gap_before && t.stalled);
    }

    #[test]
    fn continuous_flow_gives_contiguous_sample_timestamps() {
        let mut tl = Timeline::new(16_000, 250);
        let a = tl.on_batch(BATCH, 10);
        let b = tl.on_batch(BATCH, 20);
        let c = tl.on_batch(BATCH, 31); // 1 ms of wall jitter: not a gap
        assert!(!a.gap_before && !b.gap_before && !c.gap_before);
        assert_eq!(b.t_start_ms - a.t_start_ms, 10);
        assert_eq!(c.t_start_ms - b.t_start_ms, 10);
    }

    #[test]
    fn loopback_silence_gap_is_detected_and_reanchored() {
        let mut tl = Timeline::new(16_000, 250);
        tl.on_batch(BATCH, 10);
        let before = tl.on_batch(BATCH, 20);
        // System went silent for ~5 s: no samples delivered, wall moved on.
        let after = tl.on_batch(BATCH, 5_020);
        assert!(after.gap_before, "5 s stall must be flagged as a gap");
        assert!(after.stalled);
        // Re-anchored: the post-gap batch starts ~now minus its own length,
        // not at the pre-gap sample position.
        assert_eq!(after.t_start_ms, 5_010);
        assert!(after.t_start_ms > before.t_start_ms + 4_000);
    }

    #[test]
    fn catch_up_burst_is_not_a_gap_and_stays_monotonic() {
        let mut tl = Timeline::new(16_000, 250);
        tl.on_batch(BATCH, 10);
        // Drain hiccup: 4 batches arrive at once (wall behind sample clock).
        let b = tl.on_batch(4 * BATCH, 12);
        let c = tl.on_batch(BATCH, 55);
        assert!(!b.gap_before && !b.stalled);
        assert!(!c.gap_before && !c.stalled);
        assert!(c.t_start_ms >= b.t_start_ms + 40);
    }

    #[test]
    fn drain_jitter_below_threshold_is_not_a_gap() {
        let mut tl = Timeline::new(16_000, 250);
        tl.on_batch(BATCH, 10);
        let b = tl.on_batch(BATCH, 200); // 180 ms late but under 250 ms
        assert!(!b.gap_before);
        // Timestamps still sample-derived, unaffected by the jitter.
        assert_eq!(b.t_start_ms, 10);
    }

    #[test]
    fn consecutive_gaps_each_reanchor() {
        let mut tl = Timeline::new(16_000, 250);
        tl.on_batch(BATCH, 10);
        let a = tl.on_batch(BATCH, 2_010);
        assert!(a.gap_before);
        assert_eq!(a.t_start_ms, 2_000);
        let b = tl.on_batch(BATCH, 9_010);
        assert!(b.gap_before);
        assert_eq!(b.t_start_ms, 9_000);
    }
}
