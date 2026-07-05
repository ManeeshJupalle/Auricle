//! Streaming pipeline: gap-aware timeline, Silero VAD gate, chunker, and
//! transcript assembler.
//!
//! Phase 1 established that capture streams are GAPPY: event-driven WASAPI
//! loopback delivers nothing while the system renders silence, and either
//! channel may start, stop, or gap independently. Timestamps here therefore
//! come from a monotonic wall clock anchored at stream start plus cumulative
//! sample position, re-anchored whenever the wall clock runs ahead of the
//! sample clock (see [`Timeline`]). VAD spans are force-closed across gaps so
//! no phantom silence is synthesized.

mod assembler;
mod channel;
mod chunker;
mod silero;
mod timeline;
mod vad;

pub use assembler::{Assembler, PipelineEvent, TranscriptEvent};
pub use channel::ChannelPipeline;
pub use chunker::Chunker;
pub use silero::SileroDetector;
pub use timeline::Timeline;
pub use vad::{SpeechDetector, VadEvent, VadGate};

/// Tuning for the streaming pipeline. Defaults follow the architecture doc:
/// 300 ms pre-roll, 500 ms hangover, 5 s max chunk, ~0.5 s overlap.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub sample_rate_hz: u32,
    /// Speech probability at/above which a span opens (or stays open).
    pub vad_on_threshold: f32,
    /// Probability below which a frame counts toward the hangover.
    pub vad_off_threshold: f32,
    /// Audio kept from before the VAD trigger so word onsets aren't clipped.
    pub pre_roll_ms: u64,
    /// Trailing non-speech kept before a span closes.
    pub hangover_ms: u64,
    /// Emit a final chunk once this much unfinalized span audio accumulates.
    pub max_chunk_ms: u64,
    /// Overlap between consecutive final chunks within a span.
    pub overlap_ms: u64,
    /// Emit an interim (partial) chunk each time this much new audio arrives.
    pub interim_interval_ms: u64,
    /// Wall-vs-sample-clock divergence treated as a stream gap.
    pub gap_threshold_ms: u64,
    /// Minimum remainder worth emitting as a final chunk at span close.
    pub min_final_ms: u64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            sample_rate_hz: 16_000,
            vad_on_threshold: 0.5,
            vad_off_threshold: 0.35,
            pre_roll_ms: 300,
            hangover_ms: 500,
            max_chunk_ms: 5_000,
            overlap_ms: 500,
            interim_interval_ms: 1_000,
            gap_threshold_ms: 250,
            min_final_ms: 200,
        }
    }
}
