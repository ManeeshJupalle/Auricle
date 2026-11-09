//! Deepgram streaming provider (nova-3 over WebSocket).
//!
//! Shapes and behaviors derived from real captured payloads in
//! `fixtures/deepgram/` (see docs/PHASE3_PAYLOAD_REPORT.md):
//!
//! - Result `start`/`duration` are in **cumulative-fed-audio seconds**, not
//!   wall time (verified across a 15 s KeepAlive-bridged gap). A fed-range
//!   map translates them back to session timestamps.
//! - The server hard-closes after ~12 s without audio or text
//!   (`net0001`); a KeepAlive text message every 5 s bridges VAD gaps.
//! - `{"type":"Finalize"}` forces utterance finalization; sent at span
//!   seams so speech from different spans is never merged across the
//!   silence that was gated out, and once audio stops, because the server
//!   otherwise sits on a short utterance with no frame at all. Its docs don't
//!   promise an answer when there is very little audio.
//! - Empty-transcript Results frames are real and frequent; skipped.
//!
//! A send only proves the audio reached the local socket buffer, so fed
//! audio stays in a [`ReplayBuffer`] until a final covers it. After a
//! disconnect it is replayed on the new connection, where the fresh fed map
//! puts its results back at their original session times.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, DeepgramConfig, Error, Result, Segment, SttEvent};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::language::{deepgram_language, validate_language};
use crate::queue::{enqueue_with_shed, DropCounts, ShedReporter};
use crate::{SessionCfg, SttDrops, SttKind, SttProvider, SttSession};

const DEEPGRAM_URL: &str = "wss://api.deepgram.com";
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Session-time discontinuity treated as a span seam (mirrors the
/// pipeline's gap threshold).
const SEAM_THRESHOLD_MS: u64 = 250;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Fed audio kept for replay until a final covers it.
const REPLAY_WINDOW_MS: u64 = 10_000;
/// Chunks held for a reconnect, per channel. Interims go first and cost
/// nothing, since the next final covers their audio, so a long outage settles
/// at this many finals: about 5 minutes of speech, 20 MB.
const PENDING_CAP: usize = 60;

const FINALIZE: &str = r#"{"type":"Finalize"}"#;
const CLOSE_STREAM: &str = r#"{"type":"CloseStream"}"#;

// Shortened under test so the reconnect paths run quickly against a local
// socket, but left long enough that a busy machine can't trip them.
const INITIAL_BACKOFF: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(1)
};
const CONNECT_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(10)
};
/// Nothing back this long after audio means the connection is dead, even
/// though sends still succeed. A Finalize goes out well inside it, so audio
/// that stopped has been asked for before this can run out. Kept under
/// [`REPLAY_WINDOW_MS`] so all of that audio can still be replayed.
const NO_FRAME_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(8)
};
/// How long after the last audio to ask the server for what it holds. Longer
/// than the ~1 s between chunks of a span still in progress, so an utterance
/// isn't cut in two, and well under [`NO_FRAME_TIMEOUT`].
const FINALIZE_AFTER_AUDIO: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_millis(1500)
};

