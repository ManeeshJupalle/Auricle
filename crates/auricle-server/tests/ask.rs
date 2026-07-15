//! POST /api/v1/ask end to end: SSE streaming + /ws/live mirroring against
//! a wiremock LLM replaying SSE bodies, a scripted ScreenReader, and (for
//! transcript context) a real fixture-WAV session through the fake STT
//! provider. Also proves the retain_context=false persistence guarantee.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, ChunkKind, Config, Result, Segment, SttEvent};
use auricle_server::{build_router_with_reader, AudioSource, Engine, EngineOptions, StartParams};
use auricle_stt::{SessionCfg, SttKind, SttProvider, SttSession};
use auricle_vision::{ScreenContext, ScreenReader, VisionError};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------- fake STT provider (as in integration.rs) ----------

struct FakeProvider;

struct FakeSession {
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
            finals: Vec::new(),
            tx,
            rx,
        }))
    }
}

#[async_trait]
impl SttSession for FakeSession {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
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

// ---------- scripted screen reader (as in peek.rs) ----------

struct MockReader {
    script: Mutex<VecDeque<std::result::Result<ScreenContext, VisionError>>>,
    excludes_seen: Mutex<Vec<Option<isize>>>,
}

impl MockReader {
    fn with_script(
        script: Vec<std::result::Result<ScreenContext, VisionError>>,
    ) -> Arc<MockReader> {
        Arc::new(MockReader {
            script: Mutex::new(script.into()),
            excludes_seen: Mutex::new(Vec::new()),
        })
    }
}

impl ScreenReader for MockReader {
    fn capture_active_window(
        &self,
        exclude_hwnd: Option<isize>,
    ) -> std::result::Result<ScreenContext, VisionError> {
        self.excludes_seen.lock().unwrap().push(exclude_hwnd);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("unscripted capture call")
    }
}

fn jira_screen() -> ScreenContext {
    ScreenContext {
        window_title: "Sprint 12 Board - Jira".to_string(),
        app_name: "chrome".to_string(),
        text: "AUR-42 Fix chunker overlap dedup\nAUR-43 Ship copilot".to_string(),
        captured_at: 1_784_000_000_000,
        ocr_ms: 142,
    }
}

// ---------- SSE fixtures for the mock LLM ----------

/// Hand-rolled SSE body in the exact captured shape (Groq-style): empty
/// first delta, two content deltas, finish chunk, usage frame, DONE.
const MOCK_SSE: &str = concat!(
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"The answer \"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"is 42.\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
    "data: [DONE]\n\n",
);

async fn mock_llm(body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
        )
        .mount(&server)
        .await;
    server
}

// ---------- server bootstrap ----------

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("auricle-ask-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn_server(cfg: Config, name: &str, reader: Arc<MockReader>) -> (String, Arc<Engine>) {
    let engine = Engine::new(EngineOptions {
        cfg,
        data_root: temp_root(name),
        provider_override: Some(Arc::new(FakeProvider)),
    })
    .unwrap();
    let app = build_router_with_reader(engine.clone(), None, reader);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), engine)
}

fn cfg_with_llm(llm_uri: &str) -> Config {
    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm_uri.to_string();
    cfg.llm.ollama.model = "mock-model".to_string();
    cfg
}

/// Read an ask's SSE response to completion: parse every `data:` JSON
/// event until answer_done/ask_error (keep-alive comments skipped).
async fn read_sse(resp: reqwest::Response) -> Vec<serde_json::Value> {
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let mut events = Vec::new();
    let mut buf = String::new();
    let mut body = resp.bytes_stream();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    'read: while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, body.next()).await {
        buf.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim_end();
            if let Some(payload) = line.strip_prefix("data: ") {
                let v: serde_json::Value = serde_json::from_str(payload).unwrap();
                let t = v["type"].as_str().unwrap_or_default().to_string();
                events.push(v);
                if t == "answer_done" || t == "ask_error" {
                    break 'read;
                }
            }
        }
    }
    events
}

