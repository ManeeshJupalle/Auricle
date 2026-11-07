//! Dictation end to end: the engine and the REST surface with a fake provider,
//! audio from the fixture WAV. These tests never open a capture device: a
//! start that could reach one is guarded by a provider set to refuse first.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, ChunkKind, Config, Error, Result, Segment, SttEvent};
use auricle_server::{
    AudioSource, DictationError, DictationParams, Engine, EngineOptions, StartParams, WsEvent,
};
use auricle_stt::{SessionCfg, SttKind, SttProvider, SttSession};
use tokio::sync::{broadcast, mpsc, Semaphore};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const DEADLINE: Duration = Duration::from_secs(10);
/// What the fake provider hears in every final, to search for afterwards.
const HEARD: &str = "quokka";

// ---------- fake provider ----------

#[derive(Default)]
struct Probe {
    /// Text of every final fed, in order.
    said: Mutex<Vec<String>>,
    languages: Mutex<Vec<String>>,
    entered: AtomicUsize,
    opened: AtomicUsize,
    closed: AtomicUsize,
    /// While set, `start_session` waits for `release`.
    hold: AtomicBool,
    /// Refuse every `start_session`: a start that should have been turned
    /// away earlier then fails before it can reach a device.
    refuse: AtomicBool,
    /// Answer finals only once the session is finishing, as a provider that
    /// has fallen behind would.
    answer_at_finish: AtomicBool,
    /// How long `finish` takes.
    finish_ms: AtomicUsize,
    gate: Gate,
}

/// Where a held `start_session` waits.
struct Gate(Semaphore);

impl Default for Gate {
    fn default() -> Self {
        Gate(Semaphore::new(0))
    }
}

impl Probe {
    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.gate.0.add_permits(16);
    }

    fn said(&self) -> String {
        self.said.lock().unwrap().join(" ")
    }
}

struct FakeProvider {
    probe: Arc<Probe>,
    id: &'static str,
    kind: SttKind,
}

struct FakeSession {
    probe: Arc<Probe>,
    tx: Option<mpsc::UnboundedSender<SttEvent>>,
    rx: mpsc::UnboundedReceiver<SttEvent>,
    held: Vec<SttEvent>,
}

#[async_trait]
impl SttProvider for FakeProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn kind(&self) -> SttKind {
        self.kind
    }
    async fn start_session(&self, cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
        let probe = &self.probe;
        probe.entered.fetch_add(1, Ordering::SeqCst);
        probe.languages.lock().unwrap().push(cfg.language.clone());
        if probe.hold.load(Ordering::SeqCst) {
            probe.gate.0.acquire().await.unwrap().forget();
        }
        if probe.refuse.load(Ordering::SeqCst) {
            return Err(Error::Stt("session refused".to_string()));
        }
        probe.opened.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Box::new(FakeSession {
            probe: probe.clone(),
            tx: Some(tx),
            rx,
            held: Vec::new(),
        }))
    }
}

#[async_trait]
impl SttSession for FakeSession {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
        if chunk.kind != ChunkKind::Final {
            return Ok(());
        }
        let text = format!("{HEARD} {}", chunk.seq);
        self.probe.said.lock().unwrap().push(text.clone());
        let event = SttEvent::Final(Segment {
            channel: chunk.channel,
            t_start_ms: chunk.t_start_ms,
            t_end_ms: chunk.t_end_ms,
            text,
        });
        if self.probe.answer_at_finish.load(Ordering::SeqCst) {
            self.held.push(event);
        } else if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
        Ok(())
    }
    async fn next_event(&mut self) -> Option<SttEvent> {
        self.rx.recv().await
    }
    async fn finish(&mut self) -> Result<Vec<Segment>> {
        let ms = self.probe.finish_ms.load(Ordering::SeqCst) as u64;
        tokio::time::sleep(Duration::from_millis(ms)).await;
        if let Some(tx) = self.tx.take() {
            for event in self.held.drain(..) {
                let _ = tx.send(event);
            }
        }
        self.probe.closed.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

// ---------- helpers ----------

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "auricle-server-dictation-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn engine(name: &str) -> (Arc<Engine>, Arc<Probe>) {
    engine_with(name, Config::default(), "whisper-local", SttKind::Local)
}