// ---------- wire types (derived from fixtures/deepgram/*.json) ----------

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum DgFrame {
    Results(DgResults),
    // Payload typed for documentation/fixture coverage; content unused.
    Metadata(#[allow(dead_code)] DgMetadata),
}

#[derive(Debug, Deserialize)]
pub(crate) struct DgResults {
    pub start: f64,
    pub duration: f64,
    pub is_final: bool,
    #[allow(dead_code)] // utterance-end marker; PHASE-4 may surface it
    pub speech_final: bool,
    pub channel: DgChannel,
    #[serde(default)]
    #[allow(dead_code)]
    pub from_finalize: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DgChannel {
    pub alternatives: Vec<DgAlternative>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DgAlternative {
    pub transcript: String,
    #[allow(dead_code)]
    pub confidence: f32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DgMetadata {
    #[allow(dead_code)]
    pub request_id: String,
    /// "incomplete" when the server killed the session (net0001 capture).
    #[serde(default)]
    #[allow(dead_code)]
    pub sha256: Option<String>,
}

// ---------- fed-audio -> session-time mapping ----------

/// Maps Deepgram's fed-audio clock back to session time. One entry per fed
/// chunk; entries are contiguous in fed-time but session time jumps across
/// VAD gaps.
#[derive(Debug, Default)]
pub(crate) struct FedMap {
    entries: Vec<FedRange>,
    /// Cumulative audio seconds sent on the CURRENT connection.
    pub fed_seconds: f64,
    /// Session end of the last fed chunk (survives reconnects; used to trim
    /// chunker overlap).
    pub last_session_end_ms: u64,
    channel: Option<ChannelId>,
}

#[derive(Debug)]
struct FedRange {
    fed_start_s: f64,
    fed_end_s: f64,
    session_start_ms: u64,
}

impl FedMap {
    /// Reset the per-connection state (fed clock restarts at 0 after a
    /// reconnect); the overlap watermark survives.
    fn reset_connection(&mut self) {
        self.entries.clear();
        self.fed_seconds = 0.0;
    }

    fn record(&mut self, session_start_ms: u64, seconds: f64, channel: ChannelId) {
        self.entries.push(FedRange {
            fed_start_s: self.fed_seconds,
            fed_end_s: self.fed_seconds + seconds,
            session_start_ms,
        });
        self.fed_seconds += seconds;
        self.channel = Some(channel);
    }

    /// Translate a fed-audio time to session milliseconds.
    ///
    /// A fed time exactly on a range boundary is ambiguous: it is both the
    /// end of one span and the start of the next (which may be far apart in
    /// session time because of the gated gap). The edge hint resolves it:
    /// a result's start maps into the later range, its end into the earlier.
    pub(crate) fn to_session_ms(&self, fed_t: f64, edge: Edge) -> Option<u64> {
        for e in self.entries.iter().rev() {
            let hit = match edge {
                Edge::Start => fed_t >= e.fed_start_s - 1e-6 && fed_t < e.fed_end_s - 1e-6,
                Edge::End => fed_t > e.fed_start_s + 1e-6 && fed_t <= e.fed_end_s + 1e-6,
            };
            if hit {
                return Some(
                    e.session_start_ms + ((fed_t - e.fed_start_s).max(0.0) * 1000.0) as u64,
                );
            }
        }
        // Clamp anything outside all ranges (finalize padding, boundary
        // rounding) into the last range.
        self.entries.last().map(|e| {
            let clamped = fed_t.clamp(e.fed_start_s, e.fed_end_s);
            e.session_start_ms + ((clamped - e.fed_start_s) * 1000.0) as u64
        })
    }

    fn channel(&self) -> ChannelId {
        self.channel.unwrap_or(ChannelId::Loopback)
    }
}

/// Which edge of a result window a fed time represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edge {
    Start,
    End,
}

// ---------- replay of fed-but-unfinalized audio ----------

/// One span of audio already sent, as linear16.
struct FedAudio {
    pcm: Vec<u8>,
    t_start_ms: u64,
    t_end_ms: u64,
    sample_rate_hz: u32,
    channel: ChannelId,
}

impl FedAudio {
    fn seconds(&self) -> f64 {
        (self.pcm.len() / 2) as f64 / self.sample_rate_hz as f64
    }
}

/// Audio on the wire that no final has covered yet.
#[derive(Default)]
struct ReplayBuffer {
    fed: VecDeque<FedAudio>,
    /// Unfinalized audio pushed out by the window. Only lost if the
    /// connection dies before a final gets past it.
    evicted_ms: u64,
}

impl ReplayBuffer {
    fn push(&mut self, audio: FedAudio) {
        self.fed.push_back(audio);
        while self.span_ms() > REPLAY_WINDOW_MS {
            if let Some(oldest) = self.fed.pop_front() {
                self.evicted_ms += oldest.t_end_ms - oldest.t_start_ms;
            }
        }
    }

    fn span_ms(&self) -> u64 {
        self.fed.iter().map(|a| a.t_end_ms - a.t_start_ms).sum()
    }

    /// A final reached session time `t_ms`, so replay starts there: replayed
    /// results then begin where the last final ended, leaving no overlap for
    /// the assembler to dedup.
    fn finalized_to(&mut self, t_ms: u64) {
        self.evicted_ms = 0;
        while self.fed.front().is_some_and(|a| a.t_end_ms <= t_ms) {
            self.fed.pop_front();
        }
        if let Some(audio) = self.fed.front_mut().filter(|a| a.t_start_ms < t_ms) {
            let samples = (t_ms - audio.t_start_ms) * audio.sample_rate_hz as u64 / 1000;
            audio
                .pcm
                .drain(..(samples as usize * 2).min(audio.pcm.len()));
            audio.t_start_ms = t_ms;
        }
    }
}

/// Convert a Results frame into an SttEvent using the fed map. Returns None
/// for empty transcripts (frequent in real captures).
pub(crate) fn results_to_event(r: &DgResults, map: &FedMap) -> Option<SttEvent> {
    let alt = r.alternatives_first()?;
    let text = alt.transcript.trim();
    if text.is_empty() {
        return None;
    }
    let t_start_ms = map.to_session_ms(r.start, Edge::Start)?;
    let t_end_ms = map.to_session_ms(r.start + r.duration, Edge::End)?;
    let seg = Segment {
        channel: map.channel(),
        t_start_ms,
        t_end_ms,
        text: text.to_string(),
    };
    Some(if r.is_final {
        SttEvent::Final(seg)
    } else {
        SttEvent::Partial(seg)
    })
}

impl DgResults {
    fn alternatives_first(&self) -> Option<&DgAlternative> {
        self.channel.alternatives.first()
    }
}

// ---------- provider ----------

pub struct DeepgramProvider {
    api_key: String,
    model: String,
    base_url: String,
}

impl DeepgramProvider {
    /// Read the API key from the env var named in config (process env, then
    /// the Windows user environment). The key lives in memory only; it is
    /// never persisted.
    pub fn from_env(cfg: &DeepgramConfig) -> Result<Self> {
        let api_key = auricle_core::secret_lookup(&cfg.api_key_env).ok_or_else(|| {
            Error::Stt(format!(
                "deepgram API key {} is not set (environment or credential store)",
                cfg.api_key_env
            ))
        })?;
        Ok(DeepgramProvider {
            api_key,
            model: cfg.model.clone(),
            base_url: DEEPGRAM_URL.to_string(),
        })
    }
}

#[async_trait]
impl SttProvider for DeepgramProvider {
    fn id(&self) -> &'static str {
        "deepgram"
    }

    fn kind(&self) -> SttKind {
        SttKind::CloudStreaming
    }

    async fn start_session(&self, cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
        // Goes into the URL unescaped, so this is the last place to catch it.
        validate_language(&cfg.language)?;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let finals = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(DropCounts::default());
        let url = format!(
            "{}/v1/listen?model={}&language={}&encoding=linear16&sample_rate=16000&channels=1&interim_results=true&smart_format=true",
            self.base_url,
            escape_query_value(&self.model),
            deepgram_language(&cfg.language)
        );
        let worker = tokio::spawn(worker_loop(
            self.api_key.clone(),
            url,
            cmd_rx,
            ev_tx,
            finals.clone(),
            dropped.clone(),
        ));
        Ok(Box::new(DeepgramSession {
            cmd_tx: Some(cmd_tx),
            events: ev_rx,
            worker: Some(worker),
            finals,
            dropped,
        }))
    }
}

/// Percent-encode everything but RFC 3986's unreserved characters.
fn escape_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

enum Cmd {
    Chunk(AudioChunk),
}

struct DeepgramSession {
    cmd_tx: Option<mpsc::UnboundedSender<Cmd>>,
    events: mpsc::UnboundedReceiver<SttEvent>,
    worker: Option<tokio::task::JoinHandle<()>>,
    finals: Arc<Mutex<Vec<Segment>>>,
    dropped: Arc<DropCounts>,
}

#[async_trait]
impl SttSession for DeepgramSession {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
        // Interim chunks are used as the *incremental feed*: the overlap
        // trim reduces each to only its new samples, so Deepgram receives a
        // near-continuous stream (~1 s granularity) instead of 5 s bursts,
        // and its own interim results arrive while the span is still open.
        // Final chunks are then usually fully-covered and trimmed to
        // nothing; whatever tail remains (hangover) is sent.
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| Error::Stt("session is finished".to_string()))?;
        tx.send(Cmd::Chunk(chunk))
            .map_err(|_| Error::Stt("deepgram worker is gone".to_string()))
    }

    async fn next_event(&mut self) -> Option<SttEvent> {
        self.events.recv().await
    }

    async fn finish(&mut self) -> Result<Vec<Segment>> {
        // Dropping the sender signals the worker to close the stream and
        // drain remaining results.
        self.cmd_tx.take();
        if let Some(worker) = self.worker.take() {
            worker
                .await
                .map_err(|e| Error::Stt(format!("deepgram worker panicked: {e}")))?;
        }
        Ok(self.finals.lock().unwrap().clone())
    }

    fn drops(&self) -> SttDrops {
        self.dropped.totals()
    }
}

/// Dropped without `finish` (a cancel), the session ends its worker: left
/// alone, the closed channel would read as a stop and send what is still
/// queued, or buffered from an outage, before closing the stream.
impl Drop for DeepgramSession {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, Message>;

enum ConnectError {
    /// The server refused this request; trying again cannot help.
    Rejected(String),
    Failed(String),
}

async fn connect(api_key: &str, url: &str) -> std::result::Result<WsStream, ConnectError> {
    let mut request = url
        .into_client_request()
        .map_err(|e| ConnectError::Rejected(e.to_string()))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Token {api_key}")
            .parse()
            .map_err(|_| ConnectError::Rejected("invalid api key header".to_string()))?,
    );
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((ws, _))) => Ok(ws),
        // 401/402/403 and bad parameters; 408 and 429 pass with time.
        Ok(Err(tungstenite::Error::Http(resp)))
            if resp.status().is_client_error() && !matches!(resp.status().as_u16(), 408 | 429) =>
        {
            Err(ConnectError::Rejected(format!("HTTP {}", resp.status())))
        }
        Ok(Err(e)) => Err(ConnectError::Failed(e.to_string())),
        Err(_) => Err(ConnectError::Failed(format!(
            "no handshake within {CONNECT_TIMEOUT:?}"
        ))),
    }
}

