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

/// A block of mono f32 audio samples from one capture channel.
// PHASE-2: the chunker adds session id, sequence number, and t_end.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub channel: ChannelId,
    pub sample_rate_hz: u32,
    pub t_start_ms: TimestampMs,
    pub samples: Vec<f32>,
}

impl AudioChunk {
    /// Duration of this chunk in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate_hz == 0 {
            return 0;
        }
        self.samples.len() as u64 * 1000 / self.sample_rate_hz as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_duration() {
        let c = AudioChunk {
            channel: ChannelId::Mic,
            sample_rate_hz: 16_000,
            t_start_ms: 0,
            samples: vec![0.0; 8_000],
        };
        assert_eq!(c.duration_ms(), 500);
    }
}
