//! End-to-end: server on a random port, session driven by the Phase-1
//! fixture WAV through a fake STT provider (real Silero VAD + chunker +
//! assembler + store), transcript asserted via REST, events via WS.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, ChunkKind, Config, Result, Segment, SttEvent};
use auricle_server::{build_router, AudioSource, Engine, EngineOptions, StartParams};
use auricle_stt::{SessionCfg, SttKind, SttProvider, SttSession};
use futures_util::StreamExt;
use tokio::sync::mpsc;

// ---------- fake provider: deterministic text per fed chunk ----------

struct FakeProvider {
    chunks_seen: Arc<AtomicU64>,
}

struct FakeSession {
    chunks_seen: Arc<AtomicU64>,
    finals: Vec<Segment>,
    tx: mpsc::UnboundedSender<SttEvent>,
    rx: mpsc::UnboundedReceiver<SttEvent>,
}

#[async_trait]
impl SttProvider for FakeProvider {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn kind(&self) -> SttKind {
        SttKind::Local
    }
    async fn start_session(&self, _cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Box::new(FakeSession {
            chunks_seen: self.chunks_seen.clone(),
            finals: Vec::new(),
            tx,
            rx,
        }))
    }
}

#[async_trait]
impl SttSession for FakeSession {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
        self.chunks_seen.fetch_add(1, Ordering::Relaxed);
        let seg = Segment {
            channel: chunk.channel,
            t_start_ms: chunk.t_start_ms,
            t_end_ms: chunk.t_end_ms,
            text: format!("spoken words {}", chunk.seq),
        };
        let ev = match chunk.kind {
            ChunkKind::Interim => SttEvent::Partial(seg),
            ChunkKind::Final => {
                self.finals.push(seg.clone());
                SttEvent::Final(seg)
            }
        };
        let _ = self.tx.send(ev);
        Ok(())
    }
    async fn next_event(&mut self) -> Option<SttEvent> {
        self.rx.recv().await
    }
    async fn finish(&mut self) -> Result<Vec<Segment>> {
        self.rx.close();
        Ok(self.finals.clone())
    }
}

// ---------- helpers ----------

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tts_loopback_16k.wav")
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("auricle-server-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn_server(name: &str) -> (String, Arc<Engine>) {
    let engine = Engine::new(EngineOptions {
        cfg: Config::default(),
        data_root: temp_root(name),
        provider_override: Some(Arc::new(FakeProvider {
            chunks_seen: Arc::new(AtomicU64::new(0)),
        })),
    })
    .unwrap();
    let app = build_router(engine.clone(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), engine)
}

// ---------- the test ----------

#[tokio::test]
async fn full_session_via_rest_and_ws() {
    let (base, _engine) = spawn_server("full").await;
    let client = reqwest::Client::new();

    // Health before anything.
    let health: serde_json::Value = client
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["state"], "idle");

    // Subscribe to live events first.
    let ws_url = base.replace("http://", "ws://") + "/ws/live";
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    let (_, mut ws_rx) = ws.split();
    let events: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let events_clone = events.clone();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                events_clone
                    .lock()
                    .await
                    .push(serde_json::from_str(&t).unwrap());
            }
        }
    });

    // Start a session fed by the fixture WAV on the loopback channel. The
    // WAV audio source is an engine-level option (not exposed over HTTP),
    // so the session starts via the engine; everything else — conflict
    // handling, stop, retrieval, export — is exercised over HTTP.
    let id = _engine
        .start_session(StartParams {
            title: Some("integration run".into()),
            stt_provider: None,
            mic_device: None,
            loopback_device: None,
            audio: Some(AudioSource::WavFile {
                path: fixture_path(),
                channel: ChannelId::Loopback,
            }),
        })
        .await
        .unwrap();

    // Concurrent start over HTTP must 409 while recording.
    let resp = client
        .post(format!("{base}/api/v1/sessions"))
        .json(&serde_json::json!({"title": "second"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["active_session"], id.as_str());

    // Let the (accelerated) fixture play through the pipeline.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Stop over HTTP.
    let resp = client
        .post(format!("{base}/api/v1/sessions/{id}/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Wait for the session_stopped event.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        {
            let evs = events.lock().await;
            if evs
                .iter()
                .any(|e| e["type"] == "session_stopped" && e["session"] == id.as_str())
            {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no session_stopped within 10 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // WS saw the lifecycle + finals.
    {
        let evs = events.lock().await;
        assert!(
            evs.iter()
                .any(|e| e["type"] == "session_started" && e["stt_provider"] == "fake"),
            "session_started observed: {evs:#?}"
        );
        let finals: Vec<_> = evs.iter().filter(|e| e["type"] == "final").collect();
        assert!(!finals.is_empty(), "finals observed over WS: {evs:#?}");
        assert!(finals.iter().all(|e| e["speaker"] == "Them"));
    }

    // Transcript via REST: non-empty, ordered, speaker-tagged, ended.
    let session: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["title"], "integration run");
    assert!(session["ended_at"].is_i64(), "session marked ended");
    let transcript = session["transcript"].as_array().unwrap();
    assert!(!transcript.is_empty(), "persisted transcript non-empty");
    let starts: Vec<i64> = transcript
        .iter()
        .map(|s| s["t_start_ms"].as_i64().unwrap())
        .collect();
    assert!(starts.windows(2).all(|w| w[0] <= w[1]), "ordered");
    assert!(transcript.iter().all(|s| s["speaker"] == "Them"));
    assert!(transcript.iter().all(|s| s["provider"] == "fake"));

    // Export as markdown.
    let md = client
        .get(format!("{base}/api/v1/sessions/{id}/export?format=md"))
        .send()
        .await
        .unwrap();
    assert_eq!(md.status(), 200);
    let text = md.text().await.unwrap();
    assert!(text.contains("# integration run"));
    assert!(text.contains("**["));

    // Sessions list contains it; health is idle again.
    let list: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == id.as_str()));
    let health: serde_json::Value = client
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["state"], "idle");

    // Settings roundtrip.
    let put: serde_json::Value = client
        .put(format!("{base}/api/v1/settings"))
        .json(&serde_json::json!({"transcript_font": "mono", "retain": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["transcript_font"], "mono");
    let got: serde_json::Value = client
        .get(format!("{base}/api/v1/settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["retain"], false);

    // Unknown session: 404. Unknown export format: 400.
    assert_eq!(
        client
            .get(format!("{base}/api/v1/sessions/nope"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .get(format!("{base}/api/v1/sessions/{id}/export?format=pdf"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}