fn engine_with(
    name: &str,
    cfg: Config,
    id: &'static str,
    kind: SttKind,
) -> (Arc<Engine>, Arc<Probe>) {
    let probe = Arc::new(Probe::default());
    let engine = Engine::new(EngineOptions {
        cfg,
        data_root: temp_root(name),
        provider_override: Some(Arc::new(FakeProvider {
            probe: probe.clone(),
            id,
            kind,
        })),
    })
    .unwrap();
    (engine, probe)
}

async fn serve(engine: &Arc<Engine>) -> String {
    let app = auricle_server::build_router(engine.clone(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tts_loopback_16k.wav")
}

fn dictation() -> DictationParams {
    DictationParams {
        audio: Some(AudioSource::WavFile {
            path: fixture(),
            channel: ChannelId::Mic,
        }),
        ..DictationParams::default()
    }
}

fn meeting() -> StartParams {
    StartParams {
        title: Some("meeting".into()),
        audio: Some(AudioSource::WavFile {
            path: fixture(),
            channel: ChannelId::Loopback,
        }),
        ..StartParams::default()
    }
}

fn set(engine: &Engine, key: &str, value: serde_json::Value) {
    engine.store().set_setting(key, &value).unwrap();
}

/// Long enough for the whole fixture to go in: it is fed ~30× real time.
async fn let_the_fixture_play() {
    tokio::time::sleep(Duration::from_secs(2)).await;
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + DEADLINE;
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn next_event(
    rx: &mut broadcast::Receiver<WsEvent>,
    want: impl Fn(&WsEvent) -> bool,
) -> WsEvent {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let ev = rx.recv().await.unwrap();
            if want(&ev) {
                return ev;
            }
        }
    })
    .await
    .expect("event did not arrive")
}

async fn stop_meeting(engine: &Engine, id: &str) {
    engine.stop_session(id).unwrap();
    until("meeting to stop", || engine.lifecycle_state() == "idle").await;
}

async fn post(base: &str, path: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or_default())
}

async fn get(base: &str, path: &str) -> serde_json::Value {
    reqwest::get(format!("{base}{path}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Every file under `dir` whose bytes contain `needle`.
fn files_containing(dir: &std::path::Path, needle: &[u8]) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if let Ok(bytes) = std::fs::read(&path) {
            if bytes.windows(needle.len()).any(|w| w == needle) {
                found.push(path);
            }
        }
    }
    found
}

// ---------- stop and its text ----------

#[tokio::test]
async fn stop_waits_for_the_last_text_and_answers_with_all_of_it() {
    let (engine, probe) = engine("stop-text");
    // Nothing is answered until the session finishes: only a stop that waits
    // for the end of transcription can have any text at all.
    probe.answer_at_finish.store(true, Ordering::SeqCst);
    let base = serve(&engine).await;
    let mut events = engine.subscribe_important();

    let started = engine.start_dictation(dictation()).await.unwrap();
    assert!(started.id.starts_with('d'), "{}", started.id);
    assert_eq!(
        (started.provider.as_str(), started.local),
        ("whisper-local", true)
    );
    let ev = next_event(&mut events, |e| {
        matches!(e, WsEvent::DictationStarted { .. })
    })
    .await;
    assert!(
        matches!(ev, WsEvent::DictationStarted { id, provider } if id == started.id && provider == "whisper-local")
    );
    let_the_fixture_play().await;

    let (status, body) = post(&base, "/api/v1/dictation/stop").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], started.id);
    let said = probe.said();
    assert!(said.contains(HEARD), "the fixture produced no finals");
    assert_eq!(body["text"], said);
    assert!(body["duration_ms"].as_u64().unwrap() >= 2_000, "{body}");

    let ev = next_event(&mut events, |e| {
        matches!(e, WsEvent::DictationStopped { .. })
    })
    .await;
    assert!(matches!(ev, WsEvent::DictationStopped { id } if id == started.id));
    // Free when the text arrives: the next press can start straight away.
    let next = engine.start_dictation(dictation()).await.unwrap();
    assert_ne!(next.id, started.id);
    engine.cancel_dictation().await.unwrap();
}