/// Bounded send: once a dead peer stops draining the socket, `send` blocks
/// rather than errors, which would keep the worker from ever reconnecting.
async fn send_ws(sink: &mut WsSink, message: Message) -> std::result::Result<(), ()> {
    match tokio::time::timeout(NO_FRAME_TIMEOUT, sink.send(message)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

fn f32_to_linear16(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|f| ((f.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
        .collect()
}

/// Trim the chunker's intra-span overlap so the stream never carries the
/// same audio twice, then return (samples, session_start_ms, is_seam).
fn prepare_chunk(chunk: &AudioChunk, map: &FedMap) -> Option<(Vec<f32>, u64, bool)> {
    let mut t_start = chunk.t_start_ms;
    let mut skip = 0usize;
    if map.last_session_end_ms > 0 && t_start < map.last_session_end_ms {
        let skip_ms = map.last_session_end_ms - t_start;
        skip = (skip_ms * chunk.sample_rate_hz as u64 / 1000) as usize;
        if skip >= chunk.samples.len() {
            return None; // entirely covered by already-fed audio
        }
        t_start = map.last_session_end_ms;
    }
    let is_seam =
        map.last_session_end_ms > 0 && t_start > map.last_session_end_ms + SEAM_THRESHOLD_MS;
    Some((chunk.samples[skip..].to_vec(), t_start, is_seam))
}

/// Sleep out a reconnect delay, buffering audio that arrives meanwhile. A
/// stop ends the wait at once: the closed channel would otherwise resolve
/// immediately on every poll until the deadline.
async fn wait_out_backoff(
    delay: Duration,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    pending: &mut VecDeque<AudioChunk>,
    dropped: &DropCounts,
    finishing: &mut bool,
) {
    let deadline = tokio::time::sleep(delay);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return,
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Chunk(c)) => {
                    let (accepted, evicted) = enqueue_with_shed(pending, c, PENDING_CAP);
                    dropped.record(accepted, evicted);
                }
                None => {
                    *finishing = true;
                    return;
                }
            }
        }
    }
}

/// Send audio the old connection took but never finalized. Each span is
/// recorded on the new connection's fed clock, so its results map back to
/// their original session times.
async fn replay_fed(
    sink: &mut WsSink,
    replay: &ReplayBuffer,
    map: &mut FedMap,
) -> std::result::Result<(), ()> {
    let mut prev_end = None;
    for audio in &replay.fed {
        if prev_end.is_some_and(|end| audio.t_start_ms > end + SEAM_THRESHOLD_MS) {
            send_ws(sink, Message::Text(FINALIZE.into())).await?;
        }
        send_ws(sink, Message::Binary(audio.pcm.clone().into())).await?;
        map.record(audio.t_start_ms, audio.seconds(), audio.channel);
        prev_end = Some(audio.t_end_ms);
    }
    Ok(())
}

fn report_undelivered(
    ev_tx: &mpsc::UnboundedSender<SttEvent>,
    pending: &VecDeque<AudioChunk>,
    replay: &ReplayBuffer,
    dropped: &DropCounts,
) {
    for chunk in pending {
        dropped.lost(chunk.kind);
    }
    if !pending.is_empty() || !replay.fed.is_empty() {
        let _ = ev_tx.send(SttEvent::Error(format!(
            "deepgram: {} unsent chunk(s) and {:.1} s of unfinalized audio undelivered",
            pending.len(),
            (replay.span_ms() + replay.evicted_ms) as f64 / 1000.0
        )));
    }
}

