use auricle_core::{Error, Result};
use voice_activity_detector::VoiceActivityDetector;

use crate::SpeechDetector;

/// Silero VAD V5 at 16 kHz: fixed 512-sample (32 ms) frames.
const FRAME_LEN: usize = 512;

/// Production speech detector backed by the Silero ONNX model (bundled in
/// the `voice_activity_detector` crate, run via onnxruntime).
pub struct SileroDetector {
    inner: VoiceActivityDetector,
}

impl SileroDetector {
    pub fn new() -> Result<Self> {
        let inner = VoiceActivityDetector::builder()
            .chunk_size(FRAME_LEN)
            .sample_rate(16_000i64)
            .build()
            .map_err(|e| Error::Vad(format!("building silero session: {e}")))?;
        Ok(SileroDetector { inner })
    }
}

impl SpeechDetector for SileroDetector {
    fn frame_len(&self) -> usize {
        FRAME_LEN
    }

    fn predict(&mut self, frame: &[f32]) -> f32 {
        self.inner.predict(frame.iter().copied())
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}