#[tokio::test]
async fn partials_carry_the_text_so_far() {
    let (engine, _probe) = engine("partials");
    let mut partials = engine.subscribe_partials();

    let started = engine.start_dictation(dictation()).await.unwrap();
    let ev = next_event(&mut partials, |e| {
        matches!(e, WsEvent::DictationPartial { .. })
    })
    .await;
    let WsEvent::DictationPartial { id, text } = ev else {
        unreachable!();
    };
    assert_eq!(id, started.id);
    assert!(text.starts_with(HEARD), "{text}");

    let dictated = engine.stop_dictation().await.unwrap();
    assert!(
        dictated.text.starts_with(&text),
        "{} / {text}",
        dictated.text
    );
}

#[tokio::test]
async fn stop_and_cancel_with_no_dictation_answer_404() {
    let (engine, _probe) = engine("none");
    let base = serve(&engine).await;
    for path in ["/api/v1/dictation/stop", "/api/v1/dictation/cancel"] {
        let (status, body) = post(&base, path).await;
        assert_eq!(status, 404, "{path}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("no dictation"),
            "{body}"
        );
    }
}

// ---------- nothing is kept ----------

#[tokio::test]
async fn a_dictation_leaves_no_session_segment_audio_or_text_on_disk() {
    let (engine, _probe) = engine("nothing-kept");
    // Retention on, which a meeting would honour with a WAV per channel.
    set(&engine, "retain_raw_audio", serde_json::json!(true));
    let base = serve(&engine).await;

    engine.start_dictation(dictation()).await.unwrap();
    let_the_fixture_play().await;
    let dictated = engine.stop_dictation().await.unwrap();
    assert!(
        dictated.text.contains(HEARD),
        "nothing heard, so nothing to leak"
    );

    assert!(engine.store().list_sessions(None).unwrap().is_empty());
    let lines = get(&base, &format!("/api/v1/search?q={HEARD}")).await;
    assert_eq!(lines["total"], 0, "{lines}");
    assert!(
        !engine.data_root().join("sessions").exists(),
        "audio was kept"
    );
    let egress = get(&base, "/api/v1/egress").await;
    assert_eq!(
        egress["entries"],
        serde_json::json!([]),
        "a local dictation records nothing"
    );
    assert_eq!(
        files_containing(engine.data_root(), HEARD.as_bytes()),
        Vec::<std::path::PathBuf>::new()
    );

    // The same words in a meeting are found by these same checks, so the
    // ones above can't pass by looking in the wrong place.
    let id = engine.start_session(meeting()).await.unwrap();
    let_the_fixture_play().await;
    stop_meeting(&engine, &id).await;
    assert!(engine.data_root().join("sessions").exists());
    assert!(!files_containing(engine.data_root(), HEARD.as_bytes()).is_empty());
}

