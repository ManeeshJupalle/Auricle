use auricle_core::{AudioChunk, ChannelId, ChunkKind};

use crate::{PipelineConfig, VadEvent};

/// How far before the hard limit a final may end to land in a pause.
const CUT_SEARCH_MS: u64 = 1_000;

/// Cuts speech spans into STT-sized chunks.
///
/// Within a span: a Final chunk is emitted whenever `max_chunk_ms` of
/// unfinalized audio accumulates, with consecutive finals overlapping by
/// `overlap_ms` (the assembler dedups the repeated words). It ends at the
/// lowest-probability VAD frame of the window's last second, the likeliest
/// gap between words, or at the hard limit when that region has no VAD data.
/// Interim chunks covering the unfinalized window are emitted every
/// `interim_interval_ms` for pseudo-streaming partials. Span close flushes
/// the remainder as a Final if it is at least `min_final_ms` long.
pub struct Chunker {
    session_id: String,
    channel: ChannelId,
    rate_hz: u32,
    max_chunk_samples: usize,
    cut_search_samples: usize,
    overlap_samples: usize,
    interim_samples: usize,
    min_final_samples: usize,
    seq: u64,
    span: Option<Span>,
}

struct Span {
    t_start_ms: u64,
    samples: Vec<f32>,
    /// Start (sample index) of the not-yet-finalized region.
    next_final_start: usize,
    /// Sample count at the last interim/final emission (gates interim cadence).
    last_emit_len: usize,
    /// (sample index where the frame starts, speech probability), ascending.
    frame_probs: Vec<(usize, f32)>,
}

impl Chunker {
    pub fn new(session_id: &str, channel: ChannelId, cfg: &PipelineConfig) -> Self {
        let per_ms = cfg.sample_rate_hz as u64 / 1000;
        Chunker {
            session_id: session_id.to_string(),
            channel,
            rate_hz: cfg.sample_rate_hz,
            max_chunk_samples: (cfg.max_chunk_ms * per_ms) as usize,
            cut_search_samples: (CUT_SEARCH_MS * per_ms) as usize,
            overlap_samples: (cfg.overlap_ms * per_ms) as usize,
            interim_samples: (cfg.interim_interval_ms * per_ms) as usize,
            min_final_samples: (cfg.min_final_ms * per_ms) as usize,
            seq: 0,
            span: None,
        }
    }

    /// Feed one VAD event; returns zero or more chunks ready for STT.
    pub fn on_event(&mut self, event: VadEvent) -> Vec<AudioChunk> {
        let mut out = Vec::new();
        match event {
            VadEvent::SpanOpened { t_start_ms } => {
                self.span = Some(Span {
                    t_start_ms,
                    samples: Vec::new(),
                    next_final_start: 0,
                    last_emit_len: 0,
                    frame_probs: Vec::new(),
                });
            }
            VadEvent::SpanAudio { samples } => {
                let Some(mut span) = self.span.take() else {
                    return out; // audio without an open span: ignore
                };
                span.samples.extend_from_slice(&samples);

                // Emit finals at the rolling-window boundary.
                while span.samples.len() - span.next_final_start >= self.max_chunk_samples {
                    let start = span.next_final_start;
                    let end = self.cut_point(&span, start);
                    out.push(self.make_chunk(&span, start, end, ChunkKind::Final));
                    span.next_final_start = end - self.overlap_samples;
                    span.last_emit_len = end;
                }

                // Emit an interim over the unfinalized window on cadence.
                if span.samples.len() - span.last_emit_len >= self.interim_samples {
                    let start = span.next_final_start;
                    let end = span.samples.len();
                    out.push(self.make_chunk(&span, start, end, ChunkKind::Interim));
                    span.last_emit_len = end;
                }

                self.span = Some(span);
            }
            VadEvent::FrameProb { p } => {
                if let Some(span) = &mut self.span {
                    span.frame_probs.push((span.samples.len(), p));
                }
            }
            VadEvent::SpanClosed { .. } => {
                if let Some(span) = self.span.take() {
                    let start = span.next_final_start;
                    let end = span.samples.len();
                    if end - start >= self.min_final_samples {
                        out.push(self.make_chunk(&span, start, end, ChunkKind::Final));
                    }
                }
            }
        }
        out
    }

    /// Where the final starting at `start` should end: the start of the
    /// lowest-probability frame, because Silero's dip trails the audio by
    /// about a frame, which puts that point at the end of the gap. Anything
    /// within one overlap of `start` is out of reach: cutting there would not
    /// move the next window forward.
    fn cut_point(&self, span: &Span, start: usize) -> usize {
        let limit = start + self.max_chunk_samples;
        let earliest = limit
            .saturating_sub(self.cut_search_samples)
            .max(start + self.overlap_samples + 1);
        let from = span.frame_probs.partition_point(|(i, _)| *i < earliest);
        let to = span.frame_probs.partition_point(|(i, _)| *i <= limit);
        span.frame_probs[from..to]
            .iter()
            .rev() // ties go to the latest frame, keeping the chunk long
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(limit, |(i, _)| *i)
    }

