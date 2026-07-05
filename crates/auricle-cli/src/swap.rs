//! Runtime provider swap: a shared cycle target plus per-channel drive
//! state that applies the swap at the next chunk boundary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChunkKind, Config, Result, Segment, SttEvent};
use auricle_stt::{SessionCfg, SttSession};

/// Shared swap target. The hotkey thread advances it; each channel driver
/// notices at its next chunk boundary and switches sessions.
pub struct SwapController {
    ids: Vec<&'static str>,
    target: AtomicUsize,
}

impl SwapController {
    pub fn new(ids: Vec<&'static str>, initial_idx: usize) -> Self {
        SwapController {
            ids,
            target: AtomicUsize::new(initial_idx),
        }
    }

    /// Advance to the next provider in the cycle; returns its id.
    pub fn request_next(&self) -> &'static str {
        let next = (self.target.load(Ordering::Relaxed) + 1) % self.ids.len();
        self.target.store(next, Ordering::Relaxed);
        self.ids[next]
    }

    /// Reset the target (used when a swap fails so drivers stop retrying).
    pub fn force_target(&self, idx: usize) {
        self.target.store(idx, Ordering::Relaxed);
    }

    pub fn target_idx(&self) -> usize {
        self.target.load(Ordering::Relaxed)
    }

    pub fn id_at(&self, idx: usize) -> &'static str {
        self.ids[idx]
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Async session construction, mockable for the swap state-machine tests.
#[async_trait]
pub trait SessionFactory: Send + Sync {
    async fn create(&self, idx: usize) -> Result<(&'static str, Box<dyn SttSession>)>;
}

/// Real factory: constructs providers from config lazily and caches them
/// (whisper-local's model load is expensive; cloud providers are cheap).
pub struct ProviderFactory {
    cfg: Config,
    data_dir: PathBuf,
    ids: Vec<&'static str>,
    session_cfg: SessionCfg,
    cache: tokio::sync::Mutex<HashMap<usize, Arc<dyn auricle_stt::SttProvider>>>,
}

impl ProviderFactory {
    pub fn new(
        cfg: Config,
        data_dir: PathBuf,
        ids: Vec<&'static str>,
        session_cfg: SessionCfg,
    ) -> Self {
        ProviderFactory {
            cfg,
            data_dir,
            ids,
            session_cfg,
            cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SessionFactory for ProviderFactory {
    async fn create(&self, idx: usize) -> Result<(&'static str, Box<dyn SttSession>)> {
        let id = self.ids[idx];
        let provider = {
            let mut cache = self.cache.lock().await;
            match cache.get(&idx) {
                Some(p) => p.clone(),
                None => {
                    let p = auricle_stt::create_provider(id, &self.cfg, &self.data_dir).await?;
                    cache.insert(idx, p.clone());
                    p
                }
            }
        };
        let session = provider.start_session(&self.session_cfg).await?;
        Ok((id, session))
    }
}

/// What a driver reports upward alongside plain STT events.
#[derive(Debug)]
pub enum DriverEvent {
    Stt(SttEvent),
    SwapRequested { to: &'static str },
    Swapped { to: &'static str },
}

/// Per-channel drive state: the live session and the swap bookkeeping.
pub struct DriveState {
    pub current_idx: usize,
    pub provider_id: &'static str,
    pub session: Box<dyn SttSession>,
    /// Finals fed but not yet transcribed (backpressure for interims).
    pub outstanding_finals: i64,
    /// Finals returned by finished (swapped-away) sessions.
    pub collected_finals: Vec<Segment>,
}

impl DriveState {
    pub async fn start(factory: &dyn SessionFactory, idx: usize) -> Result<Self> {
        let (provider_id, session) = factory.create(idx).await?;
        Ok(DriveState {
            current_idx: idx,
            provider_id,
            session,
            outstanding_finals: 0,
            collected_finals: Vec::new(),
        })
    }

    /// Apply a pending provider swap, if any. Called at chunk boundaries:
    /// the old session is finalized (its buffered events drained through
    /// `forward`), then the new provider's session takes over. On failure
    /// the swap target is reset and the old provider keeps the channel.
    pub async fn apply_swap_if_requested<F>(
        &mut self,
        swap: &SwapController,
        factory: &dyn SessionFactory,
        mut forward: F,
    ) -> Result<()>
    where
        F: FnMut(&'static str, SttEvent),
    {
        let target = swap.target_idx();
        if target == self.current_idx {
            return Ok(());
        }

        // Finalize the outgoing session at the boundary and keep its finals.
        let finals = self.session.finish().await?;
        while let Some(ev) = self.session.next_event().await {
            forward(self.provider_id, ev);
        }
        self.collected_finals.extend(finals);

        match factory.create(target).await {
            Ok((id, session)) => {
                self.current_idx = target;
                self.provider_id = id;
                self.session = session;
                self.outstanding_finals = 0;
                Ok(())
            }
            Err(e) => {
                // Stay on the old provider: stop other drivers from retrying
                // the broken target, then restart a session where we were.
                swap.force_target(self.current_idx);
                forward(
                    self.provider_id,
                    SttEvent::Error(format!(
                        "swap to {} failed: {e}; staying on {}",
                        swap.id_at(target),
                        self.provider_id
                    )),
                );
                let (id, session) = factory.create(self.current_idx).await?;
                self.provider_id = id;
                self.session = session;
                self.outstanding_finals = 0;
                Ok(())
            }
        }
    }

    /// Feed one chunk, applying interim backpressure (skip partial windows
    /// while more than one final is awaiting inference).
    pub async fn feed_chunk(&mut self, chunk: AudioChunk) -> Result<()> {
        match chunk.kind {
            ChunkKind::Final => {
                self.outstanding_finals += 1;
                self.session.feed(chunk).await
            }
            ChunkKind::Interim if self.outstanding_finals <= 1 => self.session.feed(chunk).await,
            ChunkKind::Interim => Ok(()), // shed: model is behind
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auricle_core::ChannelId;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// Records which session received which chunk seqs, and how sessions
    /// were opened/finished.
    #[derive(Default)]
    struct MockLog {
        fed: Vec<(usize, u64)>,    // (session #, chunk seq)
        finished: Vec<usize>,      // session #s finished
        opened: Vec<&'static str>, // provider ids opened
    }

    struct MockFactory {
        log: Arc<Mutex<MockLog>>,
        next_session: AtomicUsize,
        ids: Vec<&'static str>,
        fail_idx: Option<usize>,
    }

    struct MockSession {
        n: usize,
        log: Arc<Mutex<MockLog>>,
        // keeps next_event pending-forever until finish
        _tx: mpsc::UnboundedSender<SttEvent>,
        rx: mpsc::UnboundedReceiver<SttEvent>,
    }

    #[async_trait]
    impl SessionFactory for MockFactory {
        async fn create(&self, idx: usize) -> Result<(&'static str, Box<dyn SttSession>)> {
            if Some(idx) == self.fail_idx {
                return Err(auricle_core::Error::Stt("mock create failure".into()));
            }
            let n = self.next_session.fetch_add(1, Ordering::Relaxed);
            self.log.lock().unwrap().opened.push(self.ids[idx]);
            let (tx, rx) = mpsc::unbounded_channel();
            Ok((
                self.ids[idx],
                Box::new(MockSession {
                    n,
                    log: self.log.clone(),
                    _tx: tx,
                    rx,
                }),
            ))
        }
    }

    #[async_trait]
    impl SttSession for MockSession {
        async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
            self.log.lock().unwrap().fed.push((self.n, chunk.seq));
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SttEvent> {
            self.rx.recv().await
        }
        async fn finish(&mut self) -> Result<Vec<Segment>> {
            self.log.lock().unwrap().finished.push(self.n);
            self.rx.close();
            Ok(vec![Segment {
                channel: ChannelId::Mic,
                t_start_ms: 0,
                t_end_ms: 1,
                text: format!("session {}", self.n),
            }])
        }
    }

    fn chunk(seq: u64) -> AudioChunk {
        AudioChunk {
            channel: ChannelId::Mic,
            session_id: "t".into(),
            seq,
            kind: ChunkKind::Final,
            sample_rate_hz: 16_000,
            t_start_ms: seq * 1000,
            t_end_ms: seq * 1000 + 1000,
            samples: vec![0.0; 16],
        }
    }

    fn setup(fail_idx: Option<usize>) -> (Arc<Mutex<MockLog>>, MockFactory, SwapController) {
        let log = Arc::new(Mutex::new(MockLog::default()));
        let ids = vec!["whisper-local", "deepgram", "groq-whisper"];
        let factory = MockFactory {
            log: log.clone(),
            next_session: AtomicUsize::new(0),
            ids: ids.clone(),
            fail_idx,
        };
        (log, factory, SwapController::new(ids, 0))
    }

    #[tokio::test]
    async fn swap_happens_exactly_at_the_chunk_boundary() {
        let (log, factory, swap) = setup(None);
        let mut state = DriveState::start(&factory, 0).await.unwrap();

        for seq in 0..2 {
            state
                .apply_swap_if_requested(&swap, &factory, |_, _| {})
                .await
                .unwrap();
            state.feed_chunk(chunk(seq)).await.unwrap();
        }
        assert_eq!(swap.request_next(), "deepgram");
        for seq in 2..4 {
            state
                .apply_swap_if_requested(&swap, &factory, |_, _| {})
                .await
                .unwrap();
            state.feed_chunk(chunk(seq)).await.unwrap();
        }

        let log = log.lock().unwrap();
        assert_eq!(
            log.fed,
            vec![(0, 0), (0, 1), (1, 2), (1, 3)],
            "chunks 0-1 to session 0, 2-3 to session 1 — no chunk straddles"
        );
        assert_eq!(log.finished, vec![0], "old session finalized on swap");
        assert_eq!(log.opened, vec!["whisper-local", "deepgram"]);
        // Finals of the finished session are preserved for the summary.
        assert_eq!(state.collected_finals.len(), 1);
        assert_eq!(state.collected_finals[0].text, "session 0");
    }

    #[tokio::test]
    async fn cycle_wraps_around() {
        let (_, _, swap) = setup(None);
        assert_eq!(swap.request_next(), "deepgram");
        assert_eq!(swap.request_next(), "groq-whisper");
        assert_eq!(swap.request_next(), "whisper-local");
    }

    #[tokio::test]
    async fn failed_swap_stays_on_current_provider_and_resets_target() {
        let (log, factory, swap) = setup(Some(1)); // deepgram creation fails
        let mut state = DriveState::start(&factory, 0).await.unwrap();
        state.feed_chunk(chunk(0)).await.unwrap();

        swap.request_next(); // -> deepgram (will fail)
        let mut errors = Vec::new();
        state
            .apply_swap_if_requested(&swap, &factory, |_, ev| {
                if let SttEvent::Error(e) = ev {
                    errors.push(e);
                }
            })
            .await
            .unwrap();
        state.feed_chunk(chunk(1)).await.unwrap();

        assert_eq!(swap.target_idx(), 0, "target reset to current");
        assert_eq!(state.provider_id, "whisper-local");
        assert!(errors.iter().any(|e| e.contains("swap to deepgram failed")));
        let log = log.lock().unwrap();
        // Session 0 was finalized, then a fresh whisper-local session opened.
        assert_eq!(log.finished, vec![0]);
        assert_eq!(log.fed, vec![(0, 0), (1, 1)]);
    }

    #[tokio::test]
    async fn no_swap_requested_is_a_noop() {
        let (log, factory, swap) = setup(None);
        let mut state = DriveState::start(&factory, 0).await.unwrap();
        state
            .apply_swap_if_requested(&swap, &factory, |_, _| {})
            .await
            .unwrap();
        assert!(log.lock().unwrap().finished.is_empty());
    }
}