#[tokio::test]
async fn a_cloud_provider_records_a_dictation_row_before_any_audio_and_no_text() {
    let (engine, probe) = engine_with(
        "cloud-ledger",
        Config::default(),
        "groq-whisper",
        SttKind::CloudBatch,
    );
    set(
        &engine,
        "dictation_provider",
        serde_json::json!("groq-whisper"),
    );
    let base = serve(&engine).await;

    let started = engine.start_dictation(dictation()).await.unwrap();
    assert_eq!(
        (started.provider.as_str(), started.local),
        ("groq-whisper", false)
    );
    let entries = get(&base, "/api/v1/egress").await["entries"].clone();
    assert_eq!(entries.as_array().unwrap().len(), 1, "{entries}");
    let row = &entries[0];
    assert_eq!(row["kind"], "dictation");
    assert_eq!(row["destination"], "cloud");
    assert_eq!(row["host"], "api.groq.com");
    assert_eq!(row["provider"], "groq-whisper");
    assert!(row["session_id"].is_null(), "{row}");

    let_the_fixture_play().await;
    let dictated = engine.stop_dictation().await.unwrap();
    assert!(dictated.text.contains(HEARD));
    assert!(probe.said().contains(HEARD));
    let entries = get(&base, "/api/v1/egress").await["entries"].clone();
    assert_eq!(
        entries.as_array().unwrap().len(),
        1,
        "one row per dictation"
    );
    assert!(!entries.to_string().contains(HEARD), "{entries}");
    assert_eq!(
        files_containing(engine.data_root(), HEARD.as_bytes()),
        Vec::<std::path::PathBuf>::new()
    );
}

#[tokio::test]
async fn dictation_stays_local_when_meetings_default_to_the_cloud() {
    let mut cfg = Config::default();
    cfg.stt.provider = "groq-whisper".to_string();
    let (engine, _probe) = engine_with("default-local", cfg, "whisper-local", SttKind::Local);
    set(
        &engine,
        "default_provider",
        serde_json::json!("groq-whisper"),
    );
    let base = serve(&engine).await;

    let started = engine.start_dictation(dictation()).await.unwrap();
    assert_eq!(
        (started.provider.as_str(), started.local),
        ("whisper-local", true)
    );
    engine.stop_dictation().await.unwrap();
    assert_eq!(
        get(&base, "/api/v1/egress").await["entries"],
        serde_json::json!([])
    );
}

// ---------- one owner of the mic ----------

#[tokio::test]
async fn a_meeting_cannot_start_during_a_dictation() {
    let (engine, probe) = engine("meeting-409");
    let base = serve(&engine).await;
    engine.start_dictation(dictation()).await.unwrap();
    probe.refuse.store(true, Ordering::SeqCst);

    let error = engine.start_session(meeting()).await.unwrap_err();
    assert!(error.to_string().contains("dictation"), "{error}");
    // Over REST with no audio given, which would open real devices if the
    // slot let it through; the refusing provider stops it first.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/sessions"))
        .json(&serde_json::json!({"title": "t"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("dictation"),
        "{body}"
    );
    // A dictation is not a meeting to anyone watching for one.
    let health = get(&base, "/api/v1/health").await;
    assert_eq!(
        (health["state"].as_str(), &health["active_session"]),
        (Some("idle"), &serde_json::Value::Null)
    );

    engine.stop_dictation().await.unwrap();
    probe.refuse.store(false, Ordering::SeqCst);
    let id = engine.start_session(meeting()).await.unwrap();
    stop_meeting(&engine, &id).await;
}

#[tokio::test]
async fn a_dictation_cannot_start_during_a_meeting_or_another_dictation() {
    let (engine, probe) = engine("dictation-409");
    let base = serve(&engine).await;
    let meeting_id = engine.start_session(meeting()).await.unwrap();
    probe.refuse.store(true, Ordering::SeqCst);

    let (status, body) = post(&base, "/api/v1/dictation/start").await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("meeting"),
        "{body}"
    );
    let error = engine.start_dictation(dictation()).await.unwrap_err();
    assert!(error.to_string().contains(&meeting_id), "{error}");
    assert_eq!(
        engine.active_session().as_deref(),
        Some(meeting_id.as_str())
    );

    probe.refuse.store(false, Ordering::SeqCst);
    stop_meeting(&engine, &meeting_id).await;
    engine.start_dictation(dictation()).await.unwrap();
    probe.refuse.store(true, Ordering::SeqCst);
    let (status, body) = post(&base, "/api/v1/dictation/start").await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("already active"),
        "{body}"
    );
    engine.cancel_dictation().await.unwrap();
}

