use auricle_core::{AudioChunk, ChannelId, ChunkKind};

use crate::{PipelineConfig, VadEvent};

/// Cuts speech spans into STT-sized chunks.
///
/// Within a span: a Final chunk is emitted whenever `max_chunk_ms` of
/// unfinalized audio accumulates, with consecutive finals overlapping by
/// `overlap_ms` (the assembler dedups the repeated words). Interim chunks
/// covering the unfinalized window are emitted every `interim_interval_ms`
/// for pseudo-streaming partials. Span close flushes the remainder as a
/// Final if it is at least `min_final_ms` long.
pub struct Chunker {
    session_id: String,
    channel: ChannelId,
    rate_hz: u32,
    max_chunk_samples: usize,
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
}

impl Chunker {
    pub fn new(session_id: &str, channel: ChannelId, cfg: &PipelineConfig) -> Self {
        let per_ms = cfg.sample_rate_hz as u64 / 1000;
        Chunker {
            session_id: session_id.to_string(),
            channel,
            rate_hz: cfg.sample_rate_hz,
            max_chunk_samples: (cfg.max_chunk_ms * per_ms) as usize,
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
                    let end = start + self.max_chunk_samples;
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
}
