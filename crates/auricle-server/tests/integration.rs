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
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    spawn_server_with(Config::default(), name).await
}

async fn spawn_server_with(cfg: Config, name: &str) -> (String, Arc<Engine>) {
    let engine = Engine::new(EngineOptions {
        cfg,
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

// ---------- the tests ----------

#[tokio::test]
async fn retained_audio_is_16bit_and_served_with_ranges() {
    let (base, engine) = spawn_server("audio").await;
    let client = reqwest::Client::new();

    // Opt into raw-audio retention via the settings overlay, like the UI.
    client
        .put(format!("{base}/api/v1/settings"))
        .json(&serde_json::json!({"retain_raw_audio": true}))
        .send()
        .await
        .unwrap();

    let id = engine
        .start_session(StartParams {
            title: Some("audio run".into()),
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
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    engine.stop_session(&id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.active_session().is_some() {
        assert!(std::time::Instant::now() < deadline, "stop did not finish");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Range request (what <audio> seeking issues): 206 with the WAV header
    // slice, and the retained file is 16-bit PCM (offset 34 of the header).
    let resp = client
        .get(format!("{base}/api/v1/sessions/{id}/audio/loopback"))
        .header("Range", "bytes=0-43")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206, "range requests must be honored");
    assert!(resp.headers().contains_key("content-range"));
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(
        u16::from_le_bytes([bytes[34], bytes[35]]),
        16,
        "retained audio must be 16-bit PCM"
    );

    // Plain GET still works; unknown channel is still a 400.
    let resp = client
        .get(format!("{base}/api/v1/sessions/{id}/audio/mic"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        client
            .get(format!("{base}/api/v1/sessions/{id}/audio/nope"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn session_audio_never_serves_paths_outside_the_sessions_dir() {
    // Defense-in-depth: if session meta is tampered to point at an
    // arbitrary file, the audio endpoint must refuse to serve it.
    let (base, engine) = spawn_server("audio-containment").await;
    // The sessions root must exist on disk, or the endpoint 404s by
    // failing to canonicalize a missing root — which would pass this test
    // for the wrong reason (never exercising the containment comparison).
    std::fs::create_dir_all(engine.data_root().join("sessions")).unwrap();
    let outside = fixture_path(); // a real file, deliberately outside data_root/sessions
    engine
        .store()
        .create_session(
            "tampered",
            "t",
            1_700_000_000,
            "fake",
            &serde_json::json!({"audio": {"loopback": outside.to_string_lossy()}}),
        )
        .unwrap();

    let resp = reqwest::Client::new()
        .get(format!("{base}/api/v1/sessions/tampered/audio/loopback"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "outside path must not be served");
}

#[tokio::test]
async fn secrets_endpoint_rejects_unknown_id_and_empty_value() {
    // Both paths return before the credential store is touched, so this
    // never reads or writes real keys on the host.
    let (base, _engine) = spawn_server("secrets").await;
    let client = reqwest::Client::new();

    let unknown = client
        .put(format!("{base}/api/v1/secrets/not-a-provider"))
        .json(&serde_json::json!({"value": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404, "unknown secret id must 404");

    let empty = client
        .put(format!("{base}/api/v1/secrets/deepgram"))
        .json(&serde_json::json!({"value": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400, "empty value must 400");
}

#[tokio::test]
async fn egress_ledger_records_stt_audio_on_session_start() {
    let (base, engine) = spawn_server("egress").await;
    let client = reqwest::Client::new();

    let id = engine
        .start_session(StartParams {
            title: Some("egress run".into()),
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

    let ledger: serde_json::Value = client
        .get(format!("{base}/api/v1/egress"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = ledger["entries"].as_array().unwrap();
    let audio = entries
        .iter()
        .find(|e| e["kind"] == "audio")
        .expect("session start records an audio egress row");
    assert_eq!(
        audio["destination"], "local",
        "the fake STT provider is local"
    );
    assert_eq!(audio["session_id"], id);

    engine.stop_session(&id).unwrap();
}

#[tokio::test]
async fn cross_origin_and_rebound_requests_are_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (base, _engine) = spawn_server("secure").await;
    let addr = base.strip_prefix("http://").unwrap().to_string();
    let ws_url = base.replace("http://", "ws://") + "/ws/live";

    // Cross-site WebSocket hijack: WS handshakes carry Origin and are
    // exempt from CORS — a foreign page must not be able to subscribe to
    // the live transcript.
    let mut req = ws_url.clone().into_client_request().unwrap();
    req.headers_mut()
        .insert("Origin", "https://evil.example".parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 403);
        }
        other => panic!("foreign-origin WS must be refused with 403, got {other:?}"),
    }

    // The bundled UI (same-origin) still connects.
    let mut req = ws_url.clone().into_client_request().unwrap();
    req.headers_mut()
        .insert("Origin", format!("http://{addr}").parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(req).await.is_ok(),
        "same-origin WS must connect"
    );

    // DNS rebinding: the TCP connection reaches 127.0.0.1 but Host names
    // the attacker's domain. Raw socket — real HTTP clients won't forge
    // Host for us.
    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
    sock.write_all(
        b"GET /api/v1/health HTTP/1.1\r\nHost: evil.example:4820\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut buf = String::new();
    let _ = sock.read_to_string(&mut buf).await;
    assert!(
        buf.starts_with("HTTP/1.1 403"),
        "rebound Host must 403: {buf}"
    );

    // A genuine localhost Host still answers.
    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let request =
        format!("GET /api/v1/health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut buf = String::new();
    let _ = sock.read_to_string(&mut buf).await;
    assert!(
        buf.starts_with("HTTP/1.1 200"),
        "localhost Host must 200: {buf}"
    );
}

#[tokio::test]
async fn session_gets_auto_titled_after_stop() {
    // Mock LLM plays "ollama" (the default [llm].provider).
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "c", "object": "chat.completion", "model": "m",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant",
                            "content": "\"Pipeline Latency Review.\"\n"}}]
        })))
        .mount(&llm)
        .await;
    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri();

    let (base, engine) = spawn_server_with(cfg, "autotitle").await;
    let client = reqwest::Client::new();

    // Watch for the session_updated event.
    let ws_url = base.replace("http://", "ws://") + "/ws/live";
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    let (_, mut ws_rx) = ws.split();
    let titled: Arc<tokio::sync::Mutex<Option<(String, String)>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let titled_clone = titled.clone();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "session_updated" {
                    *titled_clone.lock().await = Some((
                        v["session"].as_str().unwrap().to_string(),
                        v["title"].as_str().unwrap().to_string(),
                    ));
                }
            }
        }
    });

    // Start WITHOUT a title: placeholder + auto_title flag.
    let id = engine
        .start_session(StartParams {
            title: None,
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
    let detail: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["title"], "Untitled session");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    engine.stop_session(&id).unwrap();

    // Wait for the auto-title (sanitized: quotes + trailing dot stripped).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some((session, title)) = titled.lock().await.clone() {
            assert_eq!(session, id);
            assert_eq!(title, "Pipeline Latency Review");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no session_updated");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let detail: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["title"], "Pipeline Latency Review");

    // A user rename clears the flag, so future auto-titling never bites.
    client
        .patch(format!("{base}/api/v1/sessions/{id}"))
        .json(&serde_json::json!({"title": "My name"}))
        .send()
        .await
        .unwrap();
    let detail: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["title"], "My name");
    assert_eq!(detail["meta"]["auto_title"], false);
}

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
    // The overlay's attach guard requires a string `version`; pin it here
    // so a server-side rename/retype can't silently break cross-crate attach.
    assert!(
        health["version"].is_string(),
        "health must carry a string version"
    );

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