// ---------- cancel, the cap, and clients that go away ----------

#[tokio::test]
async fn cancel_discards_what_was_heard() {
    let (engine, probe) = engine("cancel");
    let base = serve(&engine).await;
    let mut events = engine.subscribe_important();
    let started = engine.start_dictation(dictation()).await.unwrap();
    until("something heard", || probe.said().contains(HEARD)).await;

    let (status, body) = post(&base, "/api/v1/dictation/cancel").await;
    assert_eq!(
        (status, &body),
        (200, &serde_json::json!({"id": started.id}))
    );
    let ev = next_event(&mut events, |e| {
        matches!(e, WsEvent::DictationStopped { .. })
    })
    .await;
    assert!(matches!(ev, WsEvent::DictationStopped { id } if id == started.id));

    // Gone: no stop can collect it, and the mic is free at once.
    let (status, _) = post(&base, "/api/v1/dictation/stop").await;
    assert_eq!(status, 404);
    let next = engine.start_dictation(dictation()).await.unwrap();
    engine.cancel_dictation().await.unwrap();
    assert_ne!(next.id, started.id);
}

#[tokio::test]
async fn the_cap_stops_a_dictation_nobody_stopped_and_keeps_its_text_for_the_stop() {
    let (engine, _probe) = engine("cap");
    let mut events = engine.subscribe_important();
    let started = engine
        .start_dictation(DictationParams {
            max_duration: Some(Duration::from_millis(300)),
            ..dictation()
        })
        .await
        .unwrap();

    let ev = next_event(&mut events, |e| {
        matches!(e, WsEvent::DictationStopped { .. })
    })
    .await;
    assert!(matches!(ev, WsEvent::DictationStopped { id } if id == started.id));
    // The mic is free without anyone asking.
    let meeting_id = engine.start_session(meeting()).await.unwrap();
    stop_meeting(&engine, &meeting_id).await;

    // A key released after the cap still gets what was said.
    let dictated = engine.stop_dictation().await.unwrap();
    assert_eq!(dictated.id, started.id);
    assert!(
        (300..2_000).contains(&dictated.duration_ms),
        "{}",
        dictated.duration_ms
    );
    assert!(matches!(
        engine.stop_dictation().await,
        Err(DictationError::NotActive)
    ));

    // Text nobody collects goes when the next dictation starts.
    let capped = engine
        .start_dictation(DictationParams {
            max_duration: Some(Duration::from_millis(300)),
            ..dictation()
        })
        .await
        .unwrap();
    let ev = next_event(&mut events, |e| {
        matches!(e, WsEvent::DictationStopped { .. })
    })
    .await;
    assert!(matches!(ev, WsEvent::DictationStopped { id } if id == capped.id));
    let fresh = engine.start_dictation(dictation()).await.unwrap();
    assert_eq!(engine.stop_dictation().await.unwrap().id, fresh.id);
    assert!(matches!(
        engine.stop_dictation().await,
        Err(DictationError::NotActive)
    ));
}

#[tokio::test]
async fn a_client_gone_mid_start_cannot_wedge_the_mic() {
    let (engine, probe) = engine("gone-mid-start");
    let mut events = engine.subscribe_important();
    probe.hold.store(true, Ordering::SeqCst);

    // The caller disconnects while the model loads: its future is dropped.
    let params = DictationParams {
        max_duration: Some(Duration::from_millis(500)),
        ..dictation()
    };
    let abandoned =
        tokio::time::timeout(Duration::from_millis(100), engine.start_dictation(params));
    assert!(abandoned.await.is_err(), "start should still be held open");
    assert!(
        engine.start_session(meeting()).await.is_err(),
        "the dictation has the mic"
    );

    // Nobody will stop it; the cap does.
    probe.release();
    next_event(&mut events, |e| {
        matches!(e, WsEvent::DictationStopped { .. })
    })
    .await;
    let id = engine.start_session(meeting()).await.unwrap();
    stop_meeting(&engine, &id).await;
}