async fn worker_loop(
    api_key: String,
    url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ev_tx: mpsc::UnboundedSender<SttEvent>,
    finals: Arc<Mutex<Vec<Segment>>>,
    dropped: Arc<DropCounts>,
) {
    let mut map = FedMap::default();
    let mut replay = ReplayBuffer::default();
    let mut pending: VecDeque<AudioChunk> = VecDeque::new();
    let mut shed = ShedReporter::new("deepgram is unreachable");
    let mut backoff = INITIAL_BACKOFF;
    let mut finishing = false;
    let mut reconnecting = false;
    // One more connection is worth trying to deliver the tail at session
    // end; beyond that the stop must not wait.
    let mut tail_attempted = false;
    // The watchdog gave up on a connection with audio unanswered, so the next
    // one is that audio's second and last chance.
    let mut retrying_unanswered = false;

    'outer: loop {
        if reconnecting {
            // Every reconnect waits, so a server that accepts and closes
            // straight away cannot make this spin.
            if !finishing {
                wait_out_backoff(backoff, &mut cmd_rx, &mut pending, &dropped, &mut finishing)
                    .await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            if let Some(notice) = shed.poll(&dropped) {
                let _ = ev_tx.send(notice);
            }
            if finishing {
                if pending.is_empty() && replay.fed.is_empty() {
                    return;
                }
                if tail_attempted {
                    report_undelivered(&ev_tx, &pending, &replay, &dropped);
                    return;
                }
                tail_attempted = true;
            }
        }
        reconnecting = true;

        let ws = match connect(&api_key, &url).await {
            Ok(ws) => ws,
            Err(ConnectError::Rejected(reason)) => {
                let _ = ev_tx.send(SttEvent::Error(format!(
                    "deepgram rejected the connection ({reason}); check the API key and \
                     model. Not retrying"
                )));
                report_undelivered(&ev_tx, &pending, &replay, &dropped);
                return;
            }
            Err(ConnectError::Failed(e)) => {
                let _ = ev_tx.send(SttEvent::Error(format!(
                    "deepgram connect failed: {e}; retrying in {backoff:?}"
                )));
                continue;
            }
        };
        map.reset_connection();
        let (mut sink, mut stream) = ws.split();
        // When the server came to owe a frame: audio went out with nothing
        // heard since, or a Finalize did. VAD gaps send no audio and get no
        // frames back, so plain silence means nothing.
        let mut unanswered_since: Option<Instant> = None;
        // When to ask for what the server holds, if audio stays stopped.
        let mut finalize_at: Option<Instant> = None;

        if replay.evicted_ms > 0 {
            let _ = ev_tx.send(SttEvent::Error(format!(
                "deepgram: about {:.1} s of audio from before the disconnect was past the \
                 replay window and is lost",
                replay.evicted_ms as f64 / 1000.0
            )));
            replay.evicted_ms = 0;
        }
        if !replay.fed.is_empty() {
            if replay_fed(&mut sink, &replay, &mut map).await.is_err() {
                let _ = ev_tx.send(SttEvent::Error(
                    "deepgram send failed right after reconnect; retrying".to_string(),
                ));
                continue 'outer;
            }
            unanswered_since = Some(Instant::now());
        }

        // Re-send anything buffered while disconnected.
        while let Some(chunk) = pending.pop_front() {
            match send_chunk(&mut sink, &chunk, &mut map, &mut replay).await {
                Ok(true) => {
                    unanswered_since.get_or_insert_with(Instant::now);
                }
                Ok(false) => {}
                Err(()) => {
                    pending.push_front(chunk);
                    let _ = ev_tx.send(SttEvent::Error(
                        "deepgram send failed right after reconnect; retrying".to_string(),
                    ));
                    continue 'outer;
                }
            }
        }
        // That may be all the audio there is, and nothing else would ask the
        // server to finish with it.
        if !replay.fed.is_empty() {
            finalize_at = Some(Instant::now());
        }
        if finishing {
            let _ = send_ws(&mut sink, Message::Text(CLOSE_STREAM.into())).await;
            unanswered_since.get_or_insert_with(Instant::now);
        }

        let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let stale_at = unanswered_since.map(|t| t + NO_FRAME_TIMEOUT);
            // CloseStream flushes the server on its own.
            let finalize_due = finalize_at.filter(|_| !finishing);
            tokio::select! {
                cmd = cmd_rx.recv(), if !finishing => {
                    match cmd {
                        Some(Cmd::Chunk(chunk)) => {
                            match send_chunk(&mut sink, &chunk, &mut map, &mut replay).await {
                                Ok(true) => {
                                    unanswered_since.get_or_insert_with(Instant::now);
                                    finalize_at = Some(Instant::now() + FINALIZE_AFTER_AUDIO);
                                }
                                Ok(false) => {}
                                Err(()) => {
                                    pending.push_back(chunk);
                                    let _ = ev_tx.send(SttEvent::Error(
                                        "deepgram send failed; reconnecting".to_string(),
                                    ));
                                    continue 'outer;
                                }
                            }
                        }
                        None => {
                            finishing = true;
                            // Ask the server to flush and close; results keep
                            // arriving until the Metadata/close frame.
                            if send_ws(&mut sink, Message::Text(CLOSE_STREAM.into()))
                                .await
                                .is_err()
                            {
                                continue 'outer;
                            }
                            unanswered_since.get_or_insert_with(Instant::now);
                        }
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            // While draining, silence is never normal: keep
                            // the watch running so a stop can't hang here.
                            unanswered_since = finishing.then(Instant::now);
                            match serde_json::from_str::<DgFrame>(&t) {
                                Ok(DgFrame::Results(r)) => {
                                    // Real results prove the connection works;
                                    // accept-then-close never reaches this.
                                    backoff = INITIAL_BACKOFF;
                                    retrying_unanswered = false;
                                    if r.is_final {
                                        // Empty finals count: that audio was
                                        // heard and held no speech.
                                        if let Some(end) =
                                            map.to_session_ms(r.start + r.duration, Edge::End)
                                        {
                                            replay.finalized_to(end);
                                        }
                                    }
                                    if let Some(ev) = results_to_event(&r, &map) {
                                        if let SttEvent::Final(seg) = &ev {
                                            finals.lock().unwrap().push(seg.clone());
                                        }
                                        let _ = ev_tx.send(ev);
                                    }
                                }
                                Ok(DgFrame::Metadata(_)) => {
                                    // Sent right before close; nothing to do.
                                }
                                Err(_) => { /* unknown frame type: ignore */ }
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            if finishing {
                                return;
                            }
                            let reason = frame
                                .map(|f| format!("{} {}", f.code, f.reason))
                                .unwrap_or_else(|| "no close frame".to_string());
                            let _ = ev_tx.send(SttEvent::Error(format!(
                                "deepgram closed the stream ({reason}); reconnecting"
                            )));
                            continue 'outer;
                        }
                        Some(Ok(_)) => {} // binary/ping/pong: ignore
                        Some(Err(e)) => {
                            if !finishing {
                                let _ = ev_tx.send(SttEvent::Error(format!(
                                    "deepgram connection error: {e}; reconnecting"
                                )));
                            }
                            continue 'outer;
                        }
                        None => {
                            if !finishing {
                                let _ = ev_tx.send(SttEvent::Error(
                                    "deepgram stream ended; reconnecting".to_string(),
                                ));
                            }
                            continue 'outer;
                        }
                    }
                }
                _ = keepalive.tick() => {
                    // Bridges VAD gaps: the server closes after ~12 s without
                    // audio or text (captured verbatim in gap_test/).
                    if send_ws(&mut sink, Message::Text(r#"{"type":"KeepAlive"}"#.into()))
                        .await
                        .is_err() && !finishing
                    {
                        let _ = ev_tx.send(SttEvent::Error(
                            "deepgram keepalive failed; reconnecting".to_string(),
                        ));
                        continue 'outer;
                    }
                }
                _ = tokio::time::sleep_until(finalize_due.unwrap_or_else(Instant::now)),
                    if finalize_due.is_some() =>
                {
                    finalize_at = None;
                    // Finals may already have covered it all, and then no
                    // answer is owed.
                    if !replay.fed.is_empty() {
                        if send_ws(&mut sink, Message::Text(FINALIZE.into())).await.is_err() {
                            let _ = ev_tx.send(SttEvent::Error(
                                "deepgram finalize failed; reconnecting".to_string(),
                            ));
                            continue 'outer;
                        }
                        // Never a restart: speech with pauses sends one of
                        // these per burst, and would hold off the watch.
                        unanswered_since.get_or_insert_with(Instant::now);
                    }
                }
                _ = tokio::time::sleep_until(stale_at.unwrap_or_else(Instant::now)),
                    if stale_at.is_some() =>
                {
                    if retrying_unanswered {
                        // Every send of this audio is billed, and it has now
                        // gone unanswered on two connections.
                        let _ = ev_tx.send(SttEvent::Error(format!(
                            "deepgram never answered {:.1} s of audio; giving up on it",
                            (replay.span_ms() + replay.evicted_ms) as f64 / 1000.0
                        )));
                        replay = ReplayBuffer::default();
                        retrying_unanswered = false;
                    } else {
                        let _ = ev_tx.send(SttEvent::Error(format!(
                            "deepgram sent nothing for {NO_FRAME_TIMEOUT:?} after audio; \
                             reconnecting"
                        )));
                        retrying_unanswered = !replay.fed.is_empty();
                    }
                    continue 'outer;
                }
            }
        }
    }
}

/// Send one chunk's new audio. `Ok(true)` when audio actually went out.
async fn send_chunk(
    sink: &mut WsSink,
    chunk: &AudioChunk,
    map: &mut FedMap,
    replay: &mut ReplayBuffer,
) -> std::result::Result<bool, ()> {
    let Some((samples, t_start_ms, is_seam)) = prepare_chunk(chunk, map) else {
        return Ok(false); // fully-overlapped chunk: nothing new to send
    };
    if is_seam {
        // Span boundary: force finalization so utterances never merge
        // across the silence the VAD gated out.
        send_ws(sink, Message::Text(FINALIZE.into())).await?;
    }
    let pcm = f32_to_linear16(&samples);
    let seconds = samples.len() as f64 / chunk.sample_rate_hz as f64;
    send_ws(sink, Message::Binary(pcm.clone().into())).await?;
    map.record(t_start_ms, seconds, chunk.channel);
    map.last_session_end_ms = chunk.t_end_ms;
    replay.push(FedAudio {
        pcm,
        t_start_ms,
        t_end_ms: chunk.t_end_ms,
        sample_rate_hz: chunk.sample_rate_hz,
        channel: chunk.channel,
    });
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use auricle_core::ChunkKind;
    use auricle_pipeline::{Assembler, PipelineEvent};
    use std::io::{Read as _, Write as _};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    fn chunk(t_start_ms: u64, ms: u64) -> AudioChunk {
        AudioChunk {
            channel: ChannelId::Loopback,
            session_id: "t".to_string(),
            seq: 0,
            kind: ChunkKind::Final,
            sample_rate_hz: 16_000,
            t_start_ms,
            t_end_ms: t_start_ms + ms,
            samples: vec![0.25; (16 * ms) as usize],
        }
    }

    /// Like `chunk`, but every sample differs, so a test can tell which part
    /// of the audio arrived.
    fn ramp(t_start_ms: u64, ms: u64) -> AudioChunk {
        let n = 16 * ms as usize;
        AudioChunk {
            samples: (0..n).map(|i| i as f32 / n as f32).collect(),
            ..chunk(t_start_ms, ms)
        }
    }

    fn fed(t_start_ms: u64, t_end_ms: u64) -> FedAudio {
        FedAudio {
            pcm: vec![0; (t_end_ms - t_start_ms) as usize * 32],
            t_start_ms,
            t_end_ms,
            sample_rate_hz: 16_000,
            channel: ChannelId::Loopback,
        }
    }

    // ---------- worker tests against a local WebSocket server ----------

    async fn bound() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    async fn open_session(addr: SocketAddr) -> Box<dyn SttSession> {
        let provider = DeepgramProvider {
            api_key: "k".to_string(),
            model: "nova-3".to_string(),
            base_url: format!("ws://{addr}"),
        };
        provider
            .start_session(&SessionCfg {
                session_id: "t".to_string(),
                language: "en".to_string(),
            })
            .await
            .unwrap()
    }

    async fn accept(listener: &TcpListener) -> WebSocketStream<TcpStream> {
        let (socket, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(socket).await.unwrap()
    }

    /// The next audio frame, skipping keepalives.
    async fn next_audio(ws: &mut WebSocketStream<TcpStream>) -> Vec<u8> {
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(pcm))) => return pcm.to_vec(),
                Some(Ok(_)) => {}
                other => panic!("expected audio, got {other:?}"),
            }
        }
    }

    /// Wait for CloseStream, then close like the real server, reading on to
    /// the end of the handshake. Dropping the socket with client frames
    /// unread sends a reset on Windows, which discards any final the client
    /// hasn't read yet.
    async fn close_on_request(ws: &mut WebSocketStream<TcpStream>) {
        while let Some(Ok(message)) = ws.next().await {
            if matches!(&message, Message::Text(t) if t.as_str() == CLOSE_STREAM) {
                ws.close(None).await.unwrap();
            }
        }
    }

    fn final_frame(start: f64, duration: f64, text: &str) -> Message {
        let frame = serde_json::json!({
            "type": "Results", "start": start, "duration": duration,
            "is_final": true, "speech_final": true,
            "channel": {"alternatives": [{"transcript": text, "confidence": 0.99}]}
        });
        Message::Text(frame.to_string().into())
    }

    async fn next_event(session: &mut Box<dyn SttSession>) -> Option<SttEvent> {
        tokio::time::timeout(Duration::from_secs(5), session.next_event())
            .await
            .expect("no event within 5 s")
    }

    /// The next final, skipping the reconnect notices in between. One overall
    /// deadline: a worker stuck reconnecting emits notices forever.
    async fn next_final(session: &mut Box<dyn SttSession>) -> Segment {
        let wait = async {
            loop {
                match session.next_event().await {
                    Some(SttEvent::Final(seg)) => return seg,
                    Some(_) => {}
                    None => panic!("session ended before a final"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("no final within 10 s")
    }

    #[tokio::test]
    async fn disconnect_replays_unfinalized_audio_once_at_its_original_time() {
        let (listener, addr) = bound().await;
        let (a, b, c) = (ramp(1000, 1000), ramp(2000, 1000), ramp(3000, 1000));
        // The first final will reach 2500 ms, halfway through `b`.
        let unfinalized = f32_to_linear16(&b.samples)[16_000..].to_vec();
        let (saw_final, wait_for_final) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let mut ws = accept(&listener).await;
            next_audio(&mut ws).await;
            next_audio(&mut ws).await;
            ws.send(final_frame(0.0, 1.5, "first words")).await.unwrap();
            wait_for_final.await.unwrap();
            drop(ws); // no close handshake: the connection just dies

            let mut ws = accept(&listener).await;
            assert_eq!(
                next_audio(&mut ws).await,
                unfinalized,
                "replay from the final"
            );
            ws.send(final_frame(0.0, 0.5, "replayed words"))
                .await
                .unwrap();
            assert_eq!(
                next_audio(&mut ws).await.len(),
                32_000,
                "live audio resumes"
            );
            ws.send(final_frame(0.5, 1.0, "live words")).await.unwrap();
            close_on_request(&mut ws).await;
        });

        let mut session = open_session(addr).await;
        session.feed(a).await.unwrap();
        session.feed(b).await.unwrap();
        let first = next_final(&mut session).await;
        saw_final.send(()).unwrap();
        let second = next_final(&mut session).await;
        session.feed(c).await.unwrap();
        let third = next_final(&mut session).await;
        let finals = session.finish().await.unwrap();
        server.await.unwrap();

        let times: Vec<_> = [&first, &second, &third]
            .iter()
            .map(|s| (s.t_start_ms, s.t_end_ms))
            .collect();
        assert_eq!(
            times,
            vec![(1000, 2500), (2500, 3000), (3000, 4000)],
            "results land on session time despite the fed clock restarting"
        );
        assert_eq!(finals.len(), 3);

        let mut assembler = Assembler::new("t");
        let lines: Vec<_> = [first, second, third]
            .into_iter()
            .filter_map(|seg| match assembler.ingest(SttEvent::Final(seg)) {
                Some(PipelineEvent::Transcript(t)) => Some(t.text),
                _ => None,
            })
            .collect();
        assert_eq!(lines, vec!["first words", "replayed words", "live words"]);
    }

    #[tokio::test]
    async fn silent_socket_is_abandoned_and_its_audio_replayed() {
        let (listener, addr) = bound().await;
        let audio = ramp(1000, 1000);
        let expected = f32_to_linear16(&audio.samples);

        let server = tokio::spawn(async move {
            // Takes the audio and never answers, like a half-open socket.
            let mut dead = accept(&listener).await;
            next_audio(&mut dead).await;

            let mut ws = accept(&listener).await;
            assert_eq!(next_audio(&mut ws).await, expected, "nothing was finalized");
            ws.send(final_frame(0.0, 1.0, "recovered")).await.unwrap();
            close_on_request(&mut ws).await;
        });

        let mut session = open_session(addr).await;
        let started = Instant::now();
        session.feed(audio).await.unwrap();
        let seg = next_final(&mut session).await;
        assert_eq!(seg.text, "recovered");
        assert!(
            started.elapsed() >= NO_FRAME_TIMEOUT,
            "{:?}",
            started.elapsed()
        );
        session.finish().await.unwrap();
        server.await.unwrap();
    }

    /// Read up to the next Finalize, returning how much audio came first.
    async fn audio_before_finalize(ws: &mut WebSocketStream<TcpStream>) -> usize {
        let mut audio = 0;
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(_))) => audio += 1,
                Some(Ok(Message::Text(t))) if t.as_str() == FINALIZE => return audio,
                Some(Ok(_)) => {}
                other => panic!("expected a Finalize, got {other:?}"),
            }
        }
    }

    /// Holds short audio the way the real server does: nothing comes back
    /// until a Finalize asks for it. Each reconnect here would bill the audio
    /// again, so the silence after it has to stay on one connection.
    #[tokio::test]
    async fn a_short_utterance_then_long_silence_is_finalized_not_reconnected() {
        let (listener, addr) = bound().await;
        let (silence_over, wait_for_silence) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let mut ws = accept(&listener).await;
            assert_eq!(audio_before_finalize(&mut ws).await, 1);
            ws.send(final_frame(0.0, 0.6, "okay")).await.unwrap();

            let reconnected = tokio::time::timeout(NO_FRAME_TIMEOUT * 3, listener.accept()).await;
            assert!(reconnected.is_err(), "reconnected during silence");
            silence_over.send(()).unwrap();
            close_on_request(&mut ws).await;
        });

        let mut session = open_session(addr).await;
        session.feed(ramp(1000, 600)).await.unwrap();
        assert_eq!(next_final(&mut session).await.text, "okay");
        wait_for_silence.await.unwrap();
        session.finish().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_replay_is_finalized_without_waiting_for_more_audio() {
        let (listener, addr) = bound().await;
        let server = tokio::spawn(async move {
            let mut ws = accept(&listener).await;
            next_audio(&mut ws).await;
            drop(ws); // no close handshake: the connection just dies

            let mut ws = accept(&listener).await;
            assert_eq!(audio_before_finalize(&mut ws).await, 1, "the replay");
            ws.send(final_frame(0.0, 0.6, "replayed")).await.unwrap();
            close_on_request(&mut ws).await;
        });

        let mut session = open_session(addr).await;
        session.feed(ramp(1000, 600)).await.unwrap();
        assert_eq!(next_final(&mut session).await.text, "replayed");
        session.finish().await.unwrap();
        server.await.unwrap();
    }

    /// Deepgram doesn't promise an answer to a Finalize on very little audio,
    /// so this has to end on its own or the same tail is billed forever.
    #[tokio::test]
    async fn an_unanswered_tail_is_replayed_once_and_then_given_up_on() {
        let (listener, addr) = bound().await;
        let audio_frames = Arc::new(AtomicU32::new(0));
        let seen = audio_frames.clone();
        let server = tokio::spawn(async move {
            loop {
                let mut ws = accept(&listener).await;
                let seen = seen.clone();
                tokio::spawn(async move {
                    while let Some(Ok(message)) = ws.next().await {
                        if matches!(message, Message::Binary(_)) {
                            seen.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                });
            }
        });

        let mut session = open_session(addr).await;
        session.feed(ramp(1000, 600)).await.unwrap();
        let gave_up = async {
            loop {
                match session.next_event().await {
                    Some(SttEvent::Error(e)) if e.contains("giving up") => return e,
                    Some(_) => {}
                    None => panic!("session ended"),
                }
            }
        };
        let notice = tokio::time::timeout(NO_FRAME_TIMEOUT * 6, gave_up)
            .await
            .expect("the worker kept retrying the same audio");
        assert!(notice.contains("0.6 s"), "{notice}");

        tokio::time::sleep(NO_FRAME_TIMEOUT * 3).await;
        assert_eq!(
            audio_frames.load(Ordering::SeqCst),
            2,
            "sent, replayed once"
        );
        server.abort();
    }

    #[tokio::test]
    async fn audio_is_finalized_once_it_stops_not_while_it_flows() {
        let (listener, addr) = bound().await;
        let server = tokio::spawn(async move {
            let mut ws = accept(&listener).await;
            let audio = audio_before_finalize(&mut ws).await;
            ws.send(final_frame(0.0, 3.0, "all of it")).await.unwrap();
            close_on_request(&mut ws).await;
            audio
        });

        let mut session = open_session(addr).await;
        for i in 0..3 {
            session.feed(chunk(1000 + i * 1000, 1000)).await.unwrap();
        }
        assert_eq!(next_final(&mut session).await.text, "all of it");
        session.finish().await.unwrap();
        assert_eq!(server.await.unwrap(), 3);
    }

    #[tokio::test]
    async fn audio_a_final_already_covered_needs_no_finalize() {
        let (listener, addr) = bound().await;
        let server = tokio::spawn(async move {
            let mut ws = accept(&listener).await;
            next_audio(&mut ws).await;
            ws.send(final_frame(0.0, 1.0, "covered")).await.unwrap();

            let asked =
                tokio::time::timeout(FINALIZE_AFTER_AUDIO * 5, audio_before_finalize(&mut ws))
                    .await;
            assert!(asked.is_err(), "nothing was left to finalize");
            close_on_request(&mut ws).await;
        });

        let mut session = open_session(addr).await;
        session.feed(ramp(1000, 1000)).await.unwrap();
        assert_eq!(next_final(&mut session).await.text, "covered");
        tokio::time::sleep(FINALIZE_AFTER_AUDIO * 6).await;
        session.finish().await.unwrap();
        server.await.unwrap();
    }

    /// A long monologue never pauses for a Finalize, and a dead socket must
    /// still be noticed inside the replay window.
    #[tokio::test]
    async fn audio_that_keeps_flowing_unanswered_still_trips_the_watchdog() {
        let (listener, addr) = bound().await;
        let mut session = open_session(addr).await;
        let _dead = accept(&listener).await;

        let feed = async {
            for i in 0.. {
                session.feed(chunk(1000 + i * 50, 50)).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::select! {
            _ = feed => unreachable!(),
            reconnected = tokio::time::timeout(NO_FRAME_TIMEOUT * 4, listener.accept()) => {
                assert!(reconnected.is_ok(), "a dead socket went unnoticed");
            }
        }
    }

    /// Speech with pauses draws a Finalize after every burst. None of them may
    /// push the watch back, or talking would hide a dead socket indefinitely.
    #[tokio::test]
    async fn audio_in_bursts_cannot_hold_off_the_watchdog() {
        let (listener, addr) = bound().await;
        let mut session = open_session(addr).await;
        let _dead = accept(&listener).await;
        let started = Instant::now();

        let feed = async {
            for i in 0.. {
                session.feed(chunk(1000 + i * 50, 50)).await.unwrap();
                tokio::time::sleep(FINALIZE_AFTER_AUDIO * 2).await;
            }
        };
        tokio::select! {
            _ = feed => unreachable!(),
            reconnected = tokio::time::timeout(NO_FRAME_TIMEOUT * 4, listener.accept()) => {
                assert!(reconnected.is_ok(), "a dead socket went unnoticed");
            }
        }
        assert!(
            started.elapsed() >= NO_FRAME_TIMEOUT,
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn quiet_connection_with_no_audio_is_left_alone() {
        let (listener, addr) = bound().await;
        let session = open_session(addr).await;
        let _held = accept(&listener).await;

        // A VAD gap: no audio out, so no frames back is normal.
        let reconnected = tokio::time::timeout(NO_FRAME_TIMEOUT * 2, listener.accept()).await;
        assert!(reconnected.is_err(), "reconnected during a VAD gap");
        drop(session);
    }

    #[tokio::test]
    async fn auth_failure_is_surfaced_once_and_never_retried() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = attempts.clone();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                seen.fetch_add(1, Ordering::SeqCst);
                let _ = socket.read(&mut [0u8; 2048]);
                let _ = socket.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
            }
        });

        let mut session = open_session(addr).await;
        session.feed(chunk(0, 100)).await.unwrap();

        match next_event(&mut session).await {
            Some(SttEvent::Error(e)) => {
                assert!(e.contains("401"), "{e}");
                assert!(e.contains("Not retrying"), "{e}");
            }
            other => panic!("expected the auth error, got {other:?}"),
        }
        // The worker is gone, so the session ends rather than idling.
        while next_event(&mut session).await.is_some() {}
        tokio::time::sleep(INITIAL_BACKOFF * 5).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "no retries");
        assert!(session.feed(chunk(100, 100)).await.is_err());
        assert!(session.finish().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn accept_then_close_backs_off_instead_of_spinning() {
        let (listener, addr) = bound().await;
        let started = Instant::now();
        let session = open_session(addr).await;

        for _ in 0..4 {
            let mut ws = accept(&listener).await;
            let _ = ws.close(None).await;
        }

        // Three reconnects at 20, 40, and 80 ms. Spinning takes a few ms all
        // told, and load can only add time, so this bound cannot flake.
        let elapsed = started.elapsed();
        assert!(elapsed >= INITIAL_BACKOFF * 7, "{elapsed:?}");
        drop(session);
    }

    #[tokio::test]
    async fn a_session_dropped_unfinished_sends_none_of_the_audio_it_still_holds() {
        let (listener, addr) = bound().await;
        let mut session = open_session(addr).await;
        // The server has yet to take the connection, so this waits in the session.
        session.feed(chunk(0, 1000)).await.unwrap();
        session.feed(chunk(1000, 1000)).await.unwrap();
        // A cancel: dropped without finish.
        drop(session);

        let audio = Arc::new(AtomicU32::new(0));
        let seen = audio.clone();
        let _ = tokio::time::timeout(Duration::from_secs(2), async move {
            let (socket, _) = listener.accept().await.unwrap();
            let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
                return;
            };
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Binary(_) => {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                    Message::Text(t) if t.as_str() == CLOSE_STREAM => return,
                    _ => {}
                }
            }
        })
        .await;
        assert_eq!(audio.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stop_ends_a_backoff_wait_at_once_and_keeps_queued_audio() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Cmd::Chunk(chunk(0, 100))).unwrap();
        drop(tx);
        let mut pending = VecDeque::new();
        let mut finishing = false;

        tokio::time::timeout(
            Duration::from_secs(1),
            wait_out_backoff(
                Duration::from_secs(30),
                &mut rx,
                &mut pending,
                &DropCounts::default(),
                &mut finishing,
            ),
        )
        .await
        .expect("a closed channel ends the wait");

        assert!(finishing);
        assert_eq!(pending.len(), 1);
    }

    /// Run a backoff wait over `chunks`, as a long outage would.
    async fn buffer_during_outage(chunks: Vec<AudioChunk>) -> (VecDeque<AudioChunk>, DropCounts) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for chunk in chunks {
            tx.send(Cmd::Chunk(chunk)).unwrap();
        }
        drop(tx);
        let mut pending = VecDeque::new();
        let dropped = DropCounts::default();
        let mut finishing = false;
        wait_out_backoff(
            Duration::from_secs(30),
            &mut rx,
            &mut pending,
            &dropped,
            &mut finishing,
        )
        .await;
        (pending, dropped)
    }

    #[tokio::test]
    async fn a_long_outage_holds_a_bounded_backlog_and_counts_the_finals_lost() {
        let extra = 5;
        let chunks = (0..(PENDING_CAP + extra) as u64)
            .map(|i| chunk(i * 1000, 1000))
            .collect();

        let (pending, dropped) = buffer_during_outage(chunks).await;

        assert_eq!(pending.len(), PENDING_CAP);
        assert_eq!(pending[0].t_start_ms, extra as u64 * 1000, "oldest went");
        let lost = extra as u64;
        assert_eq!(
            dropped.totals(),
            SttDrops {
                chunks: lost,
                finals: lost
            }
        );
    }

    #[tokio::test]
    async fn interims_go_first_because_a_final_covers_their_audio() {
        let interim = |i: u64| AudioChunk {
            kind: ChunkKind::Interim,
            ..chunk(i * 1000, 1000)
        };
        let mut chunks: Vec<_> = (0..3).map(interim).collect();
        chunks.extend((3..PENDING_CAP as u64 + 3).map(|i| chunk(i * 1000, 1000)));

        let (pending, dropped) = buffer_during_outage(chunks).await;

        assert!(pending.iter().all(|c| c.kind == ChunkKind::Final));
        assert_eq!(
            dropped.totals(),
            SttDrops {
                chunks: 3,
                finals: 0
            }
        );
    }

    #[tokio::test]
    async fn an_outage_that_sheds_audio_says_so_while_it_lasts() {
        // Bound but never accepting: the worker sits in connect, then backoff.
        let (_listener, addr) = bound().await;
        let mut session = open_session(addr).await;
        for i in 0..PENDING_CAP as u64 + 5 {
            session.feed(chunk(i * 100, 100)).await.unwrap();
        }

        let wait = async {
            loop {
                match session.next_event().await {
                    Some(SttEvent::Error(e)) if e.contains("shed") => return e,
                    Some(_) => {}
                    None => panic!("session ended"),
                }
            }
        };
        let notice = tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("no shed notice");

        assert!(
            notice.starts_with("deepgram is unreachable: 5 chunk(s)"),
            "{notice}"
        );
        assert!(notice.contains("5 final(s) lost"), "{notice}");
        assert_eq!(
            session.drops(),
            SttDrops {
                chunks: 5,
                finals: 5
            }
        );
    }

    #[tokio::test]
    async fn chunks_still_held_when_the_session_gives_up_count_as_dropped() {
        // Bound but never accepting: the handshake can only time out.
        let (_listener, addr) = bound().await;
        let mut session = open_session(addr).await;
        session.feed(chunk(0, 100)).await.unwrap();
        session.feed(chunk(100, 100)).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), session.finish())
            .await
            .expect("stop must not hang")
            .unwrap();

        assert_eq!(session.drops().finals, 2, "{:?}", session.drops());
    }

    #[tokio::test]
    async fn a_session_never_holds_back_its_incremental_feed() {
        let (listener, addr) = bound().await;
        let fed = 6;
        let server = tokio::spawn(async move {
            let mut ws = accept(&listener).await;
            for _ in 0..fed {
                next_audio(&mut ws).await;
            }
            close_on_request(&mut ws).await;
        });

        let mut session = open_session(addr).await;
        // Finals that draw no answer, then interims: the shape that stopped
        // the feed when each final had to be answered first.
        for i in 0..fed {
            let kind = if i < 3 {
                ChunkKind::Final
            } else {
                ChunkKind::Interim
            };
            let audio = AudioChunk {
                kind,
                ..chunk(i * 1000, 1000)
            };
            session.feed(audio).await.unwrap();
        }

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("every chunk must reach the socket")
            .unwrap();
        drop(session);
    }

    #[test]
    fn a_model_name_cannot_add_url_parameters() {
        assert_eq!(escape_query_value("nova-3"), "nova-3");
        assert_eq!(
            escape_query_value("nova-3&redact=pci x"),
            "nova-3%26redact%3Dpci%20x"
        );
    }

    #[tokio::test]
    async fn a_session_refuses_a_language_that_would_change_its_url() {
        let provider = DeepgramProvider {
            api_key: "k".to_string(),
            model: "nova-3".to_string(),
            base_url: "ws://127.0.0.1:9".to_string(),
        };
        let cfg = SessionCfg {
            session_id: "t".to_string(),
            language: "en&model=x".to_string(),
        };
        assert!(provider.start_session(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn stop_with_no_server_reports_the_undelivered_audio() {
        // Bound but never accepting: the handshake can only time out.
        let (_listener, addr) = bound().await;
        let mut session = open_session(addr).await;
        session.feed(chunk(0, 100)).await.unwrap();

        match next_event(&mut session).await {
            Some(SttEvent::Error(e)) => assert!(e.contains("no handshake"), "{e}"),
            other => panic!("expected the connect timeout, got {other:?}"),
        }
        tokio::time::timeout(Duration::from_secs(5), session.finish())
            .await
            .expect("stop must not hang")
            .unwrap();

        let mut undelivered = false;
        while let Some(event) = next_event(&mut session).await {
            undelivered |= matches!(event, SttEvent::Error(e) if e.contains("undelivered"));
        }
        assert!(undelivered);
    }

    #[tokio::test]
    async fn stop_on_a_dead_socket_still_delivers_the_tail() {
        let (listener, addr) = bound().await;
        let server = tokio::spawn(async move {
            // Ignores CloseStream as well as the audio.
            let mut dead = accept(&listener).await;
            next_audio(&mut dead).await;

            let mut ws = accept(&listener).await;
            next_audio(&mut ws).await;
            ws.send(final_frame(0.0, 1.0, "the tail")).await.unwrap();
            close_on_request(&mut ws).await;
        });

        let mut session = open_session(addr).await;
        session.feed(ramp(1000, 1000)).await.unwrap();
        let finals = tokio::time::timeout(Duration::from_secs(5), session.finish())
            .await
            .expect("stop must not hang")
            .unwrap();

        let mut events = Vec::new();
        while let Some(event) = next_event(&mut session).await {
            events.push(event);
        }
        assert_eq!(finals.len(), 1, "{events:?}");
        assert_eq!(finals[0].text, "the tail");
        server.await.unwrap();
    }

    // ---------- replay buffer ----------

    #[test]
    fn a_final_releases_covered_audio_and_trims_the_one_it_splits() {
        let mut replay = ReplayBuffer::default();
        replay.push(fed(1000, 2000));
        replay.push(fed(2000, 3000));

        replay.finalized_to(2500);

        assert_eq!(replay.fed.len(), 1);
        assert_eq!(replay.fed[0].t_start_ms, 2500);
        assert_eq!(replay.fed[0].pcm.len(), 500 * 32, "500 ms of linear16");
        assert!((replay.fed[0].seconds() - 0.5).abs() < 1e-9);

        replay.finalized_to(3000);
        assert!(replay.fed.is_empty());
    }

    #[test]
    fn audio_past_the_window_counts_as_lost_until_a_final_passes_it() {
        let mut replay = ReplayBuffer::default();
        for second in 0..12 {
            replay.push(fed(second * 1000, (second + 1) * 1000));
        }
        assert_eq!(replay.span_ms(), REPLAY_WINDOW_MS);
        assert_eq!(replay.evicted_ms, 2000);

        replay.finalized_to(5000);
        assert_eq!(replay.evicted_ms, 0, "the server had it after all");
    }

    #[test]
    fn fed_map_translates_across_a_gap() {
        let mut map = FedMap::default();
        // Span 1 at session 1000..6000, span 2 at 20000..22000 (14 s gap).
        map.record(1000, 5.0, ChannelId::Loopback);
        map.last_session_end_ms = 6000;
        map.record(20_000, 2.0, ChannelId::Loopback);
        map.last_session_end_ms = 22_000;

        // Deepgram's clock is fed-audio seconds: 0..5 is span 1, 5..7 span 2.
        assert_eq!(map.to_session_ms(0.0, Edge::Start), Some(1000));
        assert_eq!(map.to_session_ms(2.5, Edge::Start), Some(3500));
        assert_eq!(map.to_session_ms(2.5, Edge::End), Some(3500));
        // Boundary 5.0 is the end of span 1 AND the start of span 2 — a
        // 14-second difference in session time. The edge hint disambiguates.
        assert_eq!(map.to_session_ms(5.0, Edge::End), Some(6000));
        assert_eq!(map.to_session_ms(5.0, Edge::Start), Some(20_000));
        assert_eq!(map.to_session_ms(5.5, Edge::Start), Some(20_500));
        assert_eq!(map.to_session_ms(7.0, Edge::End), Some(22_000));
        // Past-the-end (finalize padding): clamps into the last range.
        assert_eq!(map.to_session_ms(9.0, Edge::End), Some(22_000));
    }

    #[test]
    fn overlap_is_trimmed_before_sending() {
        let mut map = FedMap::default();
        map.record(1000, 5.0, ChannelId::Loopback);
        map.last_session_end_ms = 6000;

        // Chunker overlap: next final starts 500 ms inside fed audio.
        let c = chunk(5500, 5000);
        let (samples, t_start, is_seam) = prepare_chunk(&c, &map).unwrap();
        assert_eq!(t_start, 6000, "starts where fed audio ended");
        assert_eq!(samples.len(), (16 * 4500) as usize, "500 ms trimmed");
        assert!(!is_seam, "contiguous audio is not a seam");
    }

    #[test]
    fn fully_covered_chunk_is_dropped() {
        let mut map = FedMap::default();
        map.record(0, 10.0, ChannelId::Loopback);
        map.last_session_end_ms = 10_000;
        assert!(prepare_chunk(&chunk(8000, 1000), &map).is_none());
    }

    #[test]
    fn span_gap_is_a_seam() {
        let mut map = FedMap::default();
        map.record(0, 5.0, ChannelId::Loopback);
        map.last_session_end_ms = 5000;
        let (_, t_start, is_seam) = prepare_chunk(&chunk(9000, 1000), &map).unwrap();
        assert_eq!(t_start, 9000);
        assert!(is_seam, "4 s of gated silence must trigger Finalize");
    }

    #[test]
    fn first_chunk_is_never_a_seam() {
        let map = FedMap::default();
        let (_, _, is_seam) = prepare_chunk(&chunk(3000, 1000), &map).unwrap();
        assert!(!is_seam);
    }

    #[test]
    fn results_map_to_partial_and_final_events() {
        let mut map = FedMap::default();
        map.record(2000, 5.0, ChannelId::Loopback);
        map.last_session_end_ms = 7000;

        // Interim frame shape from fixtures/deepgram/frame_000.json.
        let interim: DgFrame = serde_json::from_str(
            r#"{"type":"Results","channel_index":[0,1],"duration":1.0,"start":0.0,
                "is_final":false,"speech_final":false,
                "channel":{"alternatives":[{"transcript":"Quick brown","confidence":0.99}]}}"#,
        )
        .unwrap();
        let DgFrame::Results(r) = interim else {
            panic!()
        };
        match results_to_event(&r, &map) {
            Some(SttEvent::Partial(seg)) => {
                assert_eq!(seg.t_start_ms, 2000);
                assert_eq!(seg.t_end_ms, 3000);
                assert_eq!(seg.text, "Quick brown");
                assert_eq!(seg.channel, ChannelId::Loopback);
            }
            other => panic!("expected partial, got {other:?}"),
        }

        let fin: DgFrame = serde_json::from_str(
            r#"{"type":"Results","channel_index":[0,1],"duration":2.37,"start":0.0,
                "is_final":true,"speech_final":true,
                "channel":{"alternatives":[{"transcript":"Quick brown fox.","confidence":0.99}]},
                "from_finalize":false}"#,
        )
        .unwrap();
        let DgFrame::Results(r) = fin else { panic!() };
        assert!(matches!(
            results_to_event(&r, &map),
            Some(SttEvent::Final(_))
        ));
    }

    #[test]
    fn empty_transcript_results_are_skipped() {
        let mut map = FedMap::default();
        map.record(0, 5.0, ChannelId::Loopback);
        let r: DgFrame = serde_json::from_str(
            r#"{"type":"Results","duration":1.13,"start":2.37,"is_final":false,
                "speech_final":false,
                "channel":{"alternatives":[{"transcript":"","confidence":0.0}]}}"#,
        )
        .unwrap();
        let DgFrame::Results(r) = r else { panic!() };
        assert!(results_to_event(&r, &map).is_none());
    }
}
