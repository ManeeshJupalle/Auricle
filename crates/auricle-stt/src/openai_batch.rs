//! Batch-per-chunk providers speaking the OpenAI `audio/transcriptions`
//! shape: `groq-whisper` and `openai-compat` (configurable base_url).
//!
//! Response types derived from real captured payloads in `fixtures/groq/`
//! (Groq is OpenAI-compatible; see docs/PHASE3_PAYLOAD_REPORT.md).

use std::io::Cursor;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChunkKind, Error, Result, Segment, SttEvent};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::queue::{pop_next, SharedQueue, ShedReporter};
use crate::{SessionCfg, SttKind, SttProvider, SttSession};

const MAX_ATTEMPTS: u32 = 4;
const BASE_BACKOFF_MS: u64 = 500;
/// Ceiling on a single 429 wait, even when the server advises longer.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);
/// After any 429, interim uploads are shed for this long so remaining
/// quota goes to finals (the transcript).
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);

// ---------- wire types (derived from fixtures/groq/*.json) ----------

/// Default (`json`) response: `{"text": "...", "x_groq": {...}}` — the
/// undocumented `x_groq` extension is ignored by serde's default behavior.
#[derive(Debug, Deserialize)]
pub(crate) struct BatchTranscription {
    pub text: String,
}

/// `verbose_json` response (unused by the session, typed for fixture
/// coverage and future re-transcription features). Note `language` is a
/// full English word ("English"), not an ISO code.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct VerboseTranscription {
    pub task: String,
    pub language: String,
    pub duration: f64,
    pub text: String,
    pub segments: Vec<VerboseSegment>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct VerboseSegment {
    pub id: u32,
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub avg_logprob: f64,
    pub no_speech_prob: f64,
}

// ---------- provider ----------

