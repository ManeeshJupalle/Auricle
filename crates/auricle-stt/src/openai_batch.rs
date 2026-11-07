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

use crate::hallucination::{is_hallucination, is_low_energy, is_noise_annotation};
use crate::language::batch_language;
use crate::queue::{SharedQueue, ShedReporter};
use crate::repetition::collapse_repetition;
use crate::{http_error, SessionCfg, SttDrops, SttKind, SttProvider, SttSession};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds the upload plus the wait for headers, then each body read. A chunk
/// is at most ~5 s of audio, so a healthy provider answers in seconds; any
/// longer and the worker should retry rather than sit on a dead socket while
/// finals shed behind it.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The same for a server on this machine, which may load its model on the
/// first request.
const LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: u32 = 4;
const BASE_BACKOFF_MS: u64 = 500;
/// Ceiling on a single 429 wait, even when the server advises longer.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);
/// After any 429, interim uploads are shed for this long so remaining
/// quota goes to finals (the transcript).
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);

// ---------- wire types (derived from fixtures/groq/*.json) ----------

/// `{"text": "...", "x_groq": {...}}`, plus `segments` when `verbose_json`
/// was honored. The undocumented `x_groq` extension is ignored by serde's
/// default behavior; `segments` stays optional because compatible servers
/// may answer plain `json` regardless, or send it as null.
#[derive(Debug, Deserialize)]
pub(crate) struct BatchTranscription {
    pub text: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub segments: Vec<BatchSegment>,
}

fn null_as_empty<'de, D>(deserializer: D) -> std::result::Result<Vec<BatchSegment>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchSegment {
    pub text: String,
    #[serde(default)]
    pub no_speech_prob: Option<f32>,
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
    /// Read the API key named `api_key_env` from the environment or the OS
    /// credential store. Key lives in memory only.
    pub fn from_env(
        id: &'static str,
        api_key_env: &str,
        base_url: &str,
        model: &str,
    ) -> Result<Self> {
        let api_key = auricle_core::secret_lookup(api_key_env).ok_or_else(|| {
            Error::Stt(format!(
                "{id} API key {api_key_env} is not set (environment or credential store)"
            ))
        })?;
        Ok(Self::with_key(id, api_key, base_url, model))
    }

    /// Construct with an explicit key (used by tests with a mock server).
    pub fn with_key(id: &'static str, api_key: String, base_url: &str, model: &str) -> Self {
        OpenAiBatchProvider {
            id,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client: http_client(read_timeout(base_url)),
        }
    }
}

fn read_timeout(base_url: &str) -> Duration {
    if auricle_core::is_loopback_url(base_url) {
        LOCAL_READ_TIMEOUT
    } else {
        READ_TIMEOUT
    }
}

fn http_client(read_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
        .build()
        .expect("building http client")
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
            batch_language(&cfg.language),
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
        self.shared.close();
        if let Some(worker) = self.worker.take() {
            worker
                .await
                .map_err(|e| Error::Stt(format!("batch worker panicked: {e}")))?;
        }
        Ok(self.shared.finals.lock().unwrap().clone())
    }

    fn drops(&self) -> SttDrops {
        self.shared.dropped.totals()
    }
}

/// A session dropped without `finish` (a start that failed after opening it)
/// must not leave its worker parked forever.
impl Drop for BatchSession {
    fn drop(&mut self) {
        self.shared.close();
    }
}

