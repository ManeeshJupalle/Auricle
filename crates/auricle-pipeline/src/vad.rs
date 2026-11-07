use std::collections::VecDeque;

use crate::PipelineConfig;

/// Frame-level speech probability source. Abstracted so span logic is
/// testable without ONNX; the production impl is [`crate::SileroDetector`].
pub trait SpeechDetector: Send {
    /// Samples per prediction frame (512 at 16 kHz for Silero V5 = 32 ms).
    fn frame_len(&self) -> usize;
    /// Probability of speech in the frame, 0.0..=1.0.
    fn predict(&mut self, frame: &[f32]) -> f32;
    /// Clear internal recurrent state (called across stream gaps).
    fn reset(&mut self);
}

/// Events emitted by the VAD gate, consumed by the chunker.
#[derive(Debug, Clone, PartialEq)]
pub enum VadEvent {
    SpanOpened {
        t_start_ms: u64,
    },
    /// Audio belonging to the open span, in order (includes pre-roll and
    /// hangover padding).
    SpanAudio {
        samples: Vec<f32>,
    },
    /// Speech probability of the frame in the [`SpanAudio`](Self::SpanAudio)
    /// that follows, so the chunker can cut at a pause. Pre-roll has none.
    FrameProb {
        p: f32,
    },
    SpanClosed {
        t_end_ms: u64,
    },
}

/// Speech-span detector with pre-roll and hangover padding.
///
/// Feed contiguous batches via [`push`](Self::push); set `gap_before` when
/// the [`crate::Timeline`] flagged a stall — the open span (if any) is
/// force-closed at the pre-gap clock and the detector state is reset, so a
/// gap never produces phantom silence inside a span.
pub struct VadGate<D: SpeechDetector> {
    detector: D,
    on_threshold: f32,
    off_threshold: f32,
    pre_roll_samples: usize,
    hangover_ms: u64,
    frame_ms: u64,
    rate_hz: u32,
    /// Pending samples not yet forming a full frame.
    pending: Vec<f32>,
    /// Session-relative time of pending[0] / the next frame to process.
    clock_ms: f64,
    /// Ring of recent non-speech audio kept for span pre-roll.
    pre_roll: VecDeque<f32>,
    /// Milliseconds of continuous sub-threshold audio inside an open span.
    silence_ms: u64,
    open: bool,
}

impl<D: SpeechDetector> VadGate<D> {
    pub fn new(detector: D, cfg: &PipelineConfig) -> Self {
        let frame_len = detector.frame_len();
        VadGate {
            on_threshold: cfg.vad_on_threshold,
            off_threshold: cfg.vad_off_threshold,
            pre_roll_samples: (cfg.sample_rate_hz as u64 * cfg.pre_roll_ms / 1000) as usize,
            hangover_ms: cfg.hangover_ms,
            frame_ms: frame_len as u64 * 1000 / cfg.sample_rate_hz as u64,
            rate_hz: cfg.sample_rate_hz,
            detector,
            pending: Vec::new(),
            clock_ms: 0.0,
            pre_roll: VecDeque::new(),
            silence_ms: 0,
            open: false,
        }
    }

    /// Feed a batch of 16 kHz mono samples starting at `t_start_ms`.
    pub fn push(&mut self, samples: &[f32], t_start_ms: u64, gap_before: bool) -> Vec<VadEvent> {
        let mut events = Vec::new();

        if gap_before {
            self.close_open_span(&mut events);
            self.pending.clear();
            self.pre_roll.clear();
            self.detector.reset();
        }
        // An open span's audio is contiguous, so it keeps its own clock.
        // Between spans the frame clock follows the batch time, which is
        // where a correction for device clock drift reaches it.
        if !self.open {
            self.clock_ms = t_start_ms as f64 - self.pending_ms();
        }

        self.pending.extend_from_slice(samples);
        let frame_len = self.detector.frame_len();

        let mut offset = 0;
        while self.pending.len() - offset >= frame_len {
            let frame_t1 = self.clock_ms + self.frame_ms as f64;
            let p = {
                let frame = &self.pending[offset..offset + frame_len];
                self.detector.predict(frame)
            };

            if self.open {
                events.push(VadEvent::FrameProb { p });
                events.push(VadEvent::SpanAudio {
                    samples: self.pending[offset..offset + frame_len].to_vec(),
                });
                if p >= self.on_threshold {
                    self.silence_ms = 0;
                } else if p < self.off_threshold {
                    self.silence_ms += self.frame_ms;
                    if self.silence_ms >= self.hangover_ms {
                        events.push(VadEvent::SpanClosed {
                            t_end_ms: frame_t1 as u64,
                        });
                        self.open = false;
                        self.silence_ms = 0;
                    }
                }
            } else {
                for &s in &self.pending[offset..offset + frame_len] {
                    if self.pre_roll.len() == self.pre_roll_samples {
                        self.pre_roll.pop_front();
                    }
                    self.pre_roll.push_back(s);
                }
                if p >= self.on_threshold {
                    let pre_roll_ms = self.pre_roll.len() as f64 * 1000.0 / self.rate_hz as f64;
                    events.push(VadEvent::SpanOpened {
                        t_start_ms: (frame_t1 - pre_roll_ms).max(0.0) as u64,
                    });
                    events.push(VadEvent::SpanAudio {
                        samples: self.pre_roll.iter().copied().collect(),
                    });
                    self.pre_roll.clear();
                    self.open = true;
                    self.silence_ms = 0;
                }
            }

            self.clock_ms = frame_t1;
            offset += frame_len;
        }
        self.pending.drain(..offset);

        events
    }

