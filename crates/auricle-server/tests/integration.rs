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
use tokio::task::JoinHandle;
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

fn fake_engine(cfg: Config, name: &str) -> Arc<Engine> {
    Engine::new(EngineOptions {
        cfg,
        data_root: temp_root(name),
        provider_override: Some(Arc::new(FakeProvider {
            chunks_seen: Arc::new(AtomicU64::new(0)),
        })),
    })
    .unwrap()
}

async fn spawn_server_with(cfg: Config, name: &str) -> (String, Arc<Engine>) {
    let engine = fake_engine(cfg, name);
    let app = build_router(engine.clone(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), engine)
}

/// `engine` run the whole way `serve` runs one, not just its router. The
/// handle resolves when the daemon would exit.
async fn run_daemon(engine: &Arc<Engine>) -> (String, JoinHandle<Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let daemon = tokio::spawn(auricle_server::serve_engine(engine.clone(), listener, None));
    (format!("http://{addr}"), daemon)
}

async fn start_fixture_session(engine: &Arc<Engine>, title: Option<&str>) -> String {
    engine
        .start_session(StartParams {
            title: title.map(str::to_string),
            stt_provider: None,
            mic_device: None,
            loopback_device: None,
            audio: Some(AudioSource::WavFile {
                path: fixture_path(),
                channel: ChannelId::Loopback,
            }),
        })
        .await
        .unwrap()
}

