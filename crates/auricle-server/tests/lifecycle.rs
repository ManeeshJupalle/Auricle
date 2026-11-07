//! Start/stop races and failure paths, driven through the engine with a fake
//! provider that can be held mid-start. Audio comes from the fixture WAV:
//! these tests never open a capture device.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, Config, Error, Result, Segment, SttEvent};
use auricle_server::{AudioSource, Engine, EngineOptions, StartParams, WsEvent};
use auricle_stt::{SessionCfg, SttDrops, SttKind, SttProvider, SttSession};
use tokio::sync::{broadcast, mpsc, Semaphore};

const DEADLINE: Duration = Duration::from_secs(10);

// ---------- fake provider: can hold or refuse a session open ----------

struct Probe {
    /// `start_session` calls that have begun (held ones included).
    entered: AtomicUsize,
    opened: AtomicUsize,
    closed: AtomicUsize,
    /// While set, `start_session` waits for `release`.
    hold: AtomicBool,
    /// Refuse the Nth `start_session` call (1-based); 0 refuses none.
    refuse_call: AtomicUsize,
    /// Queued on every session as if its worker had reported it.
    notice: Mutex<Option<String>>,
    drops: Mutex<SttDrops>,
    /// The language each session was opened with.
    languages: Mutex<Vec<String>>,
    gate: Semaphore,
}

impl Probe {
    fn new() -> Arc<Probe> {
        Arc::new(Probe {
            entered: 0.into(),
            opened: 0.into(),
            closed: 0.into(),
            hold: false.into(),
            refuse_call: 0.into(),
            notice: Mutex::new(None),
            drops: Mutex::new(SttDrops::default()),
            languages: Mutex::new(Vec::new()),
            gate: Semaphore::new(0),
        })
    }

    fn hold(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.gate.add_permits(16);
    }

    fn opened(&self) -> usize {
        self.opened.load(Ordering::SeqCst)
    }

    fn closed(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }
}

struct FakeProvider {
    probe: Arc<Probe>,
}

struct FakeSession {
    probe: Arc<Probe>,
    /// Stands in for a live worker: `next_event` ending means the worker died.
    _tx: mpsc::UnboundedSender<SttEvent>,
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
    async fn start_session(&self, cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
        let call = self.probe.entered.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe
            .languages
            .lock()
            .unwrap()
            .push(cfg.language.clone());
        if self.probe.hold.load(Ordering::SeqCst) {
            self.probe.gate.acquire().await.unwrap().forget();
        }
        if self.probe.refuse_call.load(Ordering::SeqCst) == call {
            return Err(Error::Stt("session refused".to_string()));
        }
        self.probe.opened.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(notice) = self.probe.notice.lock().unwrap().as_ref() {
            let _ = tx.send(SttEvent::Error(notice.clone()));
        }
        Ok(Box::new(FakeSession {
            probe: self.probe.clone(),
            _tx: tx,
            rx,
        }))
    }
}

#[async_trait]
impl SttSession for FakeSession {
    async fn feed(&mut self, _chunk: AudioChunk) -> Result<()> {
        Ok(())
    }
    async fn next_event(&mut self) -> Option<SttEvent> {
        self.rx.recv().await
    }
    async fn finish(&mut self) -> Result<Vec<Segment>> {
        self.rx.close();
        self.probe.closed.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    fn drops(&self) -> SttDrops {
        *self.probe.drops.lock().unwrap()
    }
}

// ---------- helpers ----------

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "auricle-server-lifecycle-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn engine(name: &str) -> (Arc<Engine>, Arc<Probe>) {
    engine_with(name, Config::default())
}

fn engine_with(name: &str, cfg: Config) -> (Arc<Engine>, Arc<Probe>) {
    let probe = Probe::new();
    let engine = Engine::new(EngineOptions {
        cfg,
        data_root: temp_root(name),
        provider_override: Some(Arc::new(FakeProvider {
            probe: probe.clone(),
        })),
    })
    .unwrap();
    (engine, probe)
}

