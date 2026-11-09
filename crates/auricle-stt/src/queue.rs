//! Bounded chunk queue shared by chunk-in/text-out sessions (whisper-local
//! and the batch cloud providers). Policy: transcript over partials —
//! measured and tuned in Phase 2 (see docs/PHASE2_PIPELINE_REPORT.md).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use auricle_core::{AudioChunk, ChunkKind, Segment, SttEvent};
use tokio::sync::Notify;

use crate::SttDrops;

/// Max chunks queued per session before shedding kicks in. 8 chunks ≈ 40 s
/// of audio ≈ 2.5 MB: enough to absorb feed bursts (session flush) while
/// still bounding sustained overload.
pub(crate) const QUEUE_CAP: usize = 8;

/// Finals a worker may have ahead of it and still take interims. Past this it
/// is behind, and a partial would only be stale by the time it ran.
const MAX_FINALS_AHEAD: usize = 1;

/// What a session has shed, as numbers: the notices announcing it are for
/// people, and this is what ends up in the session's loss record.
#[derive(Default)]
pub(crate) struct DropCounts {
    /// Every shed chunk, interim or final.
    pub chunks: AtomicU64,
    /// The finals among `chunks`: transcript lost, not a skipped partial.
    pub finals: AtomicU64,
}

impl DropCounts {
    /// Tally one [`enqueue_with_shed`] outcome.
    pub fn record(&self, accepted: bool, evicted: Option<ChunkKind>) {
        if !accepted {
            self.lost(ChunkKind::Interim);
        }
        if let Some(kind) = evicted {
            self.lost(kind);
        }
    }

