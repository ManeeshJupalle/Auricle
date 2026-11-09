use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, ChunkKind, Error, Result, Segment, SttEvent};
use tokio::sync::mpsc;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperTokenId,
};

use crate::hallucination::{fit_for_prompt, is_hallucination, is_low_energy, is_noise_annotation};
use crate::language::whisper_language;
use crate::queue::{SharedQueue, ShedReporter};
use crate::{SessionCfg, SttDrops, SttKind, SttProvider, SttSession};

/// whisper.cpp skips inputs shorter than ~1 s; pad chunks up to this length.
const MIN_AUDIO_MS: u64 = 1_200;

/// Longest tail of the previous final kept as decoder context: enough to
/// carry names and topic across chunks, short enough not to invite the
/// repetition loops long prompts cause.
const PROMPT_TAIL_CHARS: usize = 200;

/// A pause this long ends the thought the context came from, and old words
/// would only prime the decode to invent more of them.
const PROMPT_MAX_GAP_MS: u64 = 30_000;

/// Local Whisper (whisper.cpp via whisper-rs) provider. The ggml model is
/// loaded once and shared by all sessions.
pub struct WhisperLocalProvider {
    ctx: Arc<WhisperContext>,
    n_threads: i32,
}

impl WhisperLocalProvider {
    /// Load the ggml model at `path`, on the CPU. Blocking (reads 150-500 MB);
    /// call from a blocking context.
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with(path, false)
    }

    /// [`Self::load`], on the GPU if `use_gpu` and the build has a backend.
    pub fn load_with(path: &Path, use_gpu: bool) -> Result<Self> {
        // Silence whisper.cpp/ggml stderr chatter (no log backend features =>
        // the hooks discard output). Without this, model load dumps ~40 lines
        // into the live terminal display.
        static HOOKS: OnceLock<()> = OnceLock::new();
        HOOKS.get_or_init(whisper_rs::install_logging_hooks);

        if use_gpu && !cfg!(feature = "vulkan") {
            eprintln!("warning: use_gpu is set, but this build has no GPU backend; using the CPU");
        }
        let ctx = WhisperContext::new_with_params(path, context_params(use_gpu))
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

/// Set explicitly: a GPU-enabled whisper-rs defaults to the GPU, and the
/// feature alone must never move anyone off the CPU.
fn context_params(use_gpu: bool) -> WhisperContextParameters<'static> {
    let mut params = WhisperContextParameters::default();
    params.use_gpu(use_gpu);
    params
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
        // An English-only model has no language tokens: "auto" would pay for
        // a second encode per chunk to read noise.
        let language = if self.ctx.is_multilingual() {
            whisper_language(&cfg.language)?
        } else {
            "en".to_string()
        };
        Ok(Box::new(WhisperSession::start(
            self.ctx.clone(),
            language,
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
        self.shared.close();
        if let Some(worker) = self.worker.take() {
            worker
                .await
                .map_err(|e| Error::Stt(format!("whisper worker panicked: {e}")))?;
        }
        let finals = self.shared.finals.lock().unwrap().clone();
        Ok(finals)
    }

    fn drops(&self) -> SttDrops {
        self.shared.dropped.totals()
    }
}

/// A session dropped without `finish` (a start that failed after opening it)
/// must not leave its worker parked forever, holding whisper state.
impl Drop for WhisperSession {
    fn drop(&mut self) {
        self.shared.close();
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

    // Decoder context per channel, so one speaker's words never prime the
    // other's. Tokens, not `set_initial_prompt`: that call leaks its CString
    // on every decode, and this tokenizes once per final instead. Kept with
    // the time of the final they came from.
    let mut prompts: HashMap<ChannelId, (Vec<WhisperTokenId>, u64)> = HashMap::new();
    let mut shed = ShedReporter::new("inference is falling behind");
    loop {
        // Live overload visibility: report shedding as it happens, not
        // only in the end-of-session total.
        if let Some(notice) = shed.poll(&shared.dropped) {
            let _ = tx.send(notice);
        }
        let Some(chunk) = shared.next_chunk() else {
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
        let prompt = prompts
            .get(&chunk.channel)
            .filter(|(_, end_ms)| prompt_is_fresh(*end_ms, chunk.t_start_ms))
            .map(|(tokens, _)| tokens.clone())
            .unwrap_or_default();
        // Inference on the blocking pool; the state travels in and out so it
        // can be reused. One in-flight inference per session by construction.
        let result = tokio::task::spawn_blocking(move || {
            let r = transcribe(&mut state, &chunk, &lang, &prompt, n_threads);
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
                        if fit_for_prompt(&segment.text) {
                            // A tail that won't tokenize just means a blind decode.
                            let tail = prompt_tail(&segment.text);
                            let tokens = ctx.tokenize(tail, tail.len() + 1).unwrap_or_default();
                            prompts.insert(segment.channel, (tokens, segment.t_end_ms));
                        } else {
                            prompts.remove(&segment.channel);
                        }
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
    prompt: &[WhisperTokenId],
    n_threads: i32,
) -> Result<Option<Segment>> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some(language));
    params.set_tokens(prompt);
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

    let low_energy = is_low_energy(&chunk.samples, chunk.sample_rate_hz);
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
        if piece.is_empty()
            || is_noise_annotation(piece)
            || is_hallucination(piece, low_energy, Some(seg.no_speech_probability()))
        {
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

fn prompt_is_fresh(prompt_end_ms: u64, chunk_start_ms: u64) -> bool {
    chunk_start_ms.saturating_sub(prompt_end_ms) <= PROMPT_MAX_GAP_MS
}

/// The last [`PROMPT_TAIL_CHARS`] of `text`, starting on a word boundary
/// where the text has any.
fn prompt_tail(text: &str) -> &str {
    let Some((start, _)) = text.char_indices().rev().nth(PROMPT_TAIL_CHARS - 1) else {
        return text;
    };
    let tail = &text[start..];
    tail.split_once(char::is_whitespace)
        .map_or(tail, |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_gpu_is_used_only_when_asked_for() {
        assert!(!context_params(false).use_gpu);
        assert!(context_params(true).use_gpu);
    }

    #[test]
    fn a_long_gap_retires_the_prompt() {
        assert!(prompt_is_fresh(10_000, 10_000 + PROMPT_MAX_GAP_MS));
        assert!(!prompt_is_fresh(10_000, 10_001 + PROMPT_MAX_GAP_MS));
        // Chunks overlap, so the next one can start before the final's end.
        assert!(prompt_is_fresh(10_000, 9_500));
    }

    #[test]
    fn short_final_is_its_own_prompt() {
        assert_eq!(prompt_tail("we ship on Friday"), "we ship on Friday");
        assert_eq!(prompt_tail(""), "");
    }

    #[test]
    fn long_final_keeps_a_bounded_tail_on_a_word_boundary() {
        let text = "word ".repeat(100) + "the end";
        let tail = prompt_tail(&text);
        assert!(tail.chars().count() <= PROMPT_TAIL_CHARS, "{tail}");
        assert!(tail.ends_with("the end"), "{tail}");
        assert!(tail.starts_with("word "), "no split word: {tail}");
    }

    #[test]
    fn tail_never_splits_a_multibyte_char_in_unspaced_text() {
        let text = "会".repeat(500);
        assert_eq!(prompt_tail(&text).chars().count(), PROMPT_TAIL_CHARS);
    }

    #[tokio::test]
    async fn dropping_an_unfinished_session_wakes_its_worker() {
        // Parks exactly as `worker_loop` does; the real loop needs a model
        // file (the batch session's test runs its real loop).
        let shared = Arc::new(SharedQueue::new());
        let parked = shared.clone();
        let worker = tokio::spawn(async move {
            while !parked.closed.load(Ordering::Relaxed) {
                parked.notify.notified().await;
            }
        });
        let finished = worker.abort_handle();
        let (_tx, rx) = mpsc::unbounded_channel();

        drop(WhisperSession {
            shared,
            events: rx,
            worker: Some(worker),
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            while !finished.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker exits after the session is dropped");
    }
}