    /// Close any open span (end of session or stream gap). Sub-frame pending
    /// audio (< 32 ms) is discarded.
    pub fn flush(&mut self) -> Vec<VadEvent> {
        let mut events = Vec::new();
        self.close_open_span(&mut events);
        self.pending.clear();
        self.pre_roll.clear();
        events
    }

    /// Close any open span and touch nothing else: if the stream really
    /// stopped, the next batch's gap resets the rest, and if the batch was
    /// only late, its audio must stay contiguous with the frames before it.
    pub fn close_span(&mut self) -> Vec<VadEvent> {
        let mut events = Vec::new();
        self.close_open_span(&mut events);
        events
    }

    fn close_open_span(&mut self, events: &mut Vec<VadEvent>) {
        if self.open {
            events.push(VadEvent::SpanClosed {
                t_end_ms: self.clock_ms as u64,
            });
            self.open = false;
            self.silence_ms = 0;
        }
    }

    fn pending_ms(&self) -> f64 {
        self.pending.len() as f64 * 1000.0 / self.rate_hz as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic detector: speech iff the frame's peak exceeds 0.5.
    struct EnergyDetector;
    impl SpeechDetector for EnergyDetector {
        fn frame_len(&self) -> usize {
            512
        }
        fn predict(&mut self, frame: &[f32]) -> f32 {
            if frame.iter().any(|s| s.abs() > 0.5) {
                0.9
            } else {
                0.1
            }
        }
        fn reset(&mut self) {}
    }

    fn gate() -> VadGate<EnergyDetector> {
        VadGate::new(EnergyDetector, &PipelineConfig::default())
    }

    fn silence(ms: u64) -> Vec<f32> {
        vec![0.0; (16 * ms) as usize]
    }

    fn speech(ms: u64) -> Vec<f32> {
        vec![0.8; (16 * ms) as usize]
    }

    fn opened_at(events: &[VadEvent]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|e| match e {
                VadEvent::SpanOpened { t_start_ms } => Some(*t_start_ms),
                _ => None,
            })
            .collect()
    }