pub struct OpenAiBatchProvider {
    id: &'static str,
    api_key: String,
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiBatchProvider {
    /// Read the API key from `api_key_env`. Key lives in memory only.
    pub fn from_env(
        id: &'static str,
        api_key_env: &str,
        base_url: &str,
        model: &str,
    ) -> Result<Self> {
        let api_key = auricle_core::env_lookup(api_key_env)
            .ok_or_else(|| Error::Stt(format!("{id} API key env var {api_key_env} is not set")))?;
        Ok(Self::with_key(id, api_key, base_url, model))
    }

    /// Construct with an explicit key (used by tests with a mock server).
    pub fn with_key(id: &'static str, api_key: String, base_url: &str, model: &str) -> Self {
        OpenAiBatchProvider {
            id,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl SttProvider for OpenAiBatchProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn kind(&self) -> SttKind {
        SttKind::CloudBatch
    }

    async fn start_session(&self, cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
        let shared = Arc::new(SharedQueue::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(worker_loop(
            self.client.clone(),
            format!("{}/audio/transcriptions", self.base_url),
            self.api_key.clone(),
            self.model.clone(),
            cfg.language.clone(),
            shared.clone(),
            tx,
        ));
        Ok(Box::new(BatchSession {
            shared,
            events: rx,
            worker: Some(worker),
        }))
    }
}

struct BatchSession {
    shared: Arc<SharedQueue>,
    events: mpsc::UnboundedReceiver<SttEvent>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl SttSession for BatchSession {
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
                .map_err(|e| Error::Stt(format!("batch worker panicked: {e}")))?;
        }
        Ok(self.shared.finals.lock().unwrap().clone())
    }
}

async fn worker_loop(
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    language: String,
    shared: Arc<SharedQueue>,
    tx: mpsc::UnboundedSender<SttEvent>,
) {
    let mut rate_limited_until: Option<Instant> = None;
    let mut shed = ShedReporter::new();
    loop {
        // Live overload visibility: report shedding as it happens, not
        // only in the end-of-session total.
        if let Some(notice) = shed.poll(&shared) {
            let _ = tx.send(notice);
        }
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
        if shared.closed.load(Ordering::Relaxed) && chunk.kind == ChunkKind::Interim {
            continue;
        }
        // While the provider is rate-limiting us, spend no quota on interim
        // windows — partials are expendable, finals are the transcript.
        if chunk.kind == ChunkKind::Interim
            && rate_limited_until.is_some_and(|t| Instant::now() < t)
        {
            continue;
        }

        let mut saw_429 = false;
        let outcome = transcribe_chunk(
            &client,
            &endpoint,
            &api_key,
            &model,
            &language,
            &chunk,
            &mut saw_429,
        )
        .await;
        if saw_429 {
            rate_limited_until = Some(Instant::now() + RATE_LIMIT_COOLDOWN);
        }
        match outcome {
            Ok(Some(text)) => {
                let seg = Segment {
                    channel: chunk.channel,
                    t_start_ms: chunk.t_start_ms,
                    t_end_ms: chunk.t_end_ms,
                    text,
                };
                let event = match chunk.kind {
                    ChunkKind::Interim => SttEvent::Partial(seg),
                    ChunkKind::Final => {
                        shared.finals.lock().unwrap().push(seg.clone());
                        SttEvent::Final(seg)
                    }
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
            Ok(None) => {} // empty transcription
            Err(e) => {
                let _ = tx.send(SttEvent::Error(e.to_string()));
            }
        }
    }
}

/// Upload one chunk with retry on 429/5xx (other 4xx fail fast). On 429 the
/// wait honors the server's own timing (`retry-after`, or Groq's
/// `x-ratelimit-reset-*` durations) instead of the fixed backoff — the
/// fixed 0.5–3 s ladder retries far inside the rate-limit window and used
/// to burn all attempts, dropping the chunk's text. Sets `saw_429` so the
/// worker can enter its interim-shedding cooldown.
#[allow(clippy::too_many_arguments)]
async fn transcribe_chunk(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    model: &str,
    language: &str,
    chunk: &AudioChunk,
    saw_429: &mut bool,
) -> Result<Option<String>> {
    let wav = encode_wav_16bit(&chunk.samples, chunk.sample_rate_hz)?;

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(wav.clone())
                    .file_name("chunk.wav")
                    .mime_str("audio/wav")
                    .map_err(|e| Error::Stt(format!("building upload: {e}")))?,
            )
            .text("model", model.to_string())
            .text("language", language.to_string());

        let outcome = client
            .post(endpoint)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .await;

        let (retryable_error, server_wait) = match outcome {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let body: BatchTranscription = resp
                        .json()
                        .await
                        .map_err(|e| Error::Stt(format!("parsing transcription: {e}")))?;
                    let text = body.text.trim().to_string();
                    return Ok(if text.is_empty() { None } else { Some(text) });
                }
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let detail = format!("HTTP {status} from {endpoint}");
                if !retryable {
                    return Err(Error::Stt(format!("{detail} (not retryable)")));
                }
                let wait = if status.as_u16() == 429 {
                    *saw_429 = true;
                    rate_limit_wait(resp.headers())
                } else {
                    None
                };
                (detail, wait)
            }
            Err(e) => (format!("request failed: {e}"), None),
        };

        if attempt >= MAX_ATTEMPTS {
            return Err(Error::Stt(format!(
                "{retryable_error} (giving up after {attempt} attempts)"
            )));
        }
        let backoff = backoff_with_jitter(attempt);
        let wait = server_wait.map_or(backoff, |s| s.max(backoff).min(MAX_RATE_LIMIT_WAIT));
        tokio::time::sleep(wait).await;
    }
}

/// Extract the server-advised wait from 429 response headers: standard
/// `retry-after` seconds, or Groq's `x-ratelimit-reset-requests` /
/// `x-ratelimit-reset-audio-seconds` duration strings ("7s", "43.2s",
/// "1m26.4s" — shapes captured in fixtures/groq).
fn rate_limit_wait(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(v) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Ok(secs) = v.trim().parse::<u64>() {
            return Some(Duration::from_secs(secs));
        }
    }
    [
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-audio-seconds",
    ]
    .iter()
    .filter_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()))
    .filter_map(parse_reset_duration)
    .max()
}