async fn worker_loop(
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    language: Option<String>,
    shared: Arc<SharedQueue>,
    tx: mpsc::UnboundedSender<SttEvent>,
) {
    let mut rate_limited_until: Option<Instant> = None;
    let mut verbose = true;
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
            language.as_deref(),
            &chunk,
            &mut verbose,
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
                        let seg = Segment {
                            text: collapse_repetition(&seg.text),
                            ..seg
                        };
                        shared.finals.lock().unwrap().push(seg.clone());
                        SttEvent::Final(seg)
                    }
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
            Ok(None) => {} // empty, non-speech, or an interim given up on
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
///
/// Only finals are retried. Waiting out a 429 for an interim would hold the
/// worker for a partial that is stale by then while finals shed behind it.
///
/// Asks for `verbose_json` to get `no_speech_prob`, and clears `verbose` for
/// the session if the model refuses that format.
#[allow(clippy::too_many_arguments)]
async fn transcribe_chunk(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    model: &str,
    language: Option<&str>,
    chunk: &AudioChunk,
    verbose: &mut bool,
    saw_429: &mut bool,
) -> Result<Option<String>> {
    let wav = encode_wav_16bit(&chunk.samples, chunk.sample_rate_hz)?;
    let low_energy = is_low_energy(&chunk.samples, chunk.sample_rate_hz);

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let mut form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(wav.clone())
                    .file_name("chunk.wav")
                    .mime_str("audio/wav")
                    .map_err(|e| Error::Stt(format!("building upload: {e}")))?,
            )
            .text("model", model.to_string());
        if let Some(language) = language {
            form = form.text("language", language.to_string());
        }
        if *verbose {
            form = form.text("response_format", "verbose_json");
        }

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
                    let body: BatchTranscription = resp.json().await.map_err(|e| {
                        Error::Stt(format!("parsing transcription: {}", http_error(&e)))
                    })?;
                    return Ok(spoken_text(body, low_energy));
                }
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let detail = format!("HTTP {status} from {endpoint}");
                if !retryable {
                    // gpt-4o-transcribe and friends accept only plain `json`.
                    let refused_format = *verbose
                        && resp.text().await.is_ok_and(|b| {
                            b.contains("response_format") || b.contains("verbose_json")
                        });
                    if refused_format {
                        *verbose = false;
                        continue;
                    }
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
            Err(e) => (format!("request failed: {}", http_error(&e)), None),
        };

        if chunk.kind == ChunkKind::Interim {
            return Ok(None);
        }
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

/// The transcript text worth keeping, if any. Per-segment when the provider
/// sent segments, so a stock phrase tacked onto real speech goes without
/// taking the speech with it.
fn spoken_text(body: BatchTranscription, low_energy: bool) -> Option<String> {
    if body.segments.is_empty() {
        return is_spoken(&body.text, low_energy, None).then(|| body.text.trim().to_string());
    }
    let kept: Vec<&str> = body
        .segments
        .iter()
        .filter(|s| is_spoken(&s.text, low_energy, s.no_speech_prob))
        .map(|s| s.text.trim())
        .collect();
    if kept.is_empty() {
        None
    } else if kept.len() == body.segments.len() {
        Some(body.text.trim().to_string())
    } else {
        Some(kept.join(" "))
    }
}