    fn closed_at(events: &[VadEvent]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|e| match e {
                VadEvent::SpanClosed { t_end_ms } => Some(*t_end_ms),
                _ => None,
            })
            .collect()
    }

    fn span_audio_len(events: &[VadEvent]) -> usize {
        events
            .iter()
            .map(|e| match e {
                VadEvent::SpanAudio { samples } => samples.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn span_opens_with_pre_roll_and_closes_after_hangover() {
        let mut g = gate();
        let mut events = Vec::new();
        // 1 s silence, 1 s speech, 1 s silence — contiguous batches.
        events.extend(g.push(&silence(1000), 0, false));
        events.extend(g.push(&speech(1000), 1000, false));
        events.extend(g.push(&silence(1000), 2000, false));

        let opened = opened_at(&events);
        let closed = closed_at(&events);
        assert_eq!(opened.len(), 1, "exactly one span: {events:?}");
        assert_eq!(closed.len(), 1);
        // Pre-roll: span starts ~300 ms before the first speech frame.
        assert!(
            (660..=740).contains(&opened[0]),
            "span open {} should be ~700 (speech at 1000 minus 300 pre-roll)",
            opened[0]
        );
        // Hangover: span closes ~500 ms after speech ends at 2000.
        assert!(
            (2464..=2560).contains(&closed[0]),
            "span close {} should be ~2500",
            closed[0]
        );
        // Span audio covers open..close contiguously.
        let audio_ms = span_audio_len(&events) as u64 / 16;
        assert!(
            audio_ms >= closed[0] - opened[0] - 64,
            "span audio {audio_ms} ms vs span {} ms",
            closed[0] - opened[0]
        );
    }

    #[test]
    fn short_dip_inside_speech_does_not_split_the_span() {
        let mut g = gate();
        let mut events = Vec::new();
        events.extend(g.push(&speech(800), 0, false));
        events.extend(g.push(&silence(300), 800, false)); // dip < 500 ms hangover
        events.extend(g.push(&speech(800), 1100, false));
        events.extend(g.push(&silence(700), 1900, false));

        assert_eq!(
            opened_at(&events).len(),
            1,
            "dip must not split: {events:?}"
        );
        assert_eq!(closed_at(&events).len(), 1);
    }

    #[test]
    fn gap_force_closes_open_span_at_pre_gap_clock() {
        let mut g = gate();
        let mut events = Vec::new();
        events.extend(g.push(&speech(1000), 0, false));
        assert_eq!(opened_at(&events).len(), 1);
        assert!(closed_at(&events).is_empty(), "span still open before gap");

        // Stream stalls for 8 s (loopback went silent); next batch re-anchored.
        let after_gap = g.push(&speech(500), 9000, true);
        let closed = closed_at(&after_gap);
        assert_eq!(closed.len(), 1, "gap must force-close: {after_gap:?}");
        assert!(
            closed[0] <= 1000,
            "forced close at pre-gap clock, got {}",
            closed[0]
        );
        // And the new speech opens a fresh span at post-gap time.
        let reopened = opened_at(&after_gap);
        assert_eq!(reopened.len(), 1);
        assert!(
            reopened[0] >= 8900,
            "new span must start at re-anchored time, got {}",
            reopened[0]
        );
    }

    #[test]
    fn gap_during_silence_produces_no_phantom_span() {
        let mut g = gate();
        let mut events = Vec::new();
        events.extend(g.push(&silence(500), 0, false));
        // 10 s gap while nothing was open.
        events.extend(g.push(&silence(500), 10_500, true));
        assert!(events.is_empty(), "no spans from silence + gap: {events:?}");
    }

    #[test]
    fn post_gap_timestamps_use_reanchored_clock() {
        let mut g = gate();
        g.push(&silence(500), 0, false);
        let events = g.push(&speech(1000), 60_000, true);
        let opened = opened_at(&events);
        assert_eq!(opened.len(), 1);
        // Pre-roll buffer was cleared at the gap, so the span starts at the
        // re-anchored time (no stale pre-gap audio pulled in).
        assert!(
            opened[0] >= 59_900,
            "expected ~60000, got {} — stale pre-gap pre-roll?",
            opened[0]
        );
    }

    #[test]
    fn a_clock_correction_waits_for_the_span_to_end() {
        let mut g = gate();
        let mut events = g.push(&speech(1000), 0, false);
        // The timeline re-anchored 300 ms forward without a stall.
        events.extend(g.push(&silence(1000), 1300, false));
        let closed = closed_at(&events);
        assert_eq!(closed.len(), 1, "{events:?}");
        assert!(closed[0] <= 1600, "stayed on its own clock: {}", closed[0]);

        let reopened = opened_at(&g.push(&speech(500), 2300, false));
        assert_eq!(reopened.len(), 1);
        assert!(reopened[0] >= 2000, "joined the timeline: {}", reopened[0]);
    }

    #[test]
    fn flush_closes_open_span() {
        let mut g = gate();
        let mut events = g.push(&speech(1000), 0, false);
        events.extend(g.flush());
        assert_eq!(closed_at(&events).len(), 1);
    }

    #[test]
    fn flush_with_no_span_is_empty() {
        let mut g = gate();
        g.push(&silence(1000), 0, false);
        assert!(g.flush().is_empty());
    }

    #[test]
    fn open_span_reports_each_frame_probability_just_before_its_audio() {
        let mut g = gate();
        // 10 frames: the first opens the span, the other 9 fall inside it.
        let events = g.push(&speech(320), 0, false);
        let probs: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, VadEvent::FrameProb { .. }))
            .collect();
        assert_eq!(probs.len(), 9, "{events:?}");
        for (i, event) in probs {
            assert_eq!(event, &VadEvent::FrameProb { p: 0.9 });
            assert!(
                matches!(&events[i + 1], VadEvent::SpanAudio { samples } if samples.len() == 512)
            );
        }
    }
}
