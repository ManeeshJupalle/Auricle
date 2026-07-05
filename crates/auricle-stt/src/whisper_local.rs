use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChunkKind, Error, Result, Segment, SttEvent};
use tokio::sync::mpsc;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::queue::{pop_next, SharedQueue};
use crate::{SessionCfg, SttKind, SttProvider, SttSession};

/// whisper.cpp skips inputs shorter than ~1 s; pad chunks up to this length.
const MIN_AUDIO_MS: u64 = 1_200;

/// Local Whisper (whisper.cpp via whisper-rs) provider. The ggml model is
/// loaded once and shared by all sessions.
pub struct WhisperLocalProvider {
    ctx: Arc<WhisperContext>,
    n_threads: i32,
}

impl WhisperLocalProvider {
    /// Load the ggml model at `path`. Blocking (reads 150-500 MB); call from
    /// a blocking context.
    pub fn load(path: &Path) -> Result<Self> {
        // Silence whisper.cpp/ggml stderr chatter (no log backend features =>
        // the hooks discard output). Without this, model load dumps ~40 lines
        // into the live terminal display.
        static HOOKS: OnceLock<()> = OnceLock::new();
        HOOKS.get_or_init(whisper_rs::install_logging_hooks);

        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|e| Error::Stt(format!("loading model {}: {e}", path.display())))?;
        let n_threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(2, 8))
            .unwrap_or(4) as i32;
        Ok(WhisperLocalProvider {
            ctx: Arc::new(ctx),
            n_threads,
        })
    }
}

#[async_trait]
impl SttProvider for WhisperLocalProvider {
    fn id(&self) -> &'static str {
        "whisper-local"
    }

    fn kind(&self) -> SttKind {
        SttKind::Local
    }

    async fn start_session(&self, cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
        Ok(Box::new(WhisperSession::start(
            self.ctx.clone(),
            cfg.language.clone(),
            self.n_threads,
        )))
    }
}

struct WhisperSession {
    shared: Arc<SharedQueue>,
    events: mpsc::UnboundedReceiver<SttEvent>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl WhisperSession {
    fn start(ctx: Arc<WhisperContext>, language: String, n_threads: i32) -> Self {
        let shared = Arc::new(SharedQueue::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(worker_loop(ctx, shared.clone(), tx, language, n_threads));
        WhisperSession {
            shared,
            events: rx,
            worker: Some(worker),
        }
    }
}

#[async_trait]
impl SttSession for WhisperSession {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
        if self.shared.closed.load(Ordering::Relaxed) {
            return Err(Error::Stt("session is finished".to_string()));
        }
        self.shared.push(chunk);
        Ok(())
    }

    async fn next_event(&mut self) -> Option<SttEvent> {
        self.events.recv().await
    }

    async fn finish(&mut self) -> Result<Vec<Segment>> {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.notify.notify_one();
        if let Some(worker) = self.worker.take() {
            worker
                .await
                .map_err(|e| Error::Stt(format!("whisper worker panicked: {e}")))?;
        }
        let finals = self.shared.finals.lock().unwrap().clone();
        Ok(finals)
    }
}

async fn worker_loop(
    ctx: Arc<WhisperContext>,
    shared: Arc<SharedQueue>,
    tx: mpsc::UnboundedSender<SttEvent>,
    language: String,
    n_threads: i32,
) {
    // One reusable state per session; WhisperState is Send + Sync and owns
    // its context reference internally.
    let mut state = match ctx.create_state() {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(SttEvent::Error(format!("creating whisper state: {e}")));
            return;
        }
    };