fn is_spoken(text: &str, low_energy: bool, no_speech_prob: Option<f32>) -> bool {
    let text = text.trim();
    !text.is_empty()
        && !is_noise_annotation(text)
        && !is_hallucination(text, low_energy, no_speech_prob)
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
    use auricle_core::ChannelId;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn final_chunk() -> AudioChunk {
        AudioChunk {
            channel: ChannelId::Loopback,
            session_id: "t".to_string(),
            seq: 0,
            kind: ChunkKind::Final,
            sample_rate_hz: 16_000,
            t_start_ms: 1000,
            t_end_ms: 2000,
            samples: vec![0.1; 16_000],
        }
    }

    fn body(text: &str, segments: &[(&str, Option<f32>)]) -> BatchTranscription {
        BatchTranscription {
            text: text.to_string(),
            segments: segments
                .iter()
                .map(|(text, no_speech_prob)| BatchSegment {
                    text: text.to_string(),
                    no_speech_prob: *no_speech_prob,
                })
                .collect(),
        }
    }

    /// First request hangs far past the client's read timeout; the retry
    /// gets a real answer.
    struct StallsOnce(std::sync::atomic::AtomicU32);

    impl Respond for StallsOnce {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let ok = ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "hello"}));
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                ok.set_delay(Duration::from_secs(60))
            } else {
                ok
            }
        }
    }

    #[tokio::test]
    async fn stalled_upload_times_out_and_the_retry_keeps_the_final() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(StallsOnce(Default::default()))
            .expect(2)
            .mount(&server)
            .await;

        let mut provider =
            OpenAiBatchProvider::with_key("groq-whisper", "k".to_string(), &server.uri(), "m");
        provider.client = http_client(Duration::from_secs(1));
        let mut session = provider
            .start_session(&SessionCfg {
                session_id: "t".to_string(),
                language: "en".to_string(),
            })
            .await
            .unwrap();

        session.feed(final_chunk()).await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), session.next_event())
            .await
            .expect("a stalled upload must not block the worker");
        match event {
            Some(SttEvent::Final(seg)) => assert_eq!(seg.text, "hello"),
            other => panic!("expected the retried final, got {other:?}"),
        }
        session.finish().await.unwrap();
    }

    /// Plays `first` for the opening `n` requests, then answers with text.
    struct FirstThenText {
        seen: std::sync::atomic::AtomicU32,
        n: u32,
        first: ResponseTemplate,
    }

    impl Respond for FirstThenText {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.seen.fetch_add(1, Ordering::SeqCst) < self.n {
                self.first.clone()
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "listening"}))
            }
        }
    }

    async fn session_on(server: &MockServer, language: &str) -> Box<dyn SttSession> {
        OpenAiBatchProvider::with_key("groq-whisper", "k".to_string(), &server.uri(), "m")
            .start_session(&SessionCfg {
                session_id: "t".to_string(),
                language: language.to_string(),
            })
            .await
            .unwrap()
    }

    /// Two finals answered with `first`, then partials must come through. An
    /// interim offered while the worker is still on those is rightly skipped,
    /// so keep offering until one lands or the deadline says they stopped.
    async fn assert_interims_resume_after_finals_answered_with(first: ResponseTemplate) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(FirstThenText {
                seen: Default::default(),
                n: 2,
                first,
            })
            .mount(&server)
            .await;
        let mut session = session_on(&server, "en").await;
        session.feed(final_chunk()).await.unwrap();
        session.feed(final_chunk()).await.unwrap();

        let partial = async {
            loop {
                let interim = AudioChunk {
                    kind: ChunkKind::Interim,
                    ..final_chunk()
                };
                session.feed(interim).await.unwrap();
                let wait = Duration::from_millis(50);
                match tokio::time::timeout(wait, session.next_event()).await {
                    Ok(Some(SttEvent::Partial(seg))) => return seg,
                    Ok(None) => panic!("session ended"),
                    _ => {}
                }
            }
        };
        let seg = tokio::time::timeout(Duration::from_secs(10), partial)
            .await
            .expect("interims stopped for good");
        assert_eq!(seg.text, "listening");
        session.finish().await.unwrap();
    }

    #[tokio::test]
    async fn interims_resume_after_finals_that_came_back_empty() {
        let empty = ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": ""}));
        assert_interims_resume_after_finals_answered_with(empty).await;
    }

    #[tokio::test]
    async fn interims_resume_after_finals_that_failed() {
        assert_interims_resume_after_finals_answered_with(ResponseTemplate::new(400)).await;
    }

    #[tokio::test]
    async fn a_final_stuck_in_a_loop_keeps_one_copy() {
        let server = MockServer::start().await;
        let looped = format!("We ship Friday. {}", "Thanks for watching. ".repeat(10));
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": looped})),
            )
            .mount(&server)
            .await;
        let mut session = session_on(&server, "en").await;
        session.feed(final_chunk()).await.unwrap();

        let finals = session.finish().await.unwrap();
        assert_eq!(finals[0].text, "We ship Friday. Thanks for watching.");
    }

    #[tokio::test]
    async fn the_session_reports_what_its_queue_shed() {
        let server = MockServer::start().await;
        let mut session = session_on(&server, "en").await;
        // No answer is coming fast enough for these to clear the queue.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        for _ in 0..crate::queue::QUEUE_CAP + 3 {
            session.feed(final_chunk()).await.unwrap();
        }
        let drops = session.drops();
        assert!(drops.finals >= 1, "{drops:?}");
        assert_eq!(drops.chunks, drops.finals, "only finals were fed");
    }

    /// The `language` field of the one upload `language` produces, if any.
    async fn language_field_for(language: &str) -> Option<String> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "x"})),
            )
            .mount(&server)
            .await;
        let mut session = session_on(&server, language).await;
        session.feed(final_chunk()).await.unwrap();
        session.finish().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        let field = body.split("name=\"language\"").nth(1)?;
        Some(field.split("--").next()?.trim().to_string())
    }

    #[tokio::test]
    async fn the_upload_names_a_bare_language_or_leaves_it_to_the_provider() {
        assert_eq!(language_field_for("de").await.as_deref(), Some("de"));
        assert_eq!(language_field_for("pt-BR").await.as_deref(), Some("pt"));
        assert_eq!(language_field_for("auto").await, None);
    }

    #[tokio::test]
    async fn a_timeout_says_so_in_the_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        let client = http_client(Duration::from_millis(100));
        let error = client.post(server.uri()).send().await.unwrap_err();

        let message = http_error(&error);
        assert!(message.contains("timed out"), "{message}");
        assert!(!message.contains("error sending request"), "{message}");
    }

    #[tokio::test]
    async fn dropping_an_unfinished_session_ends_its_worker() {
        let shared = Arc::new(SharedQueue::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(worker_loop(
            reqwest::Client::new(),
            "http://127.0.0.1:9/unused".to_string(),
            "k".to_string(),
            "m".to_string(),
            Some("en".to_string()),
            shared.clone(),
            tx,
        ));
        let finished = worker.abort_handle();
        // Let the worker reach its parked state before the session goes.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!finished.is_finished(), "worker parks while idle");

        drop(BatchSession {
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

    #[test]
    fn stock_phrase_needs_evidence_and_only_that_segment_goes() {
        let text = " We ship Friday. Thank you.";
        let segments = [(" We ship Friday.", Some(0.0)), (" Thank you.", Some(0.9))];
        assert_eq!(
            spoken_text(body(text, &segments), false).as_deref(),
            Some("We ship Friday.")
        );

        let segments = [(" We ship Friday.", Some(0.0)), (" Thank you.", Some(0.1))];
        assert_eq!(
            spoken_text(body(text, &segments), false).as_deref(),
            Some("We ship Friday. Thank you."),
            "said out loud at normal energy: kept"
        );
    }

    #[test]
    fn plain_json_falls_back_to_the_energy_gate() {
        assert_eq!(spoken_text(body(" Thank you.", &[]), true), None);
        assert_eq!(
            spoken_text(body(" Thank you.", &[]), false).as_deref(),
            Some("Thank you.")
        );
    }

    #[test]
    fn non_speech_annotations_are_filtered_like_the_local_provider() {
        assert_eq!(spoken_text(body(" [BLANK_AUDIO]", &[]), false), None);
        assert_eq!(
            spoken_text(body("[Music]", &[("[Music]", Some(0.0))]), false),
            None
        );
        assert_eq!(spoken_text(body("  ", &[]), false), None);
    }

    #[test]
    fn captured_verbose_fixture_keeps_its_full_text() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/groq/transcription_verbose.json");
        let raw = std::fs::read_to_string(path).unwrap();
        let parsed: BatchTranscription = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.segments.len(), 4);
        assert_eq!(parsed.segments[0].no_speech_prob, Some(0.0));
        let expected = parsed.text.trim().to_string();
        assert_eq!(spoken_text(parsed, false), Some(expected));
    }

    #[test]
    fn a_server_on_this_machine_gets_time_to_load_its_model() {
        assert_eq!(read_timeout("http://localhost:8080/v1"), LOCAL_READ_TIMEOUT);
        assert_eq!(read_timeout("https://api.groq.com/openai/v1"), READ_TIMEOUT);
    }

    #[test]
    fn null_segments_read_as_none() {
        let parsed: BatchTranscription =
            serde_json::from_str(r#"{"text": " hello", "segments": null}"#).unwrap();
        assert!(parsed.segments.is_empty());
        assert_eq!(spoken_text(parsed, false).as_deref(), Some("hello"));
    }

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