fn wav_params() -> StartParams {
    StartParams {
        title: Some("lifecycle".into()),
        audio: Some(AudioSource::WavFile {
            path: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/tts_loopback_16k.wav"),
            channel: ChannelId::Loopback,
        }),
        ..StartParams::default()
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + DEADLINE;
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn stop_and_wait(engine: &Engine, id: &str) {
    engine.stop_session(id).unwrap();
    until("session to stop", || engine.lifecycle_state() == "idle").await;
}

/// Next important event matching `want`, skipping the rest.
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

// ---------- a cancelled start must not strand the lifecycle ----------

#[tokio::test]
async fn dropping_the_start_future_midway_leaves_a_stoppable_session() {
    let (engine, probe) = engine("cancel");
    probe.hold();

    // A client that disconnects mid-start: the handler future is dropped.
    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        engine.start_session(wav_params()),
    )
    .await;
    assert!(cancelled.is_err(), "start should still be held open");
    assert_eq!(engine.lifecycle_state(), "recording");
    let id = engine.active_session().unwrap();

    // The start carries on without its caller, and the session is real.
    probe.release();
    let store = engine.store();
    until("session row", || store.get_session(&id).unwrap().is_some()).await;

    stop_and_wait(&engine, &id).await;
    let next = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &next).await;
}

#[tokio::test]
async fn a_failed_start_nobody_waits_for_frees_the_slot_and_says_so() {
    let (engine, probe) = engine("cancel-fail");
    let mut events = engine.subscribe_important();
    probe.refuse_call.store(1, Ordering::SeqCst);
    probe.hold();

    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        engine.start_session(wav_params()),
    )
    .await;
    assert!(cancelled.is_err(), "start should still be held open");
    let id = engine.active_session().unwrap();

    probe.release();
    until("slot to free", || engine.lifecycle_state() == "idle").await;

    // A reloaded dashboard learned of this start from /health; events are
    // the only way it finds out that the start died.
    let stopped = next_event(&mut events, |ev| {
        matches!(ev, WsEvent::SessionStopped { .. })
    })
    .await;
    assert!(matches!(stopped, WsEvent::SessionStopped { session } if session == id));
    let error = next_event(&mut events, |ev| matches!(ev, WsEvent::Error { .. })).await;
    assert!(
        matches!(&error, WsEvent::Error { message, .. } if message.contains("session refused")),
        "{error:?}"
    );

    let next = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &next).await;
}

// ---------- a stop during start must stop ----------

#[tokio::test]
async fn stop_during_start_stops_the_session() {
    let (engine, probe) = engine("stop-in-start");
    let mut events = engine.subscribe_important();
    probe.hold();

    let start = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.start_session(wav_params()).await })
    };
    until("start to reach the provider", || {
        probe.entered.load(Ordering::SeqCst) > 0
    })
    .await;
    let id = engine.active_session().unwrap();
    engine.stop_session(&id).unwrap();
    assert_eq!(engine.lifecycle_state(), "stopping");

    probe.release();
    let outcome = tokio::time::timeout(DEADLINE, start)
        .await
        .expect("start did not finish")
        .unwrap();
    let error = outcome.expect_err("a stopped start must not report a session");
    assert!(error.to_string().contains("stopped"), "{error}");

    until("session to stop", || engine.lifecycle_state() == "idle").await;
    assert_eq!(engine.active_session(), None);
    assert!(
        engine.store().get_session(&id).unwrap().is_none(),
        "nothing was recorded, so nothing is listed"
    );
    assert_eq!(probe.closed(), probe.opened(), "stt sessions left open");
    let stopped = next_event(&mut events, |ev| {
        matches!(ev, WsEvent::SessionStopped { .. })
    })
    .await;
    assert!(matches!(stopped, WsEvent::SessionStopped { session } if session == id));

    let next = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &next).await;
}