fn spawn_ws_collector(base: &str) -> Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> {
    let ws_url = base.replace("http://", "ws://") + "/ws/live";
    let events: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let events_clone = events.clone();
    tokio::spawn(async move {
        let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
        let (_, mut rx) = ws.split();
        while let Some(Ok(msg)) = rx.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                events_clone
                    .lock()
                    .await
                    .push(serde_json::from_str(&t).unwrap());
            }
        }
    });
    events
}

async fn wait_for_ws_event(
    events: &Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    event_type: &str,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        {
            let evs = events.lock().await;
            if let Some(e) = evs.iter().find(|e| e["type"] == event_type) {
                return e.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no {event_type} on /ws/live within 10 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

// ---------- tests ----------

#[tokio::test]
async fn ask_streams_sse_mirrors_ws_and_carries_screen_context() {
    let llm = mock_llm(MOCK_SSE).await;
    let reader = MockReader::with_script(vec![Ok(jira_screen())]);
    let (base, _engine) = spawn_server(cfg_with_llm(&llm.uri()), "sse", reader.clone()).await;
    let ws_events = spawn_ws_collector(&base);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await; // WS attach

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/ask"))
        .json(&serde_json::json!({
            "question": "what ticket is on my screen?",
            "include_screen": true,
            "exclude_hwnd": 4242
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let events = read_sse(resp).await;

    // SSE: ask_started (with the captured window), deltas, done + usage.
    assert_eq!(events[0]["type"], "ask_started");
    assert_eq!(events[0]["provider"], "ollama");
    assert_eq!(events[0]["model"], "mock-model");
    assert_eq!(
        events[0]["screen"]["window_title"],
        "Sprint 12 Board - Jira"
    );
    let ask_id = events[0]["ask_id"].as_str().unwrap();
    let deltas: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "answer_delta")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(deltas.concat(), "The answer is 42.");
    let done = events.last().unwrap();
    assert_eq!(done["type"], "answer_done");
    assert_eq!(done["ask_id"], ask_id);
    assert_eq!(done["usage"]["total_tokens"], 15);

    // WS mirror: the same deltas and done, same ask_id.
    let ws_done = wait_for_ws_event(&ws_events, "answer_done").await;
    assert_eq!(ws_done["ask_id"], ask_id);
    assert_eq!(ws_done["usage"]["total_tokens"], 15);
    {
        let evs = ws_events.lock().await;
        let ws_deltas: Vec<String> = evs
            .iter()
            .filter(|e| e["type"] == "answer_delta")
            .map(|e| e["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ws_deltas.concat(), "The answer is 42.");
    }

    // The overlay-exclusion HWND reached the reader (plumbed, Phase 9 arms it).
    assert_eq!(
        reader.excludes_seen.lock().unwrap().as_slice(),
        &[Some(4242)]
    );

    // The LLM saw: copilot system template, screen section, the question.
    let reqs = llm.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    let system = sent["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("meeting copilot"), "{system}");
    let user = sent["messages"][1]["content"].as_str().unwrap();
    assert!(
        user.contains("## Screen (chrome — \"Sprint 12 Board - Jira\")"),
        "{user}"
    );
    assert!(user.contains("AUR-42 Fix chunker overlap dedup"), "{user}");
    assert!(
        user.contains("## Question\nwhat ticket is on my screen?"),
        "{user}"
    );
    assert!(
        !user.contains("## Recent transcript"),
        "transcript not requested"
    );
}

#[tokio::test]
async fn transcript_window_reaches_the_prompt_even_after_the_session_stops() {
    let llm = mock_llm(MOCK_SSE).await;
    let reader = MockReader::with_script(vec![]);
    let (base, engine) = spawn_server(cfg_with_llm(&llm.uri()), "ring", reader).await;

    // Record the fixture WAV through the real pipeline; its finals feed
    // the transcript ring via the broadcast channel.
    let wav = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/tts_loopback_16k.wav");
    let id = engine
        .start_session(StartParams {
            title: Some("ring run".into()),
            stt_provider: None,
            mic_device: None,
            loopback_device: None,
            audio: Some(AudioSource::WavFile {
                path: wav,
                channel: ChannelId::Loopback,
            }),
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    engine.stop_session(&id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.active_session().is_some() {
        assert!(std::time::Instant::now() < deadline, "stop did not finish");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // No active session anymore — the ring still holds the window.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/ask"))
        .json(&serde_json::json!({
            "question": "what was just being discussed?",
            "include_transcript": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let events = read_sse(resp).await;
    assert_eq!(events[0]["type"], "ask_started");
    assert!(events[0]["screen"].is_null());
    assert!(events[0]["transcript_segments"].as_u64().unwrap() > 0);
    assert_eq!(events.last().unwrap()["type"], "answer_done");

    let reqs = llm.received_requests().await.unwrap();
    let user: String = serde_json::from_slice::<serde_json::Value>(&reqs[0].body).unwrap()
        ["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        user.contains("## Recent transcript (last 10 min)"),
        "{user}"
    );
    assert!(user.contains("Them: spoken words"), "{user}");
}

#[tokio::test]
async fn empty_transcript_window_still_answers() {
    let llm = mock_llm(MOCK_SSE).await;
    let reader = MockReader::with_script(vec![]);
    let (base, _engine) = spawn_server(cfg_with_llm(&llm.uri()), "empty-ring", reader).await;

    // No session ever ran: the window is honestly empty, the ask succeeds.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/ask"))
        .json(&serde_json::json!({"question": "anything?", "include_transcript": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let events = read_sse(resp).await;
    assert_eq!(events[0]["transcript_segments"], 0);
    assert_eq!(events.last().unwrap()["type"], "answer_done");

    let reqs = llm.received_requests().await.unwrap();
    let user: String = serde_json::from_slice::<serde_json::Value>(&reqs[0].body).unwrap()
        ["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        user.contains("(nothing was captured in this window)"),
        "{user}"
    );
}

#[tokio::test]
async fn follow_up_includes_prior_qa_and_plain_asks_do_not() {
    let llm = mock_llm(MOCK_SSE).await;
    let reader = MockReader::with_script(vec![]);
    let (base, _engine) = spawn_server(cfg_with_llm(&llm.uri()), "followup", reader).await;
    let client = reqwest::Client::new();

    let ask = |question: &str, follow_up: bool| {
        let client = client.clone();
        let base = base.clone();
        let body = serde_json::json!({"question": question, "follow_up": follow_up});
        async move {
            let resp = client
                .post(format!("{base}/api/v1/ask"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let events = read_sse(resp).await;
            assert_eq!(events.last().unwrap()["type"], "answer_done", "{events:?}");
        }
    };

    ask("what is the meaning of life?", false).await;
    ask("and can you elaborate?", true).await;
    ask("a fresh unrelated question", false).await;

    let reqs = llm.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 3);
    let user_of = |i: usize| -> String {
        serde_json::from_slice::<serde_json::Value>(&reqs[i].body).unwrap()["messages"][1]
            ["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // The follow-up carries the first Q and its streamed answer.
    let followup = user_of(1);
    assert!(
        followup.contains("## Earlier in this conversation"),
        "{followup}"
    );
    assert!(
        followup.contains("Q: what is the meaning of life?"),
        "{followup}"
    );
    assert!(followup.contains("A: The answer is 42."), "{followup}");
    // Non-follow-ups carry no history, before or after.
    assert!(!user_of(0).contains("## Earlier in this conversation"));
    assert!(!user_of(2).contains("## Earlier in this conversation"));
}

#[tokio::test]
async fn retain_context_off_by_default_persists_nothing() {
    let llm = mock_llm(MOCK_SSE).await;
    let reader = MockReader::with_script(vec![Ok(jira_screen())]);
    let cfg = cfg_with_llm(&llm.uri());
    assert!(!cfg.copilot.retain_context, "default must be off");
    let (base, engine) = spawn_server(cfg, "no-retain", reader).await;
    let client = reqwest::Client::new();

    for (q, screen) in [("first ask?", true), ("second ask?", false)] {
        let resp = client
            .post(format!("{base}/api/v1/ask"))
            .json(&serde_json::json!({"question": q, "include_screen": screen}))
            .send()
            .await
            .unwrap();
        let events = read_sse(resp).await;
        assert_eq!(events.last().unwrap()["type"], "answer_done");
    }
    // Give the post-stream persistence path (which must not run) a beat.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The guarantee: not "no rows" but *no table at all* — the schema
    // carries no trace of questions or screen content.
    assert_eq!(engine.store().asks_count().unwrap(), None);
}

#[tokio::test]
async fn retain_context_on_persists_question_screen_and_answer() {
    let llm = mock_llm(MOCK_SSE).await;
    let reader = MockReader::with_script(vec![Ok(jira_screen())]);
    let mut cfg = cfg_with_llm(&llm.uri());
    cfg.copilot.retain_context = true;
    let (base, engine) = spawn_server(cfg, "retain", reader).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/ask"))
        .json(&serde_json::json!({"question": "keep this one", "include_screen": true}))
        .send()
        .await
        .unwrap();
    let events = read_sse(resp).await;
    assert_eq!(events.last().unwrap()["type"], "answer_done");

    // Persistence happens after the stream completes; poll briefly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while engine.store().asks_count().unwrap() != Some(1) {
        assert!(
            std::time::Instant::now() < deadline,
            "ask row not persisted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (question, screen_context, answer, provider): (String, String, String, String) = {
        // Same path temp_root() built for this test — computed, not
        // recreated (temp_root wipes).
        let db = std::env::temp_dir()
            .join(format!("auricle-ask-it-retain-{}", std::process::id()))
            .join("auricle.db");
        assert!(db.exists(), "db missing at {}", db.display());
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(
            "SELECT question, screen_context, answer, provider FROM asks",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    };
    assert_eq!(question, "keep this one");
    assert!(screen_context.contains("Sprint 12 Board - Jira"));
    assert_eq!(answer, "The answer is 42.");
    assert_eq!(provider, "ollama");
}

#[tokio::test]
async fn ask_error_paths() {
    let llm = mock_llm(MOCK_SSE).await;
    let reader = MockReader::with_script(vec![Err(VisionError::Minimized)]);
    let mut cfg = cfg_with_llm(&llm.uri());
    cfg.llm.groq.api_key_env = "AURICLE_TEST_NO_SUCH_KEY".to_string();
    let (base, _engine) = spawn_server(cfg, "errors", reader).await;
    let ws_events = spawn_ws_collector(&base);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let client = reqwest::Client::new();
    let ask_url = format!("{base}/api/v1/ask");

    // Empty question -> 400.
    let r = client
        .post(&ask_url)
        .json(&serde_json::json!({"question": "  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // Provider with no key -> 409 (not ready), naming the env var.
    let r = client
        .post(&ask_url)
        .json(&serde_json::json!({"question": "q", "provider": "groq"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    assert!(r.text().await.unwrap().contains("AURICLE_TEST_NO_SUCH_KEY"));

    // Unknown provider -> 400.
    let r = client
        .post(&ask_url)
        .json(&serde_json::json!({"question": "q", "provider": "skynet"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // Unknown session -> 404.
    let r = client
        .post(&ask_url)
        .json(&serde_json::json!({"question": "q", "session_id": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Screen capture failure -> 409 (transient) + ask_error on the WS.
    let r = client
        .post(&ask_url)
        .json(&serde_json::json!({"question": "q", "include_screen": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    assert!(r.text().await.unwrap().contains("minimized"));
    let err = wait_for_ws_event(&ws_events, "ask_error").await;
    assert!(err["message"].as_str().unwrap().contains("minimized"));
}

#[tokio::test]
async fn upstream_llm_failure_streams_ask_error_on_both_surfaces() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("model exploded"))
        .mount(&llm)
        .await;
    let reader = MockReader::with_script(vec![]);
    let (base, _engine) = spawn_server(cfg_with_llm(&llm.uri()), "llm-fail", reader).await;
    let ws_events = spawn_ws_collector(&base);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The response itself is an SSE stream (the failure happens after the
    // readiness pre-flight); the error arrives as an ask_error event.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/ask"))
        .json(&serde_json::json!({"question": "q"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let events = read_sse(resp).await;
    let last = events.last().unwrap();
    assert_eq!(last["type"], "ask_error");
    assert!(last["message"].as_str().unwrap().contains("model exploded"));

    let ws_err = wait_for_ws_event(&ws_events, "ask_error").await;
    assert!(ws_err["message"]
        .as_str()
        .unwrap()
        .contains("model exploded"));
}
