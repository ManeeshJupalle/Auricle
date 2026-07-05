use std::fmt;

/// Monotonic milliseconds since the start of a capture session.
pub type TimestampMs = u64;

/// Which capture channel a block of audio came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelId {
    /// Microphone input ("You").
    Mic,
    /// System audio loopback ("Them").
    Loopback,
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelId::Mic => write!(f, "mic"),
            ChannelId::Loopback => write!(f, "loopback"),
        }
    }
}

/// Whether a chunk is a rolling interim window over an open speech span
/// (transcribed as a partial) or a definitive slice of the span
/// (transcribed as a final).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    Interim,
    Final,
}

/// A block of mono f32 audio cut from one channel's speech by the chunker.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub channel: ChannelId,
    pub session_id: String,
    /// Per-channel emission sequence number.
    pub seq: u64,
    pub kind: ChunkKind,
    pub sample_rate_hz: u32,
    pub t_start_ms: TimestampMs,
    pub t_end_ms: TimestampMs,
    pub samples: Vec<f32>,
}

impl AudioChunk {
    /// Duration of this chunk's audio in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate_hz == 0 {
            return 0;
        }
        self.samples.len() as u64 * 1000 / self.sample_rate_hz as u64
    }
}

/// A transcribed piece of one channel's audio.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub channel: ChannelId,
    pub t_start_ms: TimestampMs,
    pub t_end_ms: TimestampMs,
    pub text: String,
}

/// Events emitted by an STT session.
#[derive(Debug, Clone)]
pub enum SttEvent {
    /// Transient transcription of an interim window; superseded by a Final.
    Partial(Segment),
    /// Definitive transcription of a chunk; part of the transcript.
    Final(Segment),
    /// A recoverable provider error; the session keeps running.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_duration() {
        let c = AudioChunk {
            channel: ChannelId::Mic,
            session_id: "s".to_string(),
            seq: 0,
            kind: ChunkKind::Final,
            sample_rate_hz: 16_000,
            t_start_ms: 0,
            t_end_ms: 500,
            samples: vec![0.0; 8_000],
        };
        assert_eq!(c.duration_ms(), 500);
    }
}