    fn make_chunk(&mut self, span: &Span, start: usize, end: usize, kind: ChunkKind) -> AudioChunk {
        let per_ms = self.rate_hz as u64 / 1000;
        let chunk = AudioChunk {
            channel: self.channel,
            session_id: self.session_id.clone(),
            seq: self.seq,
            kind,
            sample_rate_hz: self.rate_hz,
            t_start_ms: span.t_start_ms + start as u64 / per_ms,
            t_end_ms: span.t_start_ms + end as u64 / per_ms,
            samples: span.samples[start..end].to_vec(),
        };
        self.seq += 1;
        chunk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunker() -> Chunker {
        Chunker::new("test", ChannelId::Mic, &PipelineConfig::default())
    }

    fn feed_span(c: &mut Chunker, t_start_ms: u64, ms: u64) -> Vec<AudioChunk> {
        let mut out = c.on_event(VadEvent::SpanOpened { t_start_ms });
        // Feed in VAD-frame-sized pieces like the real gate does.
        let total = (ms * 16) as usize;
        let mut fed = 0;
        while fed < total {
            let n = 512.min(total - fed);
            out.extend(c.on_event(VadEvent::SpanAudio {
                samples: vec![0.1; n],
            }));
            fed += n;
        }
        out.extend(c.on_event(VadEvent::SpanClosed {
            t_end_ms: t_start_ms + ms,
        }));
        out
    }

    #[test]
    fn long_span_produces_overlapping_finals() {
        let mut c = chunker();
        let chunks = feed_span(&mut c, 1000, 12_000);
        let finals: Vec<_> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Final)
            .collect();
        // 12 s span: finals at [0,5], [4.5,9.5], remainder [9,12].
        assert_eq!(finals.len(), 3, "{finals:#?}");
        assert_eq!(finals[0].t_start_ms, 1000);
        assert_eq!(finals[0].t_end_ms, 6000);
        assert_eq!(finals[1].t_start_ms, 5500, "0.5 s overlap");
        assert_eq!(finals[1].t_end_ms, 10_500);
        assert_eq!(finals[2].t_start_ms, 10_000);
        assert_eq!(finals[2].t_end_ms, 13_000, "remainder runs to span end");
        // Chunk length never exceeds max.
        for f in &finals {
            assert!(f.samples.len() <= 80_000);
            assert_eq!(
                f.samples.len() as u64 / 16,
                f.t_end_ms - f.t_start_ms,
                "timestamps must match sample counts"
            );
        }
    }

