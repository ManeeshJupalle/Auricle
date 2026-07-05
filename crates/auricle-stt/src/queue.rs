//! Bounded chunk queue shared by chunk-in/text-out sessions (whisper-local
//! and the batch cloud providers). Policy: transcript over partials —
//! measured and tuned in Phase 2 (see docs/PHASE2_PIPELINE_REPORT.md).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use auricle_core::{AudioChunk, ChunkKind, Segment, SttEvent};
use tokio::sync::Notify;

/// Max chunks queued per session before shedding kicks in. 8 chunks ≈ 40 s
/// of audio ≈ 2.5 MB: enough to absorb feed bursts (session flush) while
/// still bounding sustained overload.
pub(crate) const QUEUE_CAP: usize = 8;

/// State shared between a session handle and its inference/upload worker.
pub(crate) struct SharedQueue {
    pub queue: Mutex<VecDeque<AudioChunk>>,
    pub notify: Notify,
    pub closed: AtomicBool,
    pub dropped_chunks: AtomicU64,
    pub finals: Mutex<Vec<Segment>>,
}

impl SharedQueue {
    pub fn new() -> Self {
        SharedQueue {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            dropped_chunks: AtomicU64::new(0),
            finals: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue with the shed policy; wakes the worker when accepted.
    pub fn push(&self, chunk: AudioChunk) {
        let enqueued = {
            let mut q = self.queue.lock().unwrap();
            let (accepted, evicted) = enqueue_with_shed(&mut q, chunk, QUEUE_CAP);
            if !accepted || evicted {
                self.dropped_chunks.fetch_add(1, Ordering::Relaxed);
            }
            accepted
        };
        if enqueued {
            self.notify.notify_one();
        }
    }

    /// Emit the shed-count notice (if any) through `send`, once, at exit.
    pub fn shed_notice(&self) -> Option<SttEvent> {
        let dropped = self.dropped_chunks.load(Ordering::Relaxed);
        if dropped > 0 {
            Some(SttEvent::Error(format!(
                "inference queue shed {dropped} chunk(s) under load"
            )))
        } else {
            None
        }
    }
}

/// Dequeue policy: transcript first, freshness second.
///
/// Finals are processed in order and jump ahead of interims; any interim
/// older than the final it precedes covers audio the final already covers,
/// so it is silently discarded (superseded, not lost). With no finals
/// queued, only the *newest* interim is worth transcribing — older ones
/// describe a window that has since grown. This keeps chunk->final latency
/// at ~one inference instead of a whole queue of stale partials.
pub(crate) fn pop_next(q: &mut VecDeque<AudioChunk>) -> Option<AudioChunk> {
    if let Some(idx) = q.iter().position(|c| c.kind == ChunkKind::Final) {
        let chunk = q.remove(idx);
        q.drain(..idx); // interims older than this final: superseded
        chunk
    } else {
        let chunk = q.pop_back();
        q.clear(); // older interims: superseded by the one we took
        chunk
    }
}

/// Shed policy for the bounded queue. Returns `(accepted, evicted_something)`.
///
/// Partials are expendable, transcript is not: on overflow, evict the oldest
/// queued interim; if none is queued and the incoming chunk is itself an
/// interim, reject the incoming chunk instead of evicting a final. Only when
/// the queue is all finals AND the incoming chunk is a final (sustained
/// overload) is the oldest final evicted — counted and surfaced at session
/// end.
pub(crate) fn enqueue_with_shed(
    q: &mut VecDeque<AudioChunk>,
    chunk: AudioChunk,
    cap: usize,
) -> (bool, bool) {
    if q.len() < cap {
        q.push_back(chunk);
        return (true, false);
    }
    if let Some(idx) = q.iter().position(|c| c.kind == ChunkKind::Interim) {
        q.remove(idx);
        q.push_back(chunk);
        return (true, true);
    }
    if chunk.kind == ChunkKind::Interim {
        return (false, false); // never evict a final for a partial
    }
    q.pop_front();
    q.push_back(chunk);
    (true, true)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use auricle_core::ChannelId;

    pub(crate) fn chunk(kind: ChunkKind, seq: u64) -> AudioChunk {
        AudioChunk {
            channel: ChannelId::Mic,
            session_id: "t".to_string(),
            seq,
            kind,
            sample_rate_hz: 16_000,
            t_start_ms: seq * 1000,
            t_end_ms: seq * 1000 + 1000,
            samples: vec![0.0; 16],
        }
    }

    #[test]
    fn pop_prefers_first_final_and_discards_older_interims() {
        let mut q = VecDeque::new();
        q.push_back(chunk(ChunkKind::Interim, 0));
        q.push_back(chunk(ChunkKind::Interim, 1));
        q.push_back(chunk(ChunkKind::Final, 2));
        q.push_back(chunk(ChunkKind::Interim, 3));
        let popped = pop_next(&mut q).unwrap();
        assert_eq!((popped.kind, popped.seq), (ChunkKind::Final, 2));
        let rest: Vec<_> = q.iter().map(|c| c.seq).collect();
        assert_eq!(rest, vec![3], "older interims superseded, newer kept");
    }

    #[test]
    fn pop_takes_newest_interim_when_no_finals() {
        let mut q = VecDeque::new();
        q.push_back(chunk(ChunkKind::Interim, 0));
        q.push_back(chunk(ChunkKind::Interim, 1));
        q.push_back(chunk(ChunkKind::Interim, 2));
        let popped = pop_next(&mut q).unwrap();
        assert_eq!(popped.seq, 2, "newest interim wins");
        assert!(q.is_empty(), "stale interims discarded");
    }

    #[test]
    fn pop_preserves_final_order() {
        let mut q = VecDeque::new();
        q.push_back(chunk(ChunkKind::Final, 0));
        q.push_back(chunk(ChunkKind::Final, 1));
        assert_eq!(pop_next(&mut q).unwrap().seq, 0);
        assert_eq!(pop_next(&mut q).unwrap().seq, 1);
        assert!(pop_next(&mut q).is_none());
    }

    #[test]
    fn shed_prefers_oldest_interim() {
        let mut q = VecDeque::new();
        enqueue_with_shed(&mut q, chunk(ChunkKind::Final, 0), 3);
        enqueue_with_shed(&mut q, chunk(ChunkKind::Interim, 1), 3);
        enqueue_with_shed(&mut q, chunk(ChunkKind::Final, 2), 3);
        let (accepted, evicted) = enqueue_with_shed(&mut q, chunk(ChunkKind::Final, 3), 3);
        assert!(accepted && evicted);
        let kinds: Vec<_> = q.iter().map(|c| (c.kind, c.seq)).collect();
        assert_eq!(
            kinds,
            vec![
                (ChunkKind::Final, 0),
                (ChunkKind::Final, 2),
                (ChunkKind::Final, 3)
            ],
            "the interim was evicted, finals kept"
        );
    }

    #[test]
    fn incoming_interim_never_evicts_a_final() {
        let mut q = VecDeque::new();
        for i in 0..3 {
            enqueue_with_shed(&mut q, chunk(ChunkKind::Final, i), 3);
        }
        let (accepted, evicted) = enqueue_with_shed(&mut q, chunk(ChunkKind::Interim, 9), 3);
        assert!(!accepted && !evicted, "incoming partial dropped instead");
        assert!(q.iter().all(|c| c.kind == ChunkKind::Final));
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn sustained_final_overload_evicts_oldest_final() {
        let mut q = VecDeque::new();
        for i in 0..3 {
            enqueue_with_shed(&mut q, chunk(ChunkKind::Final, i), 3);
        }
        let (accepted, evicted) = enqueue_with_shed(&mut q, chunk(ChunkKind::Final, 3), 3);
        assert!(accepted && evicted);
        let seqs: Vec<_> = q.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3], "oldest final evicted");
    }
}
