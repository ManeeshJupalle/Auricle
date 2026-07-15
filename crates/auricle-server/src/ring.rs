//! Transcript ring (COPILOT_ARCHITECTURE §2.2): an in-memory rolling
//! window of final transcript segments, fed from the engine's existing
//! broadcast channel. Zero DB reads on the ask hot path; works with no
//! active session (the window is simply empty). Never persisted — the
//! ring lives and dies with the daemon process.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::events::WsEvent;

/// One final segment as heard, stamped with wall-clock arrival time (the
/// window is "the last N minutes of real time", not transcript time).
#[derive(Debug, Clone)]
pub struct RingSegment {
    pub speaker: String,
    pub t_start_ms: u64,
    pub text: String,
    received_at: Instant,
}

impl RingSegment {
    #[cfg(test)]
    pub(crate) fn for_tests(
        t_start_ms: u64,
        speaker: &str,
        text: &str,
        received_at: Instant,
    ) -> RingSegment {
        RingSegment {
            speaker: speaker.into(),
            t_start_ms,
            text: text.into(),
            received_at,
        }
    }
}

pub struct TranscriptRing {
    window: Duration,
    inner: Mutex<VecDeque<RingSegment>>,
}

impl TranscriptRing {
    pub fn new(window_min: u64) -> Arc<TranscriptRing> {
        Arc::new(TranscriptRing {
            window: Duration::from_secs(window_min * 60),
            inner: Mutex::new(VecDeque::new()),
        })
    }

    /// Subscribe to the engine's important lane and keep every `final`
    /// within the window. Lagging (>1024 events behind) loses finals for
    /// the ring exactly as it would for any other slow consumer; the loop
    /// keeps going.
    pub fn spawn_feeder(self: &Arc<Self>, mut rx: broadcast::Receiver<WsEvent>) {
        let ring = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(WsEvent::Final {
                        speaker,
                        t_start_ms,
                        text,
                        ..
                    }) => ring.push_at(
                        RingSegment {
                            speaker,
                            t_start_ms,
                            text,
                            received_at: Instant::now(),
                        },
                        Instant::now(),
                    ),
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Segments currently inside the window, oldest first.
    pub fn snapshot(&self) -> Vec<RingSegment> {
        self.snapshot_at(Instant::now())
    }

    fn push_at(&self, seg: RingSegment, now: Instant) {
        let mut inner = self.inner.lock().unwrap();
        inner.push_back(seg);
        Self::prune(&mut inner, self.window, now);
    }

    fn snapshot_at(&self, now: Instant) -> Vec<RingSegment> {
        let mut inner = self.inner.lock().unwrap();
        Self::prune(&mut inner, self.window, now);
        inner.iter().cloned().collect()
    }

    fn prune(inner: &mut VecDeque<RingSegment>, window: Duration, now: Instant) {
        while let Some(front) = inner.front() {
            if now.duration_since(front.received_at) > window {
                inner.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, at: Instant) -> RingSegment {
        RingSegment {
            speaker: "Them".into(),
            t_start_ms: 0,
            text: text.into(),
            received_at: at,
        }
    }

    #[test]
    fn window_keeps_only_the_last_n_minutes() {
        let ring = TranscriptRing::new(10);
        let t0 = Instant::now();
        ring.push_at(seg("first", t0), t0);
        ring.push_at(
            seg("mid", t0 + Duration::from_secs(300)),
            t0 + Duration::from_secs(300),
        );
        ring.push_at(
            seg("late", t0 + Duration::from_secs(660)),
            t0 + Duration::from_secs(660),
        );

        // At t0+11min: "first" (11 min old) is out; "mid" (6 min) and
        // "late" (30 s) remain, oldest first.
        let texts: Vec<_> = ring
            .snapshot_at(t0 + Duration::from_secs(660))
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert_eq!(texts, ["mid", "late"]);

        // 10 minutes later still: everything has aged out.
        assert!(ring.snapshot_at(t0 + Duration::from_secs(1_500)).is_empty());
    }

    #[test]
    fn boundary_segment_exactly_at_window_edge_survives() {
        let ring = TranscriptRing::new(10);
        let t0 = Instant::now();
        ring.push_at(seg("edge", t0), t0);
        // Exactly window-old is kept (strictly-older evicts).
        assert_eq!(ring.snapshot_at(t0 + Duration::from_secs(600)).len(), 1);
        assert!(ring.snapshot_at(t0 + Duration::from_secs(601)).is_empty());
    }

    #[test]
    fn empty_ring_is_a_valid_empty_window() {
        let ring = TranscriptRing::new(10);
        assert!(ring.snapshot().is_empty());
    }

    #[tokio::test]
    async fn feeder_captures_finals_and_ignores_everything_else() {
        let (tx, rx) = broadcast::channel(16);
        let ring = TranscriptRing::new(10);
        ring.spawn_feeder(rx);

        tx.send(WsEvent::SessionStarted {
            session: "s1".into(),
            title: "t".into(),
            stt_provider: "p".into(),
        })
        .unwrap();
        tx.send(WsEvent::Partial {
            session: "s1".into(),
            channel: "mic".into(),
            speaker: "You".into(),
            t_start_ms: 0,
            t_end_ms: 500,
            text: "partia".into(),
        })
        .unwrap();
        tx.send(WsEvent::Final {
            session: "s1".into(),
            channel: "mic".into(),
            speaker: "You".into(),
            t_start_ms: 0,
            t_end_ms: 900,
            text: "hello there".into(),
            latency_ms: 120,
        })
        .unwrap();

        // The feeder runs on the runtime; give it a beat.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 1, "only the final lands in the ring");
        assert_eq!(snap[0].text, "hello there");
        assert_eq!(snap[0].speaker, "You");
    }
}
