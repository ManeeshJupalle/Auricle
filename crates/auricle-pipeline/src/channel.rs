use auricle_core::{AudioChunk, ChannelId};

use crate::{Chunker, PipelineConfig, SpeechDetector, Timeline, VadGate};

/// One channel's ingest path: gap-aware timeline -> VAD gate -> chunker.
/// Feed drained 16 kHz batches with their wall-clock arrival time; get back
/// STT-ready chunks.
pub struct ChannelPipeline<D: SpeechDetector> {
    timeline: Timeline,
    vad: VadGate<D>,
    chunker: Chunker,
}

impl<D: SpeechDetector> ChannelPipeline<D> {
    pub fn new(session_id: &str, channel: ChannelId, detector: D, cfg: &PipelineConfig) -> Self {
        ChannelPipeline {
            timeline: Timeline::new(cfg.sample_rate_hz, cfg.gap_threshold_ms),
            vad: VadGate::new(detector, cfg),
            chunker: Chunker::new(session_id, channel, cfg),
        }
    }

    /// Process a drained batch. `wall_ms` is monotonic session-relative time
    /// at drain (the timeline detects loopback silence gaps from it).
    pub fn push_batch(&mut self, samples: &[f32], wall_ms: u64) -> Vec<AudioChunk> {
        let timing = self.timeline.on_batch(samples.len(), wall_ms);
        let mut chunks = Vec::new();
        for event in self.vad.push(samples, timing.t_start_ms, timing.stalled) {
            chunks.extend(self.chunker.on_event(event));
        }
        chunks
    }

    /// Timer path for a stream that has gone quiet. `push_batch` only sees a
    /// gap when the next batch lands, and if rendering stopped mid-hangover
    /// that may be never, leaving the last sentence unfinalized. Call this
    /// periodically with the current `wall_ms`: once the stall passes the gap
    /// threshold, any open span closes and its remainder comes back as chunks.
    pub fn tick(&mut self, wall_ms: u64) -> Vec<AudioChunk> {
        let mut chunks = Vec::new();
        if self.timeline.is_stalled(wall_ms) {
            for event in self.vad.close_span() {
                chunks.extend(self.chunker.on_event(event));
            }
        }
        chunks
    }

    /// End of session: close any open span and flush the chunker.
    pub fn flush(&mut self) -> Vec<AudioChunk> {
        let mut chunks = Vec::new();
        for event in self.vad.flush() {
            chunks.extend(self.chunker.on_event(event));
        }
        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auricle_core::ChunkKind;

    /// Speech iff the frame's peak exceeds 0.5.
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

    const SPEECH: f32 = 0.8;
    const SILENCE: f32 = 0.0;

    fn pipeline() -> ChannelPipeline<EnergyDetector> {
        ChannelPipeline::new(
            "t",
            ChannelId::Loopback,
            EnergyDetector,
            &PipelineConfig::default(),
        )
    }

    /// Deliver `ms` of audio in 10 ms batches on an ideal wall clock,
    /// starting at `from_ms`.
    fn feed(
        p: &mut ChannelPipeline<EnergyDetector>,
        level: f32,
        from_ms: u64,
        ms: u64,
    ) -> Vec<AudioChunk> {
        let mut out = Vec::new();
        for i in 1..=ms / 10 {
            out.extend(p.push_batch(&[level; 160], from_ms + i * 10));
        }
        out
    }

    fn finals(chunks: &[AudioChunk]) -> Vec<&AudioChunk> {
        chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Final)
            .collect()
    }

    /// Speech that stops mid-hangover with the stream stalling at 2100 ms:
    /// the span is still open and nothing is coming to close it.
    fn stalled_mid_hangover() -> ChannelPipeline<EnergyDetector> {
        let mut p = pipeline();
        let chunks = feed(&mut p, SPEECH, 0, 2_000);
        assert!(finals(&chunks).is_empty());
        let chunks = feed(&mut p, SILENCE, 2_000, 100);
        assert!(finals(&chunks).is_empty(), "hangover has not run out");
        p
    }

    #[test]
    fn tick_finalizes_a_span_left_open_by_a_stalled_stream() {
        let mut p = stalled_mid_hangover();
        assert!(p.tick(2_200).is_empty(), "inside the gap threshold");
        assert!(p.tick(2_350).is_empty(), "exactly at the gap threshold");

        let chunks = p.tick(2_351);
        assert_eq!(chunks.len(), 1, "{chunks:#?}");
        assert_eq!(chunks[0].kind, ChunkKind::Final);
        assert!(chunks[0].t_start_ms <= 100, "{}", chunks[0].t_start_ms);
        assert!(
            (2_050..=2_100).contains(&chunks[0].t_end_ms),
            "the span ends at the pre-gap clock, got {}",
            chunks[0].t_end_ms
        );
    }