#[tokio::test]
async fn a_client_gone_mid_stop_cannot_wedge_the_mic() {
    let (engine, probe) = engine("gone-mid-stop");
    probe.finish_ms.store(500, Ordering::SeqCst);
    let mut events = engine.subscribe_important();
    engine.start_dictation(dictation()).await.unwrap();

    // The caller disconnects while the last chunk is transcribed.
    let abandoned = tokio::time::timeout(Duration::from_millis(50), engine.stop_dictation());
    assert!(abandoned.await.is_err(), "stop should still be waiting");

    next_event(&mut events, |e| {
        matches!(e, WsEvent::DictationStopped { .. })
    })
    .await;
    let id = engine.start_session(meeting()).await.unwrap();
    stop_meeting(&engine, &id).await;
}

#[tokio::test]
async fn a_stop_during_start_ends_it_with_no_text_and_nothing_left_open() {
    let (engine, probe) = engine("stop-in-start");
    probe.hold.store(true, Ordering::SeqCst);
    let start = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.start_dictation(dictation()).await })
    };
    until("start to reach the provider", || {
        probe.entered.load(Ordering::SeqCst) > 0
    })
    .await;

    // A key pressed and released while the model was still loading.
    let stop = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.stop_dictation().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    probe.release();

    let error = start.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
    let dictated = stop.await.unwrap().unwrap();
    assert_eq!(dictated.text, "");
    assert_eq!(engine.lifecycle_state(), "idle");
    assert_eq!(
        probe.closed.load(Ordering::SeqCst),
        probe.opened.load(Ordering::SeqCst)
    );
    let id = engine.start_session(meeting()).await.unwrap();
    stop_meeting(&engine, &id).await;
}

#[tokio::test]
async fn a_failed_start_leaves_nothing_to_stop() {
    let (engine, probe) = engine("failed-start");
    probe.refuse.store(true, Ordering::SeqCst);

    let error = engine.start_dictation(dictation()).await.unwrap_err();
    assert!(error.to_string().contains("refused"), "{error}");
    assert!(matches!(
        engine.stop_dictation().await,
        Err(DictationError::NotActive)
    ));
    probe.refuse.store(false, Ordering::SeqCst);
    engine.start_dictation(dictation()).await.unwrap();
    engine.cancel_dictation().await.unwrap();
}

// ---------- settings ----------

#[tokio::test]
async fn the_dictation_language_falls_back_to_the_meeting_one() {
    let (engine, probe) = engine("language");
    let base = serve(&engine).await;
    let language_of = |params: DictationParams| {
        let engine = engine.clone();
        let probe = probe.clone();
        async move {
            engine.start_dictation(params).await.unwrap();
            engine.cancel_dictation().await.unwrap();
            probe.languages.lock().unwrap().pop().unwrap()
        }
    };

    set(&engine, "stt_language", serde_json::json!("de"));
    assert_eq!(language_of(dictation()).await, "de");
    set(&engine, "dictation_language", serde_json::json!("fr"));
    assert_eq!(language_of(dictation()).await, "fr");
    let asked = DictationParams {
        language: Some("es".into()),
        ..dictation()
    };
    assert_eq!(language_of(asked).await, "es");

    // Validated as the meeting one is, before it can reach a provider's URL.
    let resp = reqwest::Client::new()
        .put(format!("{base}/api/v1/settings"))
        .json(&serde_json::json!({"dictation_language": "en&model=x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let bad = DictationParams {
        language: Some("en&model=x".into()),
        ..dictation()
    };
    let error = engine.start_dictation(bad).await.unwrap_err();
    assert!(error.to_string().contains("en&model=x"), "{error}");
    assert_eq!(engine.lifecycle_state(), "idle");
}

// ---------- LLM polish ----------

/// An engine whose LLM is a mock Ollama on loopback: a local LLM.
fn engine_with_llm(name: &str, llm: &MockServer) -> (Arc<Engine>, Arc<Probe>) {
    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri();
    cfg.llm.ollama.model = "mock-model".to_string();
    engine_with(name, cfg, "whisper-local", SttKind::Local)
}

fn completion(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-x", "object": "chat.completion", "model": "mock-model",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": content}}]
    }))
}

