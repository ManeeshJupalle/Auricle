use std::sync::atomic::{AtomicU64, Ordering};

use cpal::Sample;
use rtrb::{Consumer, Producer};

/// A front channel below this counts as silent: about -80 dBFS, under any
/// speech and over the dither some endpoints leave on an unused channel.
const SILENT: f32 = 1e-4;

/// Drain every currently-available sample from the ring buffer into `out`.
/// Returns the number of samples drained. Runs on the pipeline side (not the
/// real-time callback), so growing `out` is fine here.
pub fn drain_into(consumer: &mut Consumer<f32>, out: &mut Vec<f32>) -> usize {
    let n = consumer.slots();
    if n == 0 {
        return 0;
    }
    let Ok(chunk) = consumer.read_chunk(n) else {
        return 0;
    };
    let (a, b) = chunk.as_slices();
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    chunk.commit_all();
    n
}

/// Downmix interleaved frames to mono and push them into the ring buffer.
///
/// This is the real-time callback path: no allocation, no locks, no logging.
/// When the ring buffer is full, samples are dropped and counted.
#[inline]
pub(crate) fn push_downmixed<T>(
    data: &[T],
    channels: usize,
    producer: &mut Producer<f32>,
    dropped: &AtomicU64,
) where
    T: cpal::Sample,
    f32: cpal::FromSample<T>,
{
    if channels == 0 {
        return;
    }
    // Front left and right only: stereo content on a surround endpoint leaves
    // the other channels silent, and averaging them in would dilute it (about
    // 12 dB on 7.1). If that pair is silent the signal is elsewhere: dialogue
    // on the centre alone, or a mic on inputs 3 and 4. Chosen per buffer, as
    // per frame every zero crossing of the front pair would click.
    let front_silent = channels > 2
        && data.chunks_exact(channels).all(|frame| {
            frame[..2]
                .iter()
                .all(|s| f32::from_sample(*s).abs() < SILENT)
        });
    let (first, end) = if front_silent {
        (2, channels)
    } else {
        (0, channels.min(2))
    };
    for frame in data.chunks_exact(channels) {
        let mut sum = 0.0f32;
        for s in &frame[first..end] {
            sum += f32::from_sample(*s);
        }
        let mono = sum / (end - first) as f32;
        if producer.push(mono).is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    #[test]
    fn drain_returns_written_samples_in_order() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(16);
        let dropped = AtomicU64::new(0);
        push_downmixed::<f32>(&[0.1, 0.2, 0.3, 0.4], 1, &mut prod, &dropped);

        let mut out = Vec::new();
        let n = drain_into(&mut cons, &mut out);
        assert_eq!(n, 4);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn drain_handles_wraparound() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(4);
        let dropped = AtomicU64::new(0);
        let mut out = Vec::new();

        // Fill, drain, fill again: the second fill wraps the ring's internal
        // indices, producing a two-slice read.
        push_downmixed::<f32>(&[1.0, 2.0, 3.0], 1, &mut prod, &dropped);
        drain_into(&mut cons, &mut out);
        push_downmixed::<f32>(&[4.0, 5.0, 6.0], 1, &mut prod, &dropped);
        out.clear();
        let n = drain_into(&mut cons, &mut out);

        assert_eq!(n, 3);
        assert_eq!(out, vec![4.0, 5.0, 6.0]);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn stereo_downmix_averages_channels() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(8);
        let dropped = AtomicU64::new(0);
        // Two stereo frames: (0.2, 0.4) -> 0.3, (-1.0, 1.0) -> 0.0
        push_downmixed::<f32>(&[0.2, 0.4, -1.0, 1.0], 2, &mut prod, &dropped);

        let mut out = Vec::new();
        drain_into(&mut cons, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.3).abs() < 1e-6);
        assert!(out[1].abs() < 1e-6);
    }

    #[test]
    fn surround_downmix_uses_the_front_pair_only() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(8);
        let dropped = AtomicU64::new(0);
        // Stereo content on a 7.1 endpoint: only FL/FR carry signal, so the
        // level must match the stereo downmix, not be diluted 4x.
        push_downmixed::<f32>(
            &[0.2, 0.4, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            8,
            &mut prod,
            &dropped,
        );
        // Whatever the other channels carry stays out of the mix.
        push_downmixed::<f32>(
            &[0.2, 0.4, 0.9, 0.9, 0.9, 0.9, 0.9, 0.9],
            8,
            &mut prod,
            &dropped,
        );
        // 5.1, two frames.
        push_downmixed::<f32>(
            &[0.6, 0.0, 0.9, 0.9, 0.9, 0.9, -0.5, -0.1, 0.9, 0.9, 0.9, 0.9],
            6,
            &mut prod,
            &dropped,
        );

        let mut out = Vec::new();
        drain_into(&mut cons, &mut out);
        assert_eq!(out.len(), 4);
        for (got, want) in out.iter().zip([0.3, 0.3, 0.3, -0.3]) {
            assert!((got - want).abs() < 1e-6, "{out:?}");
        }
    }

    fn mix<T>(data: &[T], channels: usize) -> Vec<f32>
    where
        T: cpal::Sample,
        f32: cpal::FromSample<T>,
    {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(data.len().max(1));
        push_downmixed::<T>(data, channels, &mut prod, &AtomicU64::new(0));
        let mut out = Vec::new();
        drain_into(&mut cons, &mut out);
        out
    }

    fn assert_close(got: &[f32], want: &[f32]) {
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-3, "{got:?}");
        }
    }

    #[test]
    fn a_silent_front_pair_falls_back_to_the_other_channels() {
        // 5.1 with the dialogue on the centre channel alone.
        let centre_only = [0.0, 0.0, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0, -0.4, 0.0, 0.0, 0.0];
        assert_close(&mix::<f32>(&centre_only, 6), &[0.15, -0.1]);
        // An interface with the mic on inputs 3 and 4.
        assert_close(&mix::<f32>(&[0.0, 0.0, 0.5, 0.3], 4), &[0.4]);
        // Three channels, the smallest layout with anything to fall back to.
        assert_close(&mix::<f32>(&[0.0, 0.0, 0.5], 3), &[0.5]);
    }

    #[test]
    fn a_front_pair_at_the_noise_floor_counts_as_silent() {
        assert_close(&mix::<f32>(&[0.00001, -0.00001, 0.5, 0.5], 4), &[0.5]);
    }

    #[test]
    fn integer_samples_fall_back_too() {
        assert_close(&mix::<i16>(&[0, 0, 16_384, 16_384], 4), &[0.5]);
        assert_close(&mix::<u8>(&[128, 128, 192, 192], 4), &[0.5]);
    }

    #[test]
    fn the_choice_is_per_buffer_so_a_zero_crossing_cannot_click() {
        // The first frame's front pair crosses zero while the rest is loud.
        assert_close(
            &mix::<f32>(&[0.0, 0.0, 0.9, 0.9, 0.2, 0.4, 0.9, 0.9], 4),
            &[0.0, 0.3],
        );
    }

    #[test]
    fn mono_and_stereo_have_nothing_to_fall_back_to() {
        assert_close(&mix::<f32>(&[0.0, 0.0], 1), &[0.0, 0.0]);
        assert_close(&mix::<f32>(&[0.0, 0.0], 2), &[0.0]);
    }

    #[test]
    fn overflow_increments_drop_counter_and_keeps_earlier_samples() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(2);
        let dropped = AtomicU64::new(0);
        push_downmixed::<f32>(&[1.0, 2.0, 3.0, 4.0, 5.0], 1, &mut prod, &dropped);

        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        let mut out = Vec::new();
        let n = drain_into(&mut cons, &mut out);
        assert_eq!(n, 2);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn drain_on_empty_ring_is_zero() {
        let (_prod, mut cons) = RingBuffer::<f32>::new(4);
        let mut out = Vec::new();
        assert_eq!(drain_into(&mut cons, &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn i16_samples_convert_to_f32() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(4);
        let dropped = AtomicU64::new(0);
        push_downmixed::<i16>(&[i16::MAX, 0], 1, &mut prod, &dropped);

        let mut out = Vec::new();
        drain_into(&mut cons, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 1.0).abs() < 1e-3);
        assert!(out[1].abs() < 1e-6);
    }
}