    #[test]
    fn tick_closes_a_span_only_once() {
        let mut p = stalled_mid_hangover();
        assert_eq!(p.tick(3_000).len(), 1);
        assert!(p.tick(3_100).is_empty());
        assert!(p.flush().is_empty(), "nothing left to emit at stop");
    }

    #[test]
    fn tick_never_fires_on_a_flowing_stream() {
        let mut p = pipeline();
        for i in 1..=300 {
            let wall_ms = i * 10;
            p.push_batch(&[SPEECH; 160], wall_ms);
            assert!(p.tick(wall_ms + 5).is_empty(), "at {wall_ms} ms");
        }
    }

    /// Two minutes of unbroken speech from a device whose clock runs 0.6% slow:
    /// 159 samples for every 10 ms of wall time. Real devices drift far less,
    /// so this only reaches the same 250 ms of divergence sooner. A tick lands
    /// just before each batch, where the wall clock is furthest ahead.
    fn slow_device_speech(p: &mut ChannelPipeline<EnergyDetector>) -> Vec<AudioChunk> {
        let mut chunks = Vec::new();
        for i in 1..=12_000 {
            let wall_ms = i * 10;
            chunks.extend(p.push_batch(&[SPEECH; 159], wall_ms));
            chunks.extend(p.tick(wall_ms + 9));
        }
        chunks
    }

    #[test]
    fn a_slow_device_clock_never_closes_a_span_during_speech() {
        let mut p = pipeline();
        let chunks = slow_device_speech(&mut p);

        // The chunker only cuts short when its span closes.
        let lengths: Vec<_> = finals(&chunks)
            .iter()
            .map(|c| c.t_end_ms - c.t_start_ms)
            .collect();
        assert!(lengths.len() >= 20, "{lengths:?}");
        assert!(lengths.iter().all(|ms| *ms >= 4_900), "{lengths:?}");
    }

    #[test]
    fn drift_is_corrected_at_the_next_span() {
        let mut p = pipeline();
        slow_device_speech(&mut p);
        // The span ends on its hangover, and the wall clock moves on.
        let end_ms = 120_000;
        feed(&mut p, SILENCE, end_ms, 1_000);

        let mut chunks = feed(&mut p, SPEECH, end_ms + 1_000, 1_000);
        chunks.extend(p.flush());
        let resumed = finals(&chunks);
        let resumed = resumed.last().expect("the new span has a final");
        assert!(
            resumed.t_end_ms.abs_diff(end_ms + 2_000) <= 300,
            "the new span starts back on the wall clock: {}",
            resumed.t_end_ms
        );
    }

    #[test]
    fn tick_before_any_audio_does_nothing() {
        assert!(pipeline().tick(60_000).is_empty());
    }

    #[test]
    fn audio_after_a_real_gap_opens_a_fresh_span() {
        let mut p = stalled_mid_hangover();
        let first = p.tick(3_000);
        assert_eq!(first.len(), 1);

        let mut chunks = feed(&mut p, SPEECH, 10_000, 1_000);
        chunks.extend(p.flush());
        let resumed = finals(&chunks);
        assert_eq!(resumed.len(), 1, "{resumed:#?}");
        assert!(
            resumed[0].t_start_ms >= 9_900,
            "re-anchored after the gap, got {}",
            resumed[0].t_start_ms
        );
    }

    #[test]
    fn a_late_batch_after_tick_continues_on_the_sample_clock() {
        // The drain stalled rather than the stream: tick closes the span,
        // then the backlog lands in one batch and no gap is flagged.
        let mut p = pipeline();
        feed(&mut p, SPEECH, 0, 2_000);
        let first = p.tick(2_300);
        assert_eq!(first.len(), 1);

        let backlog = vec![SPEECH; 16 * 320];
        let mut chunks = p.push_batch(&backlog, 2_310);
        chunks.extend(p.flush());
        let rest = finals(&chunks);
        assert_eq!(rest.len(), 1, "{rest:#?}");
        assert!(rest[0].t_start_ms >= first[0].t_end_ms);
        assert!(rest[0].t_end_ms <= 2_320, "{}", rest[0].t_end_ms);
    }
}