    pub fn lost(&self, kind: ChunkKind) {
        self.chunks.fetch_add(1, Ordering::Relaxed);
        if kind == ChunkKind::Final {
            self.finals.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn totals(&self) -> SttDrops {
        SttDrops {
            chunks: self.chunks.load(Ordering::Relaxed),
            finals: self.finals.load(Ordering::Relaxed),
        }
    }

    /// Suffix for shed notices: dropped partials are routine, dropped finals
    /// are missing transcript and must read differently.
    fn lost_finals_note(&self) -> String {
        match self.finals.load(Ordering::Relaxed) {
            0 => String::new(),
            n => format!(", {n} final(s) lost from the transcript"),
        }
    }
}

/// State shared between a session handle and its inference/upload worker.
pub(crate) struct SharedQueue {
    pub queue: Mutex<VecDeque<AudioChunk>>,
    pub notify: Notify,
    pub closed: AtomicBool,
    pub dropped: DropCounts,
    /// The worker is still on a final it took. Only touched under `queue`.
    final_in_hand: AtomicBool,
    pub finals: Mutex<Vec<Segment>>,
}

impl SharedQueue {
    pub fn new() -> Self {
        SharedQueue {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            dropped: DropCounts::default(),
            final_in_hand: AtomicBool::new(false),
            finals: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue with the shed policy; wakes the worker when accepted. An
    /// interim is skipped while the worker is behind on finals: that is
    /// routine backpressure, so it isn't counted as shed.
    pub fn push(&self, chunk: AudioChunk) {
        let enqueued = {
            let mut q = self.queue.lock().unwrap();
            if chunk.kind == ChunkKind::Interim && self.finals_ahead(&q) > MAX_FINALS_AHEAD {
                return;
            }
            let (accepted, evicted) = enqueue_with_shed(&mut q, chunk, QUEUE_CAP);
            self.dropped.record(accepted, evicted);
            accepted
        };
        if enqueued {
            self.notify.notify_one();
        }
    }

    /// Finals the worker has not finished with, counted from what is really
    /// there: a final that was shed is simply gone from the queue.
    fn finals_ahead(&self, q: &VecDeque<AudioChunk>) -> usize {
        let queued = q.iter().filter(|c| c.kind == ChunkKind::Final).count();
        queued + self.final_in_hand.load(Ordering::Relaxed) as usize
    }

    /// The worker's next chunk. Coming back for one is how it reports the
    /// last one fully handled, whatever became of it: a final that yields no
    /// text, or fails, emits no `SttEvent::Final` to count.
    pub fn next_chunk(&self) -> Option<AudioChunk> {
        let mut q = self.queue.lock().unwrap();
        let chunk = pop_next(&mut q);
        let is_final = chunk.as_ref().is_some_and(|c| c.kind == ChunkKind::Final);
        self.final_in_hand.store(is_final, Ordering::Relaxed);
        chunk
    }

    /// Stop accepting work and wake a parked worker so it can drain and exit.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.notify.notify_one();
    }

    /// Emit the shed-count notice (if any) through `send`, once, at exit.
    pub fn shed_notice(&self) -> Option<SttEvent> {
        let dropped = self.dropped.chunks.load(Ordering::Relaxed);
        if dropped > 0 {
            Some(SttEvent::Error(format!(
                "inference queue shed {dropped} chunk(s) under load{}",
                self.dropped.lost_finals_note()
            )))
        } else {
            None
        }
    }
}

/// Minimum spacing between live shed notices from one worker.
const SHED_NOTICE_INTERVAL: Duration = Duration::from_secs(10);

/// Throttled live overload reporting for the worker loops: the end-of-session
/// total (`shed_notice`) is too late to act on — this surfaces shedding while
/// it is happening so the UI can warn during the meeting.
pub(crate) struct ShedReporter {
    /// Opens each notice: why this worker is shedding.
    cause: &'static str,
    reported: u64,
    last: Option<Instant>,
}

impl ShedReporter {
    pub fn new(cause: &'static str) -> Self {
        ShedReporter {
            cause,
            reported: 0,
            last: None,
        }
    }

    /// Notice to emit now, if the shed count grew and the throttle allows.
    pub fn poll(&mut self, counts: &DropCounts) -> Option<SttEvent> {
        let dropped = counts.chunks.load(Ordering::Relaxed);
        if dropped <= self.reported {
            return None;
        }
        if self
            .last
            .is_some_and(|t| t.elapsed() < SHED_NOTICE_INTERVAL)
        {
            return None;
        }
        self.reported = dropped;
        self.last = Some(Instant::now());
        Some(SttEvent::Error(format!(
            "{}: {dropped} chunk(s) shed so far{}",
            self.cause,
            counts.lost_finals_note()
        )))
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
fn pop_next(q: &mut VecDeque<AudioChunk>) -> Option<AudioChunk> {
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

/// Shed policy for the bounded queue. Returns `(accepted, kind_evicted)`.
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
) -> (bool, Option<ChunkKind>) {
    if q.len() < cap {
        q.push_back(chunk);
        return (true, None);
    }
    if let Some(idx) = q.iter().position(|c| c.kind == ChunkKind::Interim) {
        q.remove(idx);
        q.push_back(chunk);
        return (true, Some(ChunkKind::Interim));
    }
    if chunk.kind == ChunkKind::Interim {
        return (false, None); // never evict a final for a partial
    }
    q.pop_front();
    q.push_back(chunk);
    (true, Some(ChunkKind::Final))
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
        assert!(accepted);
        assert_eq!(evicted, Some(ChunkKind::Interim));
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
        assert!(!accepted, "incoming partial dropped instead");
        assert_eq!(evicted, None);
        assert!(q.iter().all(|c| c.kind == ChunkKind::Final));
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn shed_reporter_fires_on_first_drop_then_throttles() {
        let counts = DropCounts::default();
        let mut reporter = ShedReporter::new("falling behind");
        // Nothing shed yet: silent.
        assert!(reporter.poll(&counts).is_none());
        // First shed reports immediately.
        counts.chunks.store(3, Ordering::Relaxed);
        let notice = reporter.poll(&counts).expect("first shed reports");
        let SttEvent::Error(msg) = notice else {
            panic!("expected error notice")
        };
        assert!(msg.starts_with("falling behind: 3 chunk(s)"), "{msg}");
        // More sheds inside the throttle window stay quiet.
        counts.chunks.store(9, Ordering::Relaxed);
        assert!(reporter.poll(&counts).is_none(), "throttled");
    }

    #[test]
    fn sustained_final_overload_evicts_oldest_final() {
        let mut q = VecDeque::new();
        for i in 0..3 {
            enqueue_with_shed(&mut q, chunk(ChunkKind::Final, i), 3);
        }
        let (accepted, evicted) = enqueue_with_shed(&mut q, chunk(ChunkKind::Final, 3), 3);
        assert!(accepted);
        assert_eq!(evicted, Some(ChunkKind::Final));
        let seqs: Vec<_> = q.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3], "oldest final evicted");
    }

    #[test]
    fn final_drops_are_counted_apart_from_interim_drops() {
        let shared = SharedQueue::new();
        // One interim among finals: the first overflow evicts it (interim
        // shed), every later one evicts a final (transcript lost).
        shared.push(chunk(ChunkKind::Interim, 0));
        for seq in 1..QUEUE_CAP as u64 {
            shared.push(chunk(ChunkKind::Final, seq));
        }
        shared.push(chunk(ChunkKind::Final, 100));
        assert_eq!(
            shared.dropped.totals(),
            SttDrops {
                chunks: 1,
                finals: 0
            }
        );

        shared.push(chunk(ChunkKind::Final, 101));
        assert_eq!(
            shared.dropped.totals(),
            SttDrops {
                chunks: 2,
                finals: 1
            }
        );

        let Some(SttEvent::Error(msg)) = shared.shed_notice() else {
            panic!("expected a shed notice")
        };
        assert!(msg.starts_with("inference queue shed 2 chunk(s)"), "{msg}");
        assert!(msg.contains("1 final(s) lost"), "{msg}");
    }

    /// Run the worker's side: take everything queued, as if each chunk came
    /// back with no text.
    fn work_through_queue(shared: &SharedQueue) {
        while shared.next_chunk().is_some() {}
    }

    fn queued_seqs(shared: &SharedQueue) -> Vec<u64> {
        let q = shared.queue.lock().unwrap();
        q.iter().map(|c| c.seq).collect()
    }

    #[test]
    fn interims_wait_while_the_worker_is_behind_on_finals() {
        let shared = SharedQueue::new();
        shared.push(chunk(ChunkKind::Final, 0));
        shared.push(chunk(ChunkKind::Interim, 1));
        assert_eq!(queued_seqs(&shared), [0, 1], "one final ahead is fine");

        shared.next_chunk().unwrap(); // in inference, still not done
        shared.push(chunk(ChunkKind::Final, 2));
        shared.push(chunk(ChunkKind::Interim, 3));
        assert_eq!(queued_seqs(&shared), [1, 2], "two ahead: seq 3 skipped");
        assert_eq!(shared.dropped.totals(), SttDrops::default(), "not shed");
    }

    #[test]
    fn finals_that_produced_no_text_do_not_stop_later_interims() {
        let shared = SharedQueue::new();
        shared.push(chunk(ChunkKind::Final, 0));
        shared.push(chunk(ChunkKind::Final, 1));
        work_through_queue(&shared);

        shared.push(chunk(ChunkKind::Interim, 2));
        assert_eq!(queued_seqs(&shared), [2]);
    }

    #[test]
    fn shed_finals_do_not_stop_later_interims() {
        let shared = SharedQueue::new();
        for seq in 0..QUEUE_CAP as u64 + 3 {
            shared.push(chunk(ChunkKind::Final, seq));
        }
        assert_eq!(
            shared.dropped.totals(),
            SttDrops {
                chunks: 3,
                finals: 3
            }
        );
        work_through_queue(&shared);

        shared.push(chunk(ChunkKind::Interim, 99));
        assert_eq!(queued_seqs(&shared), [99]);
    }

    #[test]
    fn interim_only_sheds_do_not_claim_lost_transcript() {
        let shared = SharedQueue::new();
        for seq in 0..=QUEUE_CAP as u64 {
            shared.push(chunk(ChunkKind::Interim, seq));
        }
        let Some(SttEvent::Error(msg)) = shared.shed_notice() else {
            panic!("expected a shed notice")
        };
        assert!(!msg.contains("lost"), "{msg}");
    }
}