    loop {
        let chunk = pop_next(&mut shared.queue.lock().unwrap());
        let Some(chunk) = chunk else {
            if shared.closed.load(Ordering::Relaxed) {
                if let Some(notice) = shared.shed_notice() {
                    let _ = tx.send(notice);
                }
                break;
            }
            shared.notify.notified().await;
            continue;
        };

        // Once the session is finishing there is no live display left to
        // update; only queued finals are worth transcribing.
        if shared.closed.load(Ordering::Relaxed) && chunk.kind == ChunkKind::Interim {
            continue;
        }

        let lang = language.clone();
        // Inference on the blocking pool; the state travels in and out so it
        // can be reused. One in-flight inference per session by construction.
        let result = tokio::task::spawn_blocking(move || {
            let r = transcribe(&mut state, &chunk, &lang, n_threads);
            (state, chunk, r)
        })
        .await;

        let (returned_state, chunk, outcome) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(SttEvent::Error(format!("inference task failed: {e}")));
                return;
            }
        };
        state = returned_state;

        match outcome {
            Ok(Some(segment)) => {
                let event = match chunk.kind {
                    ChunkKind::Interim => SttEvent::Partial(segment),
                    ChunkKind::Final => {
                        shared.finals.lock().unwrap().push(segment.clone());
                        SttEvent::Final(segment)
                    }
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
            Ok(None) => {} // nothing but silence/noise annotations
            Err(e) => {
                let _ = tx.send(SttEvent::Error(e.to_string()));
            }
        }
    }
}

fn transcribe(
    state: &mut whisper_rs::WhisperState,
    chunk: &AudioChunk,
    language: &str,
    n_threads: i32,
) -> Result<Option<Segment>> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some(language));
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_context(true);
    params.set_suppress_blank(true);
    params.set_n_threads(n_threads);

    // whisper.cpp skips audio shorter than ~1 s; pad with trailing silence.
    let min_samples = (chunk.sample_rate_hz as u64 * MIN_AUDIO_MS / 1000) as usize;
    let padded;
    let samples: &[f32] = if chunk.samples.len() < min_samples {
        padded = {
            let mut v = chunk.samples.clone();
            v.resize(min_samples, 0.0);
            v
        };
        &padded
    } else {
        &chunk.samples
    };

    // Whisper pads every input to a 30 s window, so a full encode costs the
    // same for a 2 s chunk as for 30 s. Restrict the encoder context to the
    // frames the chunk actually fills (one ctx step = 320 samples at 16 kHz)
    // so per-call cost scales with chunk length — the whisper.cpp streaming
    // technique. Measured on the dev machine this cuts a 5 s chunk from
    // ~constant-30s cost to roughly a fifth of it.
    let audio_ctx = (samples.len() / 320 + 128).clamp(192, 1500) as i32;
    params.set_audio_ctx(audio_ctx);

    state
        .full(params, samples)
        .map_err(|e| Error::Stt(format!("whisper inference: {e}")))?;

    let mut text = String::new();
    let mut prev_norm = String::new();
    for i in 0..state.full_n_segments() {
        let Some(seg) = state.get_segment(i) else {
            continue;
        };
        let piece = match seg.to_str_lossy() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let piece = piece.trim();
        if piece.is_empty() || is_noise_annotation(piece) {
            continue;
        }
        // Greedy decoding sometimes emits the same sentence twice in a row
        // (whisper repetition artifact); keep only the first occurrence.
        let norm: String = piece
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        if !norm.is_empty() && norm == prev_norm {
            continue;
        }
        prev_norm = norm;
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(piece);
    }

    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(Segment {
        channel: chunk.channel,
        t_start_ms: chunk.t_start_ms,
        t_end_ms: chunk.t_end_ms,
        text,
    }))
}

/// Whisper emits bracketed pseudo-events for non-speech audio, e.g.
/// "[BLANK_AUDIO]", "(soft music)", "♪". Filter pieces that contain no
/// alphanumeric content outside such annotations.
fn is_noise_annotation(s: &str) -> bool {
    let wrapped = (s.starts_with('[') && s.ends_with(']'))
        || (s.starts_with('(') && s.ends_with(')'))
        || (s.starts_with('*') && s.ends_with('*'));
    wrapped || !s.chars().any(|c| c.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_annotations_are_filtered() {
        assert!(is_noise_annotation("[BLANK_AUDIO]"));
        assert!(is_noise_annotation("(soft music)"));
        assert!(is_noise_annotation("♪"));
        assert!(is_noise_annotation("*sigh*"));
        assert!(!is_noise_annotation("hello world"));
        assert!(!is_noise_annotation("it works (mostly) fine"));
    }
}