/// Parse Groq reset durations: "7s", "43.2s", "1m26.4s", bare seconds.
fn parse_reset_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (mins, rest) = match s.split_once('m') {
        // Guard against "m" inside things like "1ms" (not seen in captures,
        // but milliseconds notation would otherwise parse as minutes).
        Some((m, rest)) if !rest.starts_with('s') => (m.parse::<f64>().ok()?, rest),
        _ => (0.0, s),
    };
    let secs = match rest.strip_suffix('s') {
        Some(n) => n.parse::<f64>().ok()?,
        None if rest.is_empty() => 0.0,
        None => rest.parse::<f64>().ok()?,
    };
    let total = mins * 60.0 + secs;
    if total.is_finite() && total >= 0.0 {
        Some(Duration::from_secs_f64(total))
    } else {
        None
    }
}

/// Exponential backoff with jitter; jitter comes from the clock's
/// sub-millisecond noise so no RNG dependency is needed.
fn backoff_with_jitter(attempt: u32) -> Duration {
    let base = BASE_BACKOFF_MS * 2u64.pow(attempt - 1);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % 250)
        .unwrap_or(125);
    Duration::from_millis(base + jitter)
}

/// Encode mono f32 samples as an in-memory 16-bit PCM WAV.
pub(crate) fn encode_wav_16bit(samples: &[f32], sample_rate_hz: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate_hz,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|e| Error::Stt(format!("wav encode: {e}")))?;
        for s in samples {
            writer
                .write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .map_err(|e| Error::Stt(format!("wav encode: {e}")))?;
        }
        writer
            .finalize()
            .map_err(|e| Error::Stt(format!("wav encode: {e}")))?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_encoding_is_valid_and_16bit() {
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 100.0).sin() * 0.5).collect();
        let bytes = encode_wav_16bit(&samples, 16_000).unwrap();
        let reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(reader.len(), 1600);
    }

    #[test]
    fn reset_duration_parses_the_captured_shapes() {
        assert_eq!(parse_reset_duration("7s"), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_reset_duration("43.2s"),
            Some(Duration::from_secs_f64(43.2))
        );
        assert_eq!(
            parse_reset_duration("1m26.4s"),
            Some(Duration::from_secs_f64(86.4))
        );
        assert_eq!(parse_reset_duration("15"), Some(Duration::from_secs(15)));
        assert_eq!(parse_reset_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_reset_duration("garbage"), None);
        assert_eq!(parse_reset_duration("-3s"), None);
    }

    #[test]
    fn rate_limit_wait_prefers_retry_after_then_reset_headers() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "9".parse().unwrap());
        h.insert("x-ratelimit-reset-requests", "43.2s".parse().unwrap());
        assert_eq!(rate_limit_wait(&h), Some(Duration::from_secs(9)));

        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-ratelimit-reset-requests", "7s".parse().unwrap());
        h.insert(
            "x-ratelimit-reset-audio-seconds",
            "1m26.4s".parse().unwrap(),
        );
        assert_eq!(rate_limit_wait(&h), Some(Duration::from_secs_f64(86.4)));

        assert_eq!(rate_limit_wait(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn backoff_grows_and_stays_bounded() {
        let b1 = backoff_with_jitter(1).as_millis() as u64;
        let b2 = backoff_with_jitter(2).as_millis() as u64;
        let b3 = backoff_with_jitter(3).as_millis() as u64;
        assert!((500..800).contains(&b1), "{b1}");
        assert!((1000..1300).contains(&b2), "{b2}");
        assert!((2000..2300).contains(&b3), "{b3}");
    }
}
