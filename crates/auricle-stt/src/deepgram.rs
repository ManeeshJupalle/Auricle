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
//!   silence that was gated out.
//! - Empty-transcript Results frames are real and frequent; skipped.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, DeepgramConfig, Error, Result, Segment, SttEvent};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::{SessionCfg, SttKind, SttProvider, SttSession};

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Session-time discontinuity treated as a span seam (mirrors the
/// pipeline's gap threshold).
const SEAM_THRESHOLD_MS: u64 = 250;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

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
}

impl DeepgramProvider {
    /// Read the API key from the env var named in config (process env, then
    /// the Windows user environment). The key lives in memory only; it is
    /// never persisted.
    pub fn from_env(cfg: &DeepgramConfig) -> Result<Self> {
        let api_key = auricle_core::env_lookup(&cfg.api_key_env).ok_or_else(|| {
            Error::Stt(format!(
                "deepgram API key env var {} is not set",
                cfg.api_key_env
            ))
        })?;
        Ok(DeepgramProvider {
            api_key,
            model: cfg.model.clone(),
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
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let finals = Arc::new(Mutex::new(Vec::new()));
        let worker = tokio::spawn(worker_loop(
            self.api_key.clone(),
            self.model.clone(),
            cfg.language.clone(),
            cmd_rx,
            ev_tx,
            finals.clone(),
        ));
        Ok(Box::new(DeepgramSession {
            cmd_tx: Some(cmd_tx),
            events: ev_rx,
            worker: Some(worker),
            finals,
        }))
    }
}

enum Cmd {
    Chunk(AudioChunk),
}

struct DeepgramSession {
    cmd_tx: Option<mpsc::UnboundedSender<Cmd>>,
    events: mpsc::UnboundedReceiver<SttEvent>,
    worker: Option<tokio::task::JoinHandle<()>>,
    finals: Arc<Mutex<Vec<Segment>>>,
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
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    api_key: &str,
    model: &str,
    language: &str,
) -> std::result::Result<WsStream, String> {
    let url = format!(
        "wss://api.deepgram.com/v1/listen?model={model}&language={language}&encoding=linear16&sample_rate=16000&channels=1&interim_results=true&smart_format=true"
    );
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request.headers_mut().insert(
        "Authorization",
        format!("Token {api_key}")
            .parse()
            .map_err(|_| "invalid api key header".to_string())?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ws)
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

async fn worker_loop(
    api_key: String,
    model: String,
    language: String,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ev_tx: mpsc::UnboundedSender<SttEvent>,
    finals: Arc<Mutex<Vec<Segment>>>,
) {
    let mut map = FedMap::default();
    let mut pending: VecDeque<AudioChunk> = VecDeque::new();
    let mut backoff = Duration::from_secs(1);
    let mut finishing = false;

    'outer: loop {
        let ws = match connect(&api_key, &model, &language).await {
            Ok(ws) => ws,
            Err(e) => {
                let _ = ev_tx.send(SttEvent::Error(format!(
                    "deepgram connect failed: {e}; retrying in {backoff:?}"
                )));
                if finishing {
                    // Can't deliver the tail; give up cleanly.
                    let _ = ev_tx.send(SttEvent::Error(format!(
                        "deepgram: {} chunk(s) undelivered at session end",
                        pending.len()
                    )));
                    return;
                }
                // Sleep, but keep accepting chunks (they buffer in `pending`).
                let deadline = tokio::time::sleep(backoff);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => break,
                        cmd = cmd_rx.recv() => match cmd {
                            Some(Cmd::Chunk(c)) => pending.push_back(c),
                            None => { finishing = true; }
                        }
                    }
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        map.reset_connection();
        let (mut sink, mut stream) = ws.split();

        // Re-send anything buffered while disconnected.
        while let Some(chunk) = pending.pop_front() {
            if send_chunk(&mut sink, &chunk, &mut map).await.is_err() {
                pending.push_front(chunk);
                let _ = ev_tx.send(SttEvent::Error(
                    "deepgram send failed right after reconnect; retrying".to_string(),
                ));
                continue 'outer;
            }
        }
        if finishing {
            let _ = sink
                .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
                .await;
        }

        let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                cmd = cmd_rx.recv(), if !finishing => {
                    match cmd {
                        Some(Cmd::Chunk(chunk)) => {
                            if send_chunk(&mut sink, &chunk, &mut map).await.is_err() {
                                pending.push_back(chunk);
                                let _ = ev_tx.send(SttEvent::Error(
                                    "deepgram send failed; reconnecting".to_string(),
                                ));
                                continue 'outer;
                            }
                        }
                        None => {
                            finishing = true;
                            // Ask the server to flush and close; results keep
                            // arriving until the Metadata/close frame.
                            if sink
                                .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
                                .await
                                .is_err()
                            {
                                continue 'outer;
                            }
                        }
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            match serde_json::from_str::<DgFrame>(&t) {
                                Ok(DgFrame::Results(r)) => {
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
                            if finishing {
                                return;
                            }
                            let _ = ev_tx.send(SttEvent::Error(format!(
                                "deepgram connection error: {e}; reconnecting"
                            )));
                            continue 'outer;
                        }
                        None => {
                            if finishing {
                                return;
                            }
                            let _ = ev_tx.send(SttEvent::Error(
                                "deepgram stream ended; reconnecting".to_string(),
                            ));
                            continue 'outer;
                        }
                    }
                }
                _ = keepalive.tick() => {
                    // Bridges VAD gaps: the server closes after ~12 s without
                    // audio or text (captured verbatim in gap_test/).
                    if sink
                        .send(Message::Text(r#"{"type":"KeepAlive"}"#.into()))
                        .await
                        .is_err() && !finishing
                    {
                        let _ = ev_tx.send(SttEvent::Error(
                            "deepgram keepalive failed; reconnecting".to_string(),
                        ));
                        continue 'outer;
                    }
                }
            }
        }
    }
}

async fn send_chunk(
    sink: &mut futures_util::stream::SplitSink<WsStream, Message>,
    chunk: &AudioChunk,
    map: &mut FedMap,
) -> std::result::Result<(), ()> {
    let Some((samples, t_start_ms, is_seam)) = prepare_chunk(chunk, map) else {
        return Ok(()); // fully-overlapped chunk: nothing new to send
    };
    if is_seam {
        // Span boundary: force finalization so utterances never merge
        // across the silence the VAD gated out.
        sink.send(Message::Text(r#"{"type":"Finalize"}"#.into()))
            .await
            .map_err(|_| ())?;
    }
    let pcm = f32_to_linear16(&samples);
    let seconds = samples.len() as f64 / chunk.sample_rate_hz as f64;
    sink.send(Message::Binary(pcm.into()))
        .await
        .map_err(|_| ())?;
    map.record(t_start_ms, seconds, chunk.channel);
    map.last_session_end_ms = chunk.t_end_ms;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use auricle_core::ChunkKind;

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