#[tokio::test]
async fn a_start_stopped_over_rest_answers_409() {
    let (engine, probe) = engine("stop-in-start-rest");
    probe.hold();
    let app = auricle_server::build_router(engine.clone(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();

    let start = tokio::spawn(
        client
            .post(format!("{base}/api/v1/sessions"))
            .json(&serde_json::json!({"title": "t"}))
            .send(),
    );
    until("start to reach the provider", || {
        probe.entered.load(Ordering::SeqCst) > 0
    })
    .await;

    // Lands while the provider is held, so no device is ever opened: a start
    // checks for a stop before it touches capture.
    let id = engine.active_session().unwrap();
    let stop = client
        .post(format!("{base}/api/v1/sessions/{id}/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(stop.status(), 200);

    probe.release();
    let resp = tokio::time::timeout(DEADLINE, start)
        .await
        .expect("start did not finish")
        .unwrap()
        .unwrap();
    assert_eq!(
        resp.status(),
        409,
        "the request was fine; the session ended"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("stopped"),
        "{body}"
    );
}

// ---------- a shutdown must not wait on a start ----------

#[tokio::test]
async fn shutdown_does_not_wait_out_a_start_that_recorded_nothing() {
    let (engine, probe) = engine("shutdown-in-start");
    probe.hold();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let daemon = tokio::spawn(auricle_server::serve_engine(engine.clone(), listener, None));

    // Held at the provider: where a first run sits through a model download,
    // which a stop cannot interrupt.
    let start = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.start_session(wav_params()).await })
    };
    until("start to reach the provider", || {
        probe.entered.load(Ordering::SeqCst) > 0
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    tokio::time::timeout(DEADLINE, daemon)
        .await
        .expect("shutdown waited on a start with nothing to persist")
        .unwrap()
        .unwrap();
    assert!(engine.store().list_sessions(None).unwrap().is_empty());

    probe.release();
    start.await.unwrap().unwrap_err();
}

#[tokio::test]
async fn after_a_shutdown_request_starts_answer_503() {
    let (engine, probe) = engine("shutdown-closes");
    // A start with no `audio` opens real devices. It must never get that far,
    // whatever this test is checking: the provider is ahead of capture.
    probe.refuse_call.store(1, Ordering::SeqCst);
    let app = auricle_server::build_router(engine.clone(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();

    let shutdown = client
        .post(format!("{base}/api/v1/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(shutdown.status(), 202);

    let start = client
        .post(format!("{base}/api/v1/sessions"))
        .json(&serde_json::json!({"title": "too late"}))
        .send()
        .await
        .unwrap();

    assert_eq!(start.status(), 503);
    let body: serde_json::Value = start.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("shutting down"),
        "{body}"
    );
    assert_eq!(probe.entered.load(Ordering::SeqCst), 0);
    assert_eq!(engine.lifecycle_state(), "idle");
    assert!(engine.store().list_sessions(None).unwrap().is_empty());
}

// ---------- one daemon per data root ----------

#[tokio::test]
async fn a_data_root_takes_one_daemon_until_that_daemon_is_gone() {
    let (first, _probe) = engine("one-daemon");
    let root = first.data_root().to_path_buf();
    first
        .store()
        .create_session("live", "t", 1_700_000_000, "fake", &serde_json::json!({}))
        .unwrap();
    let options = || EngineOptions {
        cfg: Config::default(),
        data_root: root.clone(),
        provider_override: None,
    };

    let refused = Engine::new(options()).err().expect("the root is taken");
    assert!(refused.to_string().contains("already running"), "{refused}");
    let session = first.store().get_session("live").unwrap().unwrap();
    assert_eq!(session.ended_at, None, "recovery must not have run");
    assert!(
        session.meta.get("interrupted").is_none(),
        "{:?}",
        session.meta
    );

    // What a crash amounts to: the lock goes with the holder, and the next
    // daemon recovers the session the way it always did.
    drop(first);
    let next = Engine::new(options()).unwrap();
    let session = next.store().get_session("live").unwrap().unwrap();
    assert!(session.ended_at.is_some());
    assert_eq!(session.meta["interrupted"], true);
}

// ---------- a failed start must leave nothing behind ----------

#[tokio::test]
async fn second_session_failure_closes_the_first() {
    let (engine, probe) = engine("fail-second-open");
    probe.refuse_call.store(2, Ordering::SeqCst);

    engine.start_session(wav_params()).await.unwrap_err();

    assert_eq!(probe.opened(), 1);
    assert_eq!(probe.closed(), 1, "first stt session left open");
    // Free by the time the failure is reported: an immediate retry works.
    assert_eq!(engine.lifecycle_state(), "idle");
    let id = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &id).await;
}

#[tokio::test]
async fn device_failure_closes_the_stt_sessions() {
    let (engine, probe) = engine("fail-device");

    // Fails while resolving the name, before any stream is opened.
    let params = StartParams {
        mic_device: Some("no such device".into()),
        ..StartParams::default()
    };
    engine.start_session(params).await.unwrap_err();

    assert_eq!(probe.opened(), 2);
    assert_eq!(probe.closed(), 2, "stt sessions left open");
    assert_eq!(engine.lifecycle_state(), "idle");
}

#[tokio::test]
async fn persistence_failure_closes_the_stt_sessions() {
    let (engine, probe) = engine("fail-persist");
    let db = rusqlite::Connection::open(engine.data_root().join("auricle.db")).unwrap();
    db.execute_batch("ALTER TABLE sessions RENAME TO sessions_away")
        .unwrap();

    engine.start_session(wav_params()).await.unwrap_err();

    assert_eq!(probe.opened(), 2);
    assert_eq!(probe.closed(), 2, "stt sessions left open");
    assert_eq!(engine.lifecycle_state(), "idle");

    db.execute_batch("ALTER TABLE sessions_away RENAME TO sessions")
        .unwrap();
    let id = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &id).await;
}

// ---------- transcript holes stay on the record ----------

#[tokio::test]
async fn shed_chunks_are_recorded_in_the_session_meta() {
    let (engine, probe) = engine("dropped");
    *probe.drops.lock().unwrap() = SttDrops {
        chunks: 3,
        finals: 1,
    };

    let id = engine.start_session(wav_params()).await.unwrap();
    // Meta can change mid-session (a rename does this): the record must join
    // the current meta, not overwrite it with the copy from start.
    engine
        .store()
        .set_session_meta(&id, "edited", &serde_json::json!(true))
        .unwrap();
    stop_and_wait(&engine, &id).await;

    let session = engine.store().get_session(&id).unwrap().unwrap();
    assert_eq!(session.meta["dropped"]["mic"]["stt_chunks"], 3);
    assert_eq!(session.meta["dropped"]["mic"]["stt_finals"], 1);
    assert_eq!(session.meta["dropped"]["loopback"]["stt_chunks"], 3);
    assert_eq!(session.meta["edited"], true);
}

#[tokio::test]
async fn a_notice_with_a_number_in_it_is_not_a_loss() {
    let (engine, probe) = engine("wording");
    // "finished" and "pushed" both contain "shed".
    *probe.notice.lock().unwrap() = Some("finished 3 chunk(s), pushed 9 more".to_string());

    let id = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &id).await;

    let session = engine.store().get_session(&id).unwrap().unwrap();
    assert!(session.meta.get("dropped").is_none(), "{:?}", session.meta);
}

#[tokio::test]
async fn a_clean_session_records_no_losses() {
    let (engine, _probe) = engine("clean");

    let id = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &id).await;

    let session = engine.store().get_session(&id).unwrap().unwrap();
    assert!(session.meta.get("dropped").is_none(), "{:?}", session.meta);
}

// ---------- the session language ----------

fn set(engine: &Engine, key: &str, value: &str) {
    engine
        .store()
        .set_setting(key, &serde_json::json!(value))
        .unwrap();
}

#[tokio::test]
async fn the_language_setting_reaches_the_provider() {
    let (engine, probe) = engine("language-setting");
    set(&engine, "stt_language", "pt-BR");

    let id = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &id).await;

    assert_eq!(*probe.languages.lock().unwrap(), ["pt-BR", "pt-BR"]);
}

#[tokio::test]
async fn without_a_setting_the_language_comes_from_config() {
    let mut cfg = Config::default();
    cfg.stt.language = "de".to_string();
    let (engine, probe) = engine_with("language-config", cfg);

    let id = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &id).await;

    assert_eq!(*probe.languages.lock().unwrap(), ["de", "de"]);
}