/// What the mock model makes of a dictation: capitalised, with a full stop.
fn polished(heard: &str) -> String {
    let mut chars = heard.chars();
    let first = chars.next().unwrap().to_uppercase();
    format!("{first}{}.", chars.as_str())
}

/// A model that polishes what it is sent, and nothing else.
struct Polisher;

impl Respond for Polisher {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let framed = body["messages"][1]["content"].as_str().unwrap();
        let heard = framed
            .strip_prefix("<dictation>")
            .and_then(|t| t.strip_suffix("</dictation>"))
            .unwrap();
        completion(&polished(heard))
    }
}

/// Dictate the fixture until something is heard, then stop over REST. Answers
/// with the stop's status, its body, and how long the stop took.
async fn dictate_and_stop(
    engine: &Arc<Engine>,
    probe: &Probe,
    base: &str,
) -> (u16, serde_json::Value, Duration) {
    probe.said.lock().unwrap().clear();
    engine.start_dictation(dictation()).await.unwrap();
    until("something heard", || probe.said().contains(HEARD)).await;
    let asked = std::time::Instant::now();
    let (status, body) = post(base, "/api/v1/dictation/stop").await;
    (status, body, asked.elapsed())
}

#[tokio::test]
async fn polish_off_answers_with_the_text_as_heard_and_calls_no_llm() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Polisher)
        .mount(&llm)
        .await;
    let (engine, probe) = engine_with_llm("polish-off", &llm);
    let base = serve(&engine).await;

    // Unset, then set to false: both are off.
    for setting in [None, Some(false)] {
        if let Some(on) = setting {
            set(&engine, "dictation_polish", serde_json::json!(on));
        }
        let (status, body, _) = dictate_and_stop(&engine, &probe, &base).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["text"], probe.said(), "{setting:?}");
        assert_eq!(body["polished"], false);
        assert!(body.get("polish_note").is_none(), "{body}");
    }
    assert!(llm.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_polish_setting_takes_only_true_or_false() {
    let (engine, _probe) = engine("polish-setting");
    let base = serve(&engine).await;
    let put = |value: serde_json::Value| {
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .put(format!("{base}/api/v1/settings"))
                .json(&serde_json::json!({ "dictation_polish": value }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    for bad in [serde_json::json!("true"), serde_json::json!(1)] {
        assert_eq!(put(bad.clone()).await, 400, "{bad}");
    }
    let settings = get(&base, "/api/v1/settings").await;
    assert!(settings.get("dictation_polish").is_none(), "{settings}");
    assert_eq!(put(serde_json::json!(true)).await, 200);
    assert_eq!(
        get(&base, "/api/v1/settings").await["dictation_polish"],
        true
    );
}

#[tokio::test]
async fn polish_on_answers_with_the_polished_text_and_a_local_llm_records_nothing() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Polisher)
        .expect(1)
        .mount(&llm)
        .await;
    let (engine, probe) = engine_with_llm("polish-on", &llm);
    set(&engine, "dictation_polish", serde_json::json!(true));
    let base = serve(&engine).await;

    let (status, body, _) = dictate_and_stop(&engine, &probe, &base).await;
    assert_eq!(status, 200, "{body}");
    let heard = probe.said();
    assert_eq!(body["text"], polished(&heard));
    assert_eq!(body["polished"], true);
    assert!(body.get("polish_note").is_none(), "{body}");

    // The text went framed, under the fixed prompt.
    let requests = llm.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        sent["messages"][0]["content"],
        auricle_llm::DICTATION_POLISH
    );
    assert_eq!(
        sent["messages"][1]["content"],
        format!("<dictation>{heard}</dictation>")
    );

    // An LLM on this machine: no ledger row, and the text, as heard or
    // polished, is nowhere on disk.
    assert_eq!(
        get(&base, "/api/v1/egress").await["entries"],
        serde_json::json!([])
    );
    for needle in [HEARD, "Quokka"] {
        assert_eq!(
            files_containing(engine.data_root(), needle.as_bytes()),
            Vec::<std::path::PathBuf>::new(),
            "{needle}"
        );
    }
}

/// Polish on, and the LLM answers with `response`: the stop answers with the
/// words as heard, unpolished, and says why.
async fn polish_falls_back(
    name: &str,
    response: ResponseTemplate,
) -> (serde_json::Value, Duration) {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&llm)
        .await;
    let (engine, probe) = engine_with_llm(name, &llm);
    set(&engine, "dictation_polish", serde_json::json!(true));
    let base = serve(&engine).await;

    let (status, body, took) = dictate_and_stop(&engine, &probe, &base).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["text"], probe.said(), "the words as heard");
    assert_eq!(body["polished"], false);
    assert!(body["polish_note"].is_string(), "{body}");
    assert_eq!(llm.received_requests().await.unwrap().len(), 1);
    (body, took)
}