    #[test]
    fn interims_cover_the_unfinalized_window() {
        let mut c = chunker();
        let chunks = feed_span(&mut c, 0, 3_500);
        let interims: Vec<_> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Interim)
            .collect();
        // 3.5 s span, interim every 1 s: at ~1s, ~2s, ~3s.
        assert_eq!(interims.len(), 3, "{interims:#?}");
        for i in interims {
            assert_eq!(i.t_start_ms, 0, "interim covers the span so far");
        }
        // Windows grow monotonically.
        let ends: Vec<_> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Interim)
            .map(|c| c.t_end_ms)
            .collect();
        assert!(ends.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn short_span_emits_single_final_no_interim() {
        let mut c = chunker();
        let chunks = feed_span(&mut c, 500, 900);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::Final);
        assert_eq!(chunks[0].t_start_ms, 500);
        // 900 ms rounded down to whole 512-sample frames.
        assert!((1350..=1450).contains(&chunks[0].t_end_ms));
    }

    #[test]
    fn tiny_remainder_is_dropped() {
        let mut c = chunker();
        let chunks = feed_span(&mut c, 0, 100); // below min_final_ms = 200
        assert!(chunks.is_empty(), "{chunks:#?}");
    }

    #[test]
    fn seq_increments_across_all_chunks() {
        let mut c = chunker();
        let a = feed_span(&mut c, 0, 6_000);
        let b = feed_span(&mut c, 20_000, 1_000);
        let seqs: Vec<_> = a.iter().chain(b.iter()).map(|c| c.seq).collect();
        let expected: Vec<_> = (0..seqs.len() as u64).collect();
        assert_eq!(seqs, expected);
    }

    #[test]
    fn metadata_is_carried_on_every_chunk() {
        let mut c = chunker();
        for chunk in feed_span(&mut c, 0, 2_000) {
            assert_eq!(chunk.session_id, "test");
            assert_eq!(chunk.channel, ChannelId::Mic);
            assert_eq!(chunk.sample_rate_hz, 16_000);
            assert!(chunk.t_end_ms > chunk.t_start_ms);
        }
    }

    const FRAME: usize = 512; // 32 ms at 16 kHz

    /// Open a span at t=0 and feed `frames` frames the way the real gate
    /// does: a probability, then its audio. Each sample holds its frame's
    /// index, so a chunk's contents show exactly which frames it covers.
    fn feed_frames(c: &mut Chunker, frames: usize, prob: impl Fn(usize) -> f32) -> Vec<AudioChunk> {
        let mut out = c.on_event(VadEvent::SpanOpened { t_start_ms: 0 });
        for i in 0..frames {
            out.extend(c.on_event(VadEvent::FrameProb { p: prob(i) }));
            out.extend(c.on_event(VadEvent::SpanAudio {
                samples: vec![i as f32; FRAME],
            }));
        }
        out
    }

    fn finals(chunks: &[AudioChunk]) -> Vec<&AudioChunk> {
        chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Final)
            .collect()
    }

    #[test]
    fn final_cuts_at_the_lowest_vad_frame_of_the_last_second() {
        let mut c = chunker();
        // A pause at frame 140 = 4480 ms, inside the 4-5 s search region.
        let chunks = feed_frames(&mut c, 170, |i| if i == 140 { 0.05 } else { 0.9 });
        let finals = finals(&chunks);
        assert_eq!(finals.len(), 1, "{finals:#?}");
        assert_eq!(finals[0].t_end_ms, 4480);
        assert_eq!(finals[0].samples.len(), 140 * FRAME);

        // The next window starts one overlap before the cut, not the limit.
        let rest = c.on_event(VadEvent::SpanClosed { t_end_ms: 5440 });
        assert_eq!(rest[0].t_start_ms, 4480 - 500);
    }

    #[test]
    fn a_pause_before_the_last_second_is_not_a_cut_point() {
        let mut c = chunker();
        let chunks = feed_frames(&mut c, 170, |i| if i == 100 { 0.05 } else { 0.9 });
        let end = finals(&chunks)[0].t_end_ms;
        assert!((4000..=5000).contains(&end), "cut at {end} ms");
    }

    #[test]
    fn the_lowest_of_several_dips_wins() {
        let mut c = chunker();
        let chunks = feed_frames(&mut c, 170, |i| match i {
            130 => 0.4,
            145 => 0.1,
            150 => 0.3,
            _ => 0.9,
        });
        assert_eq!(finals(&chunks)[0].t_end_ms, 145 * 32);
    }

    #[test]
    fn pause_cuts_lose_no_span_audio() {
        let mut c = chunker();
        let frames = 1000; // 32 s: several cuts, then a remainder at close
        let mut chunks = feed_frames(&mut c, frames, |i| if i % 37 == 0 { 0.05 } else { 0.9 });
        chunks.extend(c.on_event(VadEvent::SpanClosed { t_end_ms: 32_000 }));
        let finals = finals(&chunks);
        assert!(finals.len() >= 7, "{}", finals.len());

        let mut covered = vec![false; frames];
        for f in &finals {
            for s in &f.samples {
                covered[*s as usize] = true;
            }
        }
        let missing: Vec<_> = (0..frames).filter(|i| !covered[*i]).collect();
        assert!(missing.is_empty(), "frames lost: {missing:?}");

        for w in finals.windows(2) {
            assert_eq!(w[1].t_start_ms, w[0].t_end_ms - 500, "0.5 s overlap");
        }
        for f in &finals {
            assert!(f.samples.len() <= 80_000);
            assert_eq!(f.samples.len() as u64 / 16, f.t_end_ms - f.t_start_ms);
        }
    }

    #[test]
    fn interims_after_a_cut_start_at_the_new_window() {
        let mut c = chunker();
        let chunks = feed_frames(&mut c, 200, |i| if i == 140 { 0.05 } else { 0.9 });
        let after_cut: Vec<_> = chunks
            .iter()
            .skip_while(|c| c.kind != ChunkKind::Final)
            .filter(|c| c.kind == ChunkKind::Interim)
            .collect();
        assert!(!after_cut.is_empty());
        for i in after_cut {
            assert_eq!(i.t_start_ms, 4480 - 500);
        }
    }

    #[test]
    fn a_window_barely_wider_than_the_overlap_still_advances() {
        // The search region spans the whole window here, so an unguarded cut
        // near its start would leave next_final_start where it was, forever.
        let cfg = PipelineConfig {
            max_chunk_ms: 1_000,
            overlap_ms: 500,
            ..PipelineConfig::default()
        };
        let mut c = Chunker::new("test", ChannelId::Mic, &cfg);
        let chunks = feed_frames(&mut c, 200, |i| if i % 32 == 1 { 0.01 } else { 0.9 });
        let starts: Vec<_> = finals(&chunks).iter().map(|f| f.t_start_ms).collect();
        assert!(starts.len() > 3, "{starts:?}");
        assert!(starts.windows(2).all(|w| w[0] < w[1]), "{starts:?}");
    }
}