#[tokio::test]
async fn a_malformed_language_is_refused_before_anything_opens() {
    let (engine, probe) = engine("language-bad");
    set(&engine, "stt_language", "en&model=x");

    let error = engine.start_session(wav_params()).await.unwrap_err();

    assert!(error.to_string().contains("en&model=x"), "{error}");
    assert_eq!(probe.entered.load(Ordering::SeqCst), 0);
    assert_eq!(engine.lifecycle_state(), "idle");
}

#[tokio::test]
async fn a_language_the_english_model_cannot_honour_is_warned_about() {
    let (engine, _probe) = engine("language-warned");
    set(&engine, "stt_language", "de");
    let mut events = engine.subscribe_important();

    let id = engine.start_session(wav_params()).await.unwrap();
    let warning = next_event(&mut events, |ev| matches!(ev, WsEvent::Error { .. })).await;
    stop_and_wait(&engine, &id).await;

    let WsEvent::Error { session, message } = warning else {
        unreachable!();
    };
    assert_eq!(session, id);
    assert!(message.contains("English-only"), "{message}");
}

#[tokio::test]
async fn a_multilingual_model_draws_no_warning() {
    let (engine, _probe) = engine("language-multilingual");
    set(&engine, "stt_language", "de");
    set(&engine, "whisper_model", "base");
    let mut events = engine.subscribe_important();

    let id = engine.start_session(wav_params()).await.unwrap();
    stop_and_wait(&engine, &id).await;

    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, WsEvent::Error { .. }), "{event:?}");
    }
}
