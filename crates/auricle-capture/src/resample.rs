use auricle_core::{Error, Result};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Input frames fed to the sinc resampler per processing call.
const CHUNK_FRAMES: usize = 1024;

/// Mono sinc resampler (rubato) from a device's native rate to a target rate
/// (16 kHz for the STT pipeline). Created once per channel and reused; input
/// is buffered internally so callers can feed arbitrary-sized slices.
pub struct MonoResampler {
    /// None when input and output rates match (passthrough).
    inner: Option<SincFixedIn<f32>>,
    pending: Vec<f32>,
    pub in_rate_hz: u32,
    pub out_rate_hz: u32,
}

impl MonoResampler {
    pub fn new(in_rate_hz: u32, out_rate_hz: u32) -> Result<Self> {
        if in_rate_hz == 0 || out_rate_hz == 0 {
            return Err(Error::Resample("sample rates must be nonzero".to_string()));
        }
        let inner = if in_rate_hz == out_rate_hz {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            let ratio = out_rate_hz as f64 / in_rate_hz as f64;
            Some(
                SincFixedIn::<f32>::new(ratio, 1.0, params, CHUNK_FRAMES, 1)
                    .map_err(|e| Error::Resample(e.to_string()))?,
            )
        };
        Ok(MonoResampler {
            inner,
            pending: Vec::new(),
            in_rate_hz,
            out_rate_hz,
        })
    }

    /// Feed input samples, appending any produced output samples to `out`.
    /// Input shorter than the internal chunk size is buffered until enough
    /// has accumulated.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        let Some(rs) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return Ok(());
        };
        self.pending.extend_from_slice(input);
        let mut consumed = 0;
        while self.pending.len() - consumed >= CHUNK_FRAMES {
            let chunk = &self.pending[consumed..consumed + CHUNK_FRAMES];
            let frames = rs
                .process(&[chunk], None)
                .map_err(|e| Error::Resample(e.to_string()))?;
            out.extend_from_slice(&frames[0]);
            consumed += CHUNK_FRAMES;
        }
        self.pending.drain(..consumed);
        Ok(())
    }

    /// Flush buffered input at end of capture, appending output to `out`.
    ///
    /// rubato's `process_partial` zero-pads the remainder to a full chunk and
    /// emits a full chunk of output, so the result is truncated to
    /// `round(pending * ratio)` frames to keep total output length equal to
    /// `input * ratio` (the trailing samples are filter response to padding).
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let Some(rs) = self.inner.as_mut() else {
            return Ok(());
        };
        if !self.pending.is_empty() {
            let ratio = self.out_rate_hz as f64 / self.in_rate_hz as f64;
            let expected = (self.pending.len() as f64 * ratio).round() as usize;
            let frames = rs
                .process_partial(Some(&[&self.pending[..]]), None)
                .map_err(|e| Error::Resample(e.to_string()))?;
            let take = expected.min(frames[0].len());
            out.extend_from_slice(&frames[0][..take]);
            self.pending.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq_hz: f32, rate_hz: u32, seconds: f32) -> Vec<f32> {
        let n = (rate_hz as f32 * seconds) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq_hz * i as f32 / rate_hz as f32).sin())
            .collect()
    }

    fn resample_all(input: &[f32], in_rate: u32, out_rate: u32, piece: usize) -> Vec<f32> {
        let mut rs = MonoResampler::new(in_rate, out_rate).unwrap();
        let mut out = Vec::new();
        for p in input.chunks(piece) {
            rs.process(p, &mut out).unwrap();
        }
        rs.finish(&mut out).unwrap();
        out
    }

    /// Total output must be `input * ratio`, minus at most the sinc filter's
    /// startup delay (`sinc_len * ratio / 2`, ~21 frames at 48k->16k).
    fn assert_output_length(out_len: usize, in_len: usize, in_rate: u32, out_rate: u32) {
        let ratio = out_rate as f64 / in_rate as f64;
        let expected = (in_len as f64 * ratio).round() as i64;
        let delay = (128.0 * ratio / 2.0).ceil() as i64;
        let err = expected - out_len as i64;
        assert!(
            (0..=delay + 8).contains(&err),
            "expected ~{expected} samples (minus <= {delay} filter delay), got {out_len}"
        );
    }

    #[test]
    fn output_length_48k_to_16k() {
        // 1 s of input at 48 kHz must produce ~1 s of output at 16 kHz,
        // independent of how the input is sliced on the way in.
        let input = sine(440.0, 48_000, 1.0);
        for piece in [480, 1024, 4801] {
            let out = resample_all(&input, 48_000, 16_000, piece);
            assert_output_length(out.len(), input.len(), 48_000, 16_000);
        }
    }

    #[test]
    fn output_length_44100_to_16k_non_integer_ratio() {
        let input = sine(440.0, 44_100, 1.0);
        let out = resample_all(&input, 44_100, 16_000, 1000);
        assert_output_length(out.len(), input.len(), 44_100, 16_000);
    }

    #[test]
    fn pitch_is_preserved_no_chipmunk() {
        // A 400 Hz tone resampled 48k -> 16k must still be ~400 Hz: count
        // rising zero crossings over the 1 s output.
        let input = sine(400.0, 48_000, 1.0);
        let out = resample_all(&input, 48_000, 16_000, 1024);
        let crossings = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!(
            (390..=410).contains(&crossings),
            "expected ~400 rising zero crossings, got {crossings}"
        );
    }

    #[test]
    fn passthrough_when_rates_match() {
        let input = sine(440.0, 16_000, 0.25);
        let mut rs = MonoResampler::new(16_000, 16_000).unwrap();
        let mut out = Vec::new();
        rs.process(&input, &mut out).unwrap();
        rs.finish(&mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn zero_rate_is_a_clean_error() {
        assert!(MonoResampler::new(0, 16_000).is_err());
        assert!(MonoResampler::new(48_000, 0).is_err());
    }
}