#[tokio::test]
async fn a_failing_llm_leaves_the_text_as_heard() {
    let (body, _) = polish_falls_back("polish-error", ResponseTemplate::new(500)).await;
    assert_eq!(body["polish_note"], "the LLM failed or gave back nothing");
}

#[tokio::test]
async fn an_empty_answer_leaves_the_text_as_heard() {
    let (body, _) = polish_falls_back("polish-empty", completion("")).await;
    assert_eq!(body["polish_note"], "the LLM failed or gave back nothing");
}

#[tokio::test]
async fn a_far_longer_answer_leaves_the_text_as_heard() {
    let essay = "Here is a poem about a small animal. ".repeat(40);
    let (body, _) = polish_falls_back("polish-long", completion(&essay)).await;
    assert_eq!(
        body["polish_note"],
        "the LLM's answer was much longer than what was said"
    );
}

#[tokio::test]
async fn a_slow_llm_leaves_the_text_as_heard_once_the_bound_passes() {
    let slow = completion("Too late.").set_delay(Duration::from_secs(12));
    let (body, took) = polish_falls_back("polish-slow", slow).await;
    assert_eq!(body["polish_note"], "the LLM took longer than 8s");
    assert!(
        (Duration::from_secs(8)..Duration::from_secs(11)).contains(&took),
        "{took:?}"
    );
}

#[tokio::test]
async fn an_llm_that_is_not_set_up_leaves_the_text_as_heard_and_sends_nothing() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Polisher)
        .mount(&llm)
        .await;
    let mut cfg = Config::default();
    // Even with a key somewhere, nothing could leave this machine.
    cfg.llm.groq.base_url = llm.uri();
    cfg.llm.groq.api_key_env = "AURICLE_TEST_NO_SUCH_KEY_POLISH".to_string();
    let (engine, probe) = engine_with("polish-no-key", cfg, "whisper-local", SttKind::Local);
    set(&engine, "dictation_polish", serde_json::json!(true));
    set(&engine, "llm_provider", serde_json::json!("groq"));
    let base = serve(&engine).await;

    let (status, body, _) = dictate_and_stop(&engine, &probe, &base).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["text"], probe.said());
    assert_eq!(body["polished"], false);
    let note = body["polish_note"].as_str().unwrap();
    assert!(note.contains("isn't set up"), "{note}");
    assert!(note.contains("AURICLE_TEST_NO_SUCH_KEY_POLISH"), "{note}");
    assert!(llm.received_requests().await.unwrap().is_empty());
}
