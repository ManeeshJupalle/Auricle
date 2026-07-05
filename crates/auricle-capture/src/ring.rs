use std::sync::atomic::{AtomicU64, Ordering};

use cpal::Sample;
use rtrb::{Consumer, Producer};

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
    for frame in data.chunks_exact(channels) {
        let mut sum = 0.0f32;
        for s in frame {
            sum += f32::from_sample(*s);
        }
        let mono = sum / channels as f32;
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