async fn stop_and_wait(engine: &Arc<Engine>, id: &str) {
    engine.stop_session(id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.active_session().is_some() {
        assert!(std::time::Instant::now() < deadline, "stop did not finish");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// An ephemeral port for `serve`, which binds for itself.
fn free_loopback_addr() -> String {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    probe.local_addr().unwrap().to_string()
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

    // The transcript went to the LLM, so the ledger has to say so, classified
    // the way summaries are (the mock endpoint is on loopback: local).
    let ledger: serde_json::Value = client
        .get(format!("{base}/api/v1/egress?session={id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = ledger["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "title")
        .expect("auto-titling records an egress row");
    assert_eq!(row["destination"], "local");
    assert_eq!(row["provider"], "ollama");
    assert_eq!(row["host"], "127.0.0.1");
    assert_eq!(row["session_id"], id.as_str());
    assert_eq!(row["detail"], "auto-title");
    assert!(row["items"].as_i64().unwrap() > 0, "{row}");

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
async fn rename_during_auto_titling_is_not_overwritten() {
    // The LLM answers slowly, the way a real one does: the user renames the
    // session while the generator is still waiting on it.
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(2))
                .set_body_json(serde_json::json!({
                    "id": "c", "object": "chat.completion", "model": "m",
                    "choices": [{"index": 0, "finish_reason": "stop",
                        "message": {"role": "assistant", "content": "Generated Title Here"}}]
                })),
        )
        .mount(&llm)
        .await;
    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri();
    let (base, engine) = spawn_server_with(cfg, "autotitle-race").await;
    let client = reqwest::Client::new();

    let id = start_fixture_session(&engine, None).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    stop_and_wait(&engine, &id).await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while llm.received_requests().await.unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline, "no title request");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let resp = client
        .patch(format!("{base}/api/v1/sessions/{id}"))
        .json(&serde_json::json!({"title": "My name"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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
async fn shutdown_route_stops_the_active_session_cleanly() {
    let (base, engine) = spawn_server("shutdown-session").await;
    let client = reqwest::Client::new();
    let id = start_fixture_session(&engine, Some("shutdown run")).await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let resp = client
        .post(format!("{base}/api/v1/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"status": "shutting_down"}));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.active_session().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "session never stopped"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let session = engine.store().get_session(&id).unwrap().unwrap();
    assert!(session.ended_at.is_some(), "stopped, not abandoned");
    assert!(
        session.meta.get("interrupted").is_none(),
        "a requested shutdown is not an interruption"
    );
}

#[tokio::test]
async fn shutdown_route_ends_serve() {
    let root = temp_root("shutdown-serve");
    let addr = free_loopback_addr();
    let server = tokio::spawn(auricle_server::serve(
        Config::default(),
        root,
        Some(addr.clone()),
    ));
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while client
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .is_err()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "daemon never answered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let resp = client
        .post(format!("{base}/api/v1/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        202,
        "the caller gets its answer before the exit"
    );

    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("serve must return after a shutdown request")
        .unwrap();
    assert!(ended.is_ok(), "{ended:?}");
}

/// An LLM that takes `delay` to name a meeting "Generated Title Here".
async fn slow_title_llm(delay: std::time::Duration) -> MockServer {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(delay).set_body_json(
            serde_json::json!({
                "id": "c", "object": "chat.completion", "model": "m",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "Generated Title Here"}}]
            }),
        ))
        .mount(&llm)
        .await;
    llm
}

async fn request_shutdown(base: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
}

#[tokio::test]
async fn shutdown_lets_a_pending_auto_title_land() {
    let llm = slow_title_llm(std::time::Duration::from_secs(1)).await;
    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri();
    let engine = fake_engine(cfg, "shutdown-title");
    let (base, daemon) = run_daemon(&engine).await;

    let id = start_fixture_session(&engine, None).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    request_shutdown(&base).await;

    tokio::time::timeout(std::time::Duration::from_secs(15), daemon)
        .await
        .expect("the daemon must exit")
        .unwrap()
        .unwrap();
    let session = engine.store().get_session(&id).unwrap().unwrap();
    assert!(session.ended_at.is_some(), "stopped, not abandoned");
    assert_eq!(session.title, "Generated Title Here");
}

#[tokio::test]
async fn a_slow_auto_title_cannot_hold_up_shutdown() {
    // Inside the title's own timeout, so only shutdown's bound can cut it off.
    let llm = slow_title_llm(std::time::Duration::from_secs(18)).await;
    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri();
    let engine = fake_engine(cfg, "shutdown-title-slow");
    let (base, daemon) = run_daemon(&engine).await;

    let id = start_fixture_session(&engine, None).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    request_shutdown(&base).await;

    tokio::time::timeout(std::time::Duration::from_secs(15), daemon)
        .await
        .expect("the wait for a title must be bounded")
        .unwrap()
        .unwrap();
    let session = engine.store().get_session(&id).unwrap().unwrap();
    assert!(session.ended_at.is_some(), "stopped, not abandoned");
    assert_eq!(session.title, "Untitled session");
}

#[tokio::test]
async fn a_second_daemon_on_another_port_leaves_the_live_session_alone() {
    let (_base, first) = spawn_server("second-daemon").await;
    first
        .store()
        .create_session("live", "t", 1_700_000_000, "fake", &serde_json::json!({}))
        .unwrap();

    // Bounded: a second daemon that got through would serve until killed.
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        auricle_server::serve(
            Config::default(),
            first.data_root().to_path_buf(),
            Some(free_loopback_addr()),
        ),
    )
    .await
    .expect("a second daemon must be refused, not run");

    let error = second.expect_err("the data root is taken");
    assert!(error.to_string().contains("already running"), "{error}");
    let session = first.store().get_session("live").unwrap().unwrap();
    assert_eq!(session.ended_at, None, "still recording");
    assert!(
        session.meta.get("interrupted").is_none(),
        "{:?}",
        session.meta
    );
}

#[tokio::test]
async fn failed_second_serve_leaves_the_live_session_alone() {
    let root = temp_root("second-serve");
    // The first daemon: its session is open in the database and it holds
    // the port.
    let store = auricle_server::Store::open(&root.join("auricle.db")).unwrap();
    store
        .create_session("live", "t", 1_700_000_000, "fake", &serde_json::json!({}))
        .unwrap();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = held.local_addr().unwrap().to_string();

    let second = auricle_server::serve(Config::default(), root, Some(addr)).await;
    assert!(second.is_err(), "the port is taken");

    let session = store.get_session("live").unwrap().unwrap();
    assert_eq!(session.ended_at, None, "still recording");
    assert!(
        session.meta.get("interrupted").is_none(),
        "a daemon that never started must not run crash recovery"
    );
}

#[tokio::test]
async fn templates_come_from_the_engines_own_data_root() {
    let (base, engine) = spawn_server("templates").await;
    let dir = engine.data_root().join("templates");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("root-probe.md"), "Summarize this.").unwrap();

    let providers: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/v1/providers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        providers["templates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "root-probe"),
        "{providers}"
    );
}

#[tokio::test]
async fn providers_default_follows_the_default_provider_setting() {
    let (base, _engine) = spawn_server("providers-default").await;
    let client = reqwest::Client::new();
    let defaults = || async {
        let providers: serde_json::Value = client
            .get(format!("{base}/api/v1/providers"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        providers["providers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["default"] == true)
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };

    assert_eq!(defaults().await, [Config::default().stt.provider]);

    client
        .put(format!("{base}/api/v1/settings"))
        .json(&serde_json::json!({"default_provider": "deepgram"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        defaults().await,
        ["deepgram"],
        "as a start would resolve it"
    );
}

async fn get_json(base: &str, path: &str) -> serde_json::Value {
    let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
    assert_eq!(resp.status(), 200, "{path}");
    resp.json().await.unwrap()
}

fn ids(rows: &serde_json::Value) -> Vec<&str> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn sessions_page_on_request_and_always_say_how_many_there_are() {
    let (base, engine) = spawn_server("sessions-page").await;
    let store = engine.store();
    for (id, title, started) in [
        ("a", "Standup", 100),
        ("b", "Standup", 200),
        ("c", "Retro", 300),
    ] {
        store
            .create_session(id, title, started, "fake", &serde_json::json!({}))
            .unwrap();
    }

    // No params: everything, as it always was, with the count alongside.
    let all = get_json(&base, "/api/v1/sessions").await;
    assert_eq!(ids(&all["sessions"]), ["c", "b", "a"]);
    assert_eq!(all["total"], 3);

    let page = get_json(&base, "/api/v1/sessions?limit=2").await;
    assert_eq!(ids(&page["sessions"]), ["c", "b"]);
    assert_eq!(page["total"], 3);
    let page = get_json(&base, "/api/v1/sessions?limit=2&offset=2").await;
    assert_eq!(ids(&page["sessions"]), ["a"]);
    assert_eq!(page["total"], 3);
    let page = get_json(&base, "/api/v1/sessions?offset=1").await;
    assert_eq!(ids(&page["sessions"]), ["b", "a"]);
    let page = get_json(&base, "/api/v1/sessions?offset=9").await;
    assert!(page["sessions"].as_array().unwrap().is_empty());
    assert_eq!(page["total"], 3);

    // A search pages too, and counts what it matched, not every session.
    let found = get_json(&base, "/api/v1/sessions?q=standup").await;
    assert_eq!(ids(&found["sessions"]), ["b", "a"]);
    assert_eq!(found["total"], 2);
    let found = get_json(&base, "/api/v1/sessions?q=standup&limit=1&offset=1").await;
    assert_eq!(ids(&found["sessions"]), ["a"]);
    assert_eq!(found["total"], 2);
}

#[tokio::test]
async fn search_finds_the_moment_not_just_the_meeting() {
    let (base, engine) = spawn_server("line-search").await;
    let store = engine.store();
    for (id, title, started) in [("old", "Standup", 100), ("new", "Retro", 200)] {
        store
            .create_session(id, title, started, "fake", &serde_json::json!({}))
            .unwrap();
    }
    for (id, channel, speaker, at, text) in [
        (
            "old",
            ChannelId::Loopback,
            "Them",
            1_000,
            "the budget is approved",
        ),
        (
            "new",
            ChannelId::Mic,
            "You",
            5_000,
            "budget review on Thursday",
        ),
        (
            "new",
            ChannelId::Loopback,
            "Them",
            9_000,
            "nothing to see here",
        ),
        (
            "new",
            ChannelId::Loopback,
            "Them",
            7_000,
            "the Budget slipped",
        ),
    ] {
        store
            .insert_segment(id, channel, speaker, at, at + 500, text, "fake")
            .unwrap();
    }

    let found = get_json(&base, "/api/v1/search?q=budget").await;
    assert_eq!(found["total"], 3);
    let matches = found["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 3);
    // Most recent meeting first, then in order through it.
    assert_eq!(matches[0]["session_id"], "new");
    assert_eq!(matches[0]["session_title"], "Retro");
    assert_eq!(matches[0]["speaker"], "You");
    assert_eq!(matches[0]["t_start_ms"], 5_000);
    assert_eq!(matches[0]["t_end_ms"], 5_500);
    assert_eq!(matches[0]["text"], "budget review on Thursday");
    assert_eq!(matches[1]["t_start_ms"], 7_000);
    assert_eq!(matches[2]["session_id"], "old");
    assert_eq!(matches[2]["started_at"], 100);

    let page = get_json(&base, "/api/v1/search?q=budget&limit=1&offset=1").await;
    assert_eq!(page["total"], 3, "total counts every match, not the page");
    assert_eq!(page["matches"].as_array().unwrap().len(), 1);
    assert_eq!(page["matches"][0]["t_start_ms"], 7_000);

    // Wildcards stay literal, and no query is no results, not everything.
    let none = get_json(&base, "/api/v1/search?q=%25").await;
    assert_eq!(none["total"], 0);
    let none = get_json(&base, "/api/v1/search").await;
    assert_eq!(none["total"], 0);
    assert!(none["matches"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn egress_pages_by_offset_and_keeps_whole_ledger_totals() {
    let (base, engine) = spawn_server("egress-offset").await;
    let store = engine.store();
    for id in ["a", "b", "c"] {
        store
            .create_session(id, "t", 100, "fake", &serde_json::json!({}))
            .unwrap();
    }
    let db = rusqlite::Connection::open(engine.data_root().join("auricle.db")).unwrap();
    db.execute_batch(
        "INSERT INTO egress(ts, session_id, destination, provider, kind)
         VALUES (1, 'a', 'local', 'p', 'audio'),
                (2, 'b', 'local', 'p', 'audio'),
                (3, 'a', 'local', 'p', 'audio');",
    )
    .unwrap();

    let ts = |ledger: &serde_json::Value| {
        ledger["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["ts"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    let ledger = get_json(&base, "/api/v1/egress?limit=1&offset=1").await;
    assert_eq!(ts(&ledger), [2]);
    let ledger = get_json(&base, "/api/v1/egress?offset=2").await;
    assert_eq!(ts(&ledger), [1]);
    let ledger = get_json(&base, "/api/v1/egress?session=a&limit=1&offset=1").await;
    assert_eq!(ts(&ledger), [1]);
    assert!(ledger["totals"].is_array(), "{ledger}");
}

#[tokio::test]
async fn retained_asks_are_listed_and_deleted_one_at_a_time() {
    let (base, engine) = spawn_server("asks").await;
    let client = reqwest::Client::new();
    let store = engine.store();
    store
        .create_session("s1", "t", 100, "fake", &serde_json::json!({}))
        .unwrap();
    let first = store
        .insert_ask(Some("s1"), "q1", Some("ocr"), "a1", "ollama", 10)
        .unwrap();
    let second = store
        .insert_ask(None, "q2", None, "a2", "groq", 20)
        .unwrap();

    let all = get_json(&base, "/api/v1/asks").await;
    assert_eq!(
        all,
        serde_json::json!({"asks": [
            {"id": second, "session_id": null, "question": "q2", "screen_context": null,
             "answer": "a2", "provider": "groq", "created_at": 20},
            {"id": first, "session_id": "s1", "question": "q1", "screen_context": "ocr",
             "answer": "a1", "provider": "ollama", "created_at": 10},
        ]})
    );
    let scoped = get_json(&base, "/api/v1/asks?session=s1").await;
    assert_eq!(scoped["asks"].as_array().unwrap().len(), 1);
    assert_eq!(scoped["asks"][0]["id"], first);
    let page = get_json(&base, "/api/v1/asks?limit=1&offset=1").await;
    assert_eq!(page["asks"].as_array().unwrap().len(), 1);
    assert_eq!(page["asks"][0]["id"], first);

    let url = format!("{base}/api/v1/asks/{first}");
    let deleted = client.delete(&url).send().await.unwrap();
    assert_eq!(deleted.status(), 204);
    assert!(deleted.bytes().await.unwrap().is_empty());
    assert_eq!(store.asks_count().unwrap(), 1);
    assert_eq!(client.delete(&url).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn backup_writes_a_working_copy_under_the_data_root() {
    let (base, engine) = spawn_server("backup").await;
    let client = reqwest::Client::new();
    engine
        .store()
        .create_session("s1", "kept", 100, "fake", &serde_json::json!({}))
        .unwrap();

    let resp = client
        .post(format!("{base}/api/v1/backup"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let path = std::path::PathBuf::from(body["path"].as_str().unwrap());
    assert_eq!(path.parent().unwrap(), engine.data_root().join("backups"));
    let name = path.file_name().unwrap().to_str().unwrap();
    let stamp = name
        .strip_prefix("auricle-")
        .and_then(|n| n.strip_suffix(".db"))
        .unwrap_or_else(|| panic!("unexpected name {name}"));
    assert!(stamp.parse::<i64>().is_ok(), "{name}");

    let copy = auricle_server::Store::open(&path).unwrap();
    assert_eq!(copy.get_session("s1").unwrap().unwrap().title, "kept");
}

#[tokio::test]
async fn a_second_backup_in_the_same_second_is_a_conflict_not_an_overwrite() {
    let (base, engine) = spawn_server("backup-clash").await;
    let dir = engine.data_root().join("backups");
    std::fs::create_dir_all(&dir).unwrap();
    // Every name the request could land on, whichever second it runs in.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for t in now..now + 30 {
        std::fs::write(dir.join(format!("auricle-{t}.db")), b"earlier backup").unwrap();
    }

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/backup"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let kept = std::fs::read(dir.join(format!("auricle-{now}.db"))).unwrap();
    assert_eq!(kept, b"earlier backup");
}

const DAY: i64 = 86_400;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A finished session that ended `days_ago`, with a retained audio directory.
fn seed_ended_session(engine: &Engine, id: &str, days_ago: i64) -> std::path::PathBuf {
    let store = engine.store();
    let ended = unix_now() - days_ago * DAY;
    store
        .create_session(id, "t", ended - 60, "fake", &serde_json::json!({}))
        .unwrap();
    store.end_session(id, ended).unwrap();
    let audio_dir = engine.data_root().join("sessions").join(id);
    std::fs::create_dir_all(&audio_dir).unwrap();
    std::fs::write(audio_dir.join("mic_16k.wav"), b"RIFF").unwrap();
    audio_dir
}

#[tokio::test]
async fn retention_takes_old_sessions_and_leaves_recent_and_active_ones() {
    let (base, engine) = spawn_server("retention").await;
    let store = engine.store();

    let old_audio = seed_ended_session(&engine, "old", 40);
    store
        .insert_segment("old", ChannelId::Mic, "You", 0, 1, "old words", "fake")
        .unwrap();
    store
        .insert_ask(Some("old"), "q", Some("ocr"), "a", "ollama", 1)
        .unwrap();

    // Dated as old as "old" with an end on record, and recorded before
    // "recent", so that only the check for the running session can save it.
    let active = start_fixture_session(&engine, Some("live")).await;
    let db = rusqlite::Connection::open(engine.data_root().join("auricle.db")).unwrap();
    db.execute(
        "UPDATE sessions SET started_at = ?1, ended_at = ?1 WHERE id = ?2",
        rusqlite::params![unix_now() - 40 * DAY, active],
    )
    .unwrap();
    let recent_audio = seed_ended_session(&engine, "recent", 2);

    let resp = reqwest::Client::new()
        .put(format!("{base}/api/v1/settings"))
        .json(&serde_json::json!({"retention_days": 30}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        get_json(&base, "/api/v1/settings").await["retention_days"],
        30
    );

    assert_eq!(engine.sweep_retention().unwrap().deleted, 1);

    assert!(store.get_session("old").unwrap().is_none());
    assert!(store.get_segments("old").unwrap().is_empty());
    assert_eq!(store.asks_count().unwrap(), 0);
    assert!(!old_audio.exists(), "retained audio goes with the rows");

    assert!(store.get_session("recent").unwrap().is_some());
    assert!(recent_audio.exists());

    assert!(store.get_session(&active).unwrap().is_some());
    assert_eq!(engine.active_session(), Some(active.clone()));
    stop_and_wait(&engine, &active).await;
}

#[tokio::test]
async fn retention_is_off_unless_set_to_a_positive_whole_number() {
    let (base, engine) = spawn_server("retention-off").await;
    let client = reqwest::Client::new();
    let store = engine.store();
    let audio = seed_ended_session(&engine, "old", 4000);

    assert_eq!(
        engine.sweep_retention().unwrap().deleted,
        0,
        "off by default"
    );

    let put = |value: serde_json::Value| {
        let (client, url) = (client.clone(), format!("{base}/api/v1/settings"));
        async move {
            let body = serde_json::json!({"retention_days": value});
            client.put(url).json(&body).send().await.unwrap().status()
        }
    };
    assert_eq!(put(serde_json::json!(0)).await, 200);
    assert_eq!(engine.sweep_retention().unwrap().deleted, 0);

    // Accepting any of these would claim a retention that never happens.
    for bad in [
        serde_json::json!(-1),
        serde_json::json!("30"),
        serde_json::json!(1.5),
        serde_json::Value::Null,
    ] {
        assert_eq!(put(bad.clone()).await, 400, "{bad}");
    }
    assert_eq!(
        get_json(&base, "/api/v1/settings").await["retention_days"],
        0
    );

    // Should one get in by another route: a negative cutoff is in the future,
    // and a sweep that honoured it would delete every meeting there is.
    store
        .set_setting("retention_days", &serde_json::json!(-1))
        .unwrap();
    assert_eq!(engine.sweep_retention().unwrap().deleted, 0);
    assert!(store.get_session("old").unwrap().is_some());
    assert!(audio.exists());

    // Larger than any calendar: nothing is that old, and nothing overflows.
    store
        .set_setting("retention_days", &serde_json::json!(i64::MAX))
        .unwrap();
    assert_eq!(engine.sweep_retention().unwrap().deleted, 0);
    // Past the hold that a 4,000-day-old library starts, so the cutoff is
    // really computed.
    let sweep = engine.sweep_retention_at(unix_now() + 8 * DAY).unwrap();
    assert_eq!((sweep.deleted, sweep.notice), (0, None));
}

#[tokio::test]
async fn the_daemon_sweeps_retention_when_it_starts() {
    let engine = fake_engine(Config::default(), "retention-startup");
    let store = engine.store();
    let audio = seed_ended_session(&engine, "old", 40);
    seed_ended_session(&engine, "recent", 2);
    store
        .set_setting("retention_days", &serde_json::json!(30))
        .unwrap();

    let (base, daemon) = run_daemon(&engine).await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while store.get_session("old").unwrap().is_some() {
        assert!(std::time::Instant::now() < deadline, "no startup sweep");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!audio.exists());
    assert!(store.get_session("recent").unwrap().is_some());

    request_shutdown(&base).await;
    daemon.await.unwrap().unwrap();
}

// ---------- a wrong clock must not cost anyone their meetings ----------

/// An engine holding `(id, days_ago)` sessions, recorded in that order, with
/// 30-day retention on.
fn retention_library(name: &str, sessions: &[(&str, i64)]) -> Arc<Engine> {
    let engine = fake_engine(Config::default(), name);
    for (id, days_ago) in sessions {
        seed_ended_session(&engine, id, *days_ago);
    }
    engine
        .store()
        .set_setting("retention_days", &serde_json::json!(30))
        .unwrap();
    engine
}

fn session_ids(engine: &Engine) -> Vec<String> {
    let mut ids: Vec<_> = engine
        .store()
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_clock_far_ahead_deletes_nothing_and_says_why() {
    let engine = retention_library("clock-ahead", &[("last-week", 7), ("yesterday", 1)]);
    let audio = engine.data_root().join("sessions").join("yesterday");
    // The sweep the daemon ran yesterday, on a clock that was fine.
    let now = unix_now();
    engine.sweep_retention_at(now - DAY).unwrap();

    let far_ahead = now + 10 * 365 * DAY;
    let sweep = engine.sweep_retention_at(far_ahead).unwrap();

    assert_eq!(sweep.deleted, 0);
    let notice = sweep.notice.expect("a refused sweep must be logged");
    assert!(notice.contains("past the last sweep"), "{notice}");
    assert_eq!(session_ids(&engine), ["last-week", "yesterday"]);
    assert!(audio.exists());

    // A crash loop is not a week going by: however often it restarts, the
    // clock has to have held for real.
    for hour in 1..=20 {
        let sweep = engine.sweep_retention_at(far_ahead + hour * 3_600).unwrap();
        assert_eq!(sweep.deleted, 0, "restart after {hour} h");
    }
    assert_eq!(session_ids(&engine), ["last-week", "yesterday"]);
}

#[tokio::test]
async fn a_clock_far_ahead_on_the_very_first_sweep_deletes_nothing() {
    let engine = retention_library("clock-ahead-first", &[("last-week", 7), ("yesterday", 1)]);

    let sweep = engine
        .sweep_retention_at(unix_now() + 10 * 365 * DAY)
        .unwrap();

    assert_eq!(sweep.deleted, 0);
    let notice = sweep.notice.expect("a refused sweep must be logged");
    assert!(notice.contains("past the newest meeting"), "{notice}");
    assert_eq!(session_ids(&engine), ["last-week", "yesterday"]);
}

#[tokio::test]
async fn a_jump_too_small_to_reach_the_newest_meeting_is_still_refused() {
    // 28 days on, with 30-day retention: yesterday's meeting would survive,
    // and everything before it would go.
    let engine = retention_library(
        "clock-ahead-small",
        &[("three-weeks", 21), ("last-week", 7), ("yesterday", 1)],
    );
    let now = unix_now();
    engine.sweep_retention_at(now).unwrap();

    let sweep = engine.sweep_retention_at(now + 28 * DAY).unwrap();

    assert_eq!(sweep.deleted, 0);
    assert!(sweep.notice.is_some());
    assert_eq!(session_ids(&engine).len(), 3);
}

#[tokio::test]
async fn correcting_the_clock_ends_the_pause_at_once() {
    let engine = retention_library("clock-corrected", &[("old", 40), ("yesterday", 1)]);
    let now = unix_now();
    engine.sweep_retention_at(now - DAY).unwrap();
    assert_eq!(session_ids(&engine), ["yesterday"], "the sweep before");
    seed_ended_session(&engine, "today", 0);

    assert!(engine
        .sweep_retention_at(now + 10 * 365 * DAY)
        .unwrap()
        .notice
        .is_some());
    let sweep = engine.sweep_retention_at(now + 3_600).unwrap();

    assert!(sweep.notice.is_none(), "{:?}", sweep.notice);
    assert_eq!(session_ids(&engine), ["today", "yesterday"]);
}

#[tokio::test]
async fn retention_runs_to_the_last_meeting_on_a_steady_clock() {
    let engine = retention_library("clock-steady", &[("older", 20), ("newer", 2)]);
    let now = unix_now();

    // The daily sweeps are the evidence, so nothing needs a later meeting to
    // vouch for it: the newest goes on time too.
    for day in 0..=29 {
        let sweep = engine.sweep_retention_at(now + day * DAY).unwrap();
        assert!(sweep.notice.is_none(), "day {day}: {:?}", sweep.notice);
        let expected: &[&str] = match day {
            0..=10 => &["newer", "older"],
            11..=28 => &["newer"],
            _ => &[],
        };
        assert_eq!(session_ids(&engine), expected, "day {day}");
    }
}

#[tokio::test]
async fn a_real_gap_only_delays_retention() {
    let engine = retention_library("clock-gap", &[("old", 2)]);
    let audio = engine.data_root().join("sessions").join("old");
    let now = unix_now();
    engine.sweep_retention_at(now).unwrap();

    // Sixty days with the daemon off look exactly like a clock sixty days
    // ahead, until the clock has held for a week.
    let back = now + 60 * DAY;
    assert_eq!(engine.sweep_retention_at(back).unwrap().deleted, 0);
    assert_eq!(
        engine.sweep_retention_at(back + 6 * DAY).unwrap().deleted,
        0
    );
    let sweep = engine.sweep_retention_at(back + 7 * DAY).unwrap();

    assert_eq!(sweep.deleted, 1);
    assert!(sweep.notice.is_none(), "{:?}", sweep.notice);
    assert!(!audio.exists());
}

#[tokio::test]
async fn a_daemon_run_once_a_month_still_gets_retention() {
    let engine = retention_library("clock-monthly", &[("old", 2)]);
    let now = unix_now();
    engine.sweep_retention_at(now).unwrap();

    // Every run is a jump, and the first one's hold has long passed by the
    // second: a rule that needs a week of uptime would never let this through.
    assert_eq!(
        engine.sweep_retention_at(now + 30 * DAY).unwrap().deleted,
        0
    );
    assert_eq!(
        engine.sweep_retention_at(now + 60 * DAY).unwrap().deleted,
        1
    );
}

#[tokio::test]
async fn a_meeting_recorded_behind_the_clock_survives_its_correction() {
    // "behind" was recorded after "yesterday", on a clock two months slow.
    let engine = retention_library("clock-behind", &[("yesterday", 1), ("behind", 60)]);
    let now = unix_now();

    assert_eq!(engine.sweep_retention_at(now).unwrap().deleted, 0);
    assert_eq!(session_ids(&engine), ["behind", "yesterday"]);

    // It does expire: with the meeting vouching for it, not on its own date.
    for day in 1..=30 {
        engine.sweep_retention_at(now + day * DAY).unwrap();
    }
    assert!(session_ids(&engine).is_empty());
}

#[tokio::test]
async fn a_clock_running_behind_deletes_nothing() {
    let engine = retention_library("clock-back", &[("old", 40), ("yesterday", 1)]);

    let sweep = engine
        .sweep_retention_at(unix_now() - 20 * 365 * DAY)
        .unwrap();

    assert_eq!(sweep.deleted, 0);
    assert_eq!(session_ids(&engine), ["old", "yesterday"]);
}

#[tokio::test]
async fn an_unreadable_sweep_record_counts_as_no_record() {
    let engine = retention_library("clock-garbled", &[("yesterday", 1)]);
    engine
        .store()
        .set_setting("retention_clock", &serde_json::json!("not a record"))
        .unwrap();

    let sweep = engine
        .sweep_retention_at(unix_now() + 10 * 365 * DAY)
        .unwrap();

    assert_eq!(sweep.deleted, 0);
    assert!(sweep.notice.is_some());
}

// ---------- raw audio must not outlive a delete ----------

fn open_engine(data_root: std::path::PathBuf) -> Arc<Engine> {
    Engine::new(EngineOptions {
        cfg: Config::default(),
        data_root,
        provider_override: None,
    })
    .unwrap()
}

fn seed_audio_dir(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("mic_16k.wav"), b"RIFF").unwrap();
}

/// Audio with no row is what restoring an older backup leaves behind. Deletes
/// remove audio before rows, so nothing else makes it now, and an automatic,
/// irreversible delete of recordings isn't worth the tidiness.
#[tokio::test]
async fn startup_never_removes_audio_on_its_own() {
    let root = temp_root("orphans");
    let store = open_engine(root.clone()).store();
    store
        .create_session("s18c0ffee", "kept", 100, "fake", &serde_json::json!({}))
        .unwrap();
    drop(store); // and with it the engine's last handle on the data root
    let dir = root.join("sessions").join("s18dead01");
    seed_audio_dir(&dir);

    let _engine = open_engine(root.clone());

    assert!(dir.join("mic_16k.wav").exists());
}

/// Put a file where the session's audio directory goes, which is enough to
/// make removing it fail.
fn block_audio_removal(engine: &Engine, id: &str) -> std::path::PathBuf {
    let dir = engine.data_root().join("sessions").join(id);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, b"in the way").unwrap();
    dir
}

#[tokio::test]
async fn a_delete_that_cannot_remove_the_audio_says_so_and_can_be_retried() {
    let (base, engine) = spawn_server("delete-stuck").await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/v1/sessions/stuck");
    engine
        .store()
        .create_session("stuck", "t", 1_700_000_000, "fake", &serde_json::json!({}))
        .unwrap();
    let blocker = block_audio_removal(&engine, "stuck");

    let resp = client.delete(&url).send().await.unwrap();

    assert_eq!(resp.status(), 500);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_ne!(body["deleted"], true);
    assert!(body["error"].as_str().unwrap().contains("audio"), "{body}");
    assert!(
        engine.store().get_session("stuck").unwrap().is_some(),
        "a retry needs something to delete"
    );

    std::fs::remove_file(&blocker).unwrap();
    let resp = client.delete(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["deleted"], true);
    assert!(engine.store().get_session("stuck").unwrap().is_none());
}

#[tokio::test]
async fn an_unknown_id_touches_no_audio() {
    let (base, engine) = spawn_server("delete-unknown").await;
    let dir = engine.data_root().join("sessions").join("s18dead01");
    seed_audio_dir(&dir);

    let resp = reqwest::Client::new()
        .delete(format!("{base}/api/v1/sessions/s18dead01"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    assert!(dir.join("mic_16k.wav").exists());
}

#[tokio::test]
async fn retention_reports_audio_it_could_not_remove_and_tries_again() {
    let engine = retention_library(
        "retention-stuck",
        &[("stuck", 40), ("free", 35), ("new", 1)],
    );
    let blocker = block_audio_removal(&engine, "stuck");

    let sweep = engine.sweep_retention().unwrap();

    assert_eq!(sweep.deleted, 1, "one stuck meeting must not stop the rest");
    let notice = sweep.notice.expect("a failed removal must be logged");
    assert!(notice.contains("1 "), "{notice}");
    assert_eq!(session_ids(&engine), ["new", "stuck"]);

    std::fs::remove_file(&blocker).unwrap();
    assert_eq!(engine.sweep_retention().unwrap().deleted, 1);
    assert_eq!(session_ids(&engine), ["new"]);
}

#[tokio::test]
async fn delete_removes_the_rows_and_the_retained_audio() {
    let (base, engine) = spawn_server("delete").await;
    let client = reqwest::Client::new();
    engine
        .store()
        .create_session("gone", "t", 1_700_000_000, "fake", &serde_json::json!({}))
        .unwrap();
    let audio_dir = engine.data_root().join("sessions").join("gone");
    std::fs::create_dir_all(&audio_dir).unwrap();

    let resp = client
        .delete(format!("{base}/api/v1/sessions/gone"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(engine.store().get_session("gone").unwrap().is_none());
    assert!(!audio_dir.exists(), "retained audio goes with the rows");

    let again = client
        .delete(format!("{base}/api/v1/sessions/gone"))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 404);
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
    // The overlay checks this against its engine.pid record: an image name
    // alone would let a recycled pid on any auricle.exe count as its engine.
    assert_eq!(health["pid"], std::process::id());
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
