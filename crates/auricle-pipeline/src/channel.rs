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
        for event in self.vad.push(samples, timing.t_start_ms, timing.gap_before) {
            chunks.extend(self.chunker.on_event(event));
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
