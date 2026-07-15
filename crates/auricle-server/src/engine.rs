//! Session engine: capture → pipeline → STT → assembler, driven by the
//! REST lifecycle. Finals are persisted as they assemble; partials are
//! broadcast only and never persisted.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use auricle_capture::{
    drain_into, start_loopback, start_mic, CaptureConsumer, CaptureHandle, MonoResampler,
};
use auricle_core::{ChannelId, ChunkKind, Config, Error, Result, SttEvent};
use auricle_pipeline::{Assembler, ChannelPipeline, PipelineConfig, PipelineEvent, SileroDetector};
use auricle_stt::{SessionCfg, SttKind, SttProvider, SttSession};
use tokio::sync::{broadcast, mpsc};

use crate::events::WsEvent;
use crate::lifecycle::{Lifecycle, LifecycleError};
use crate::store::Store;

const TARGET_RATE_HZ: u32 = 16_000;
/// Finals + lifecycle events: sized so a consumer must fall an entire
/// meeting behind before anything important is lost.
const IMPORTANT_CAPACITY: usize = 1024;
/// Partials are transient by design: a slow consumer just misses some.
const PARTIAL_CAPACITY: usize = 16;

/// Where session audio comes from.
#[derive(Debug, Clone)]
pub enum AudioSource {
    /// Real capture using the configured/selected devices.
    Devices,
    /// Stream a 16 kHz mono WAV into one channel (integration tests,
    /// offline transcription). Runs faster than real time.
    WavFile { path: PathBuf, channel: ChannelId },
}

pub struct EngineOptions {
    pub cfg: Config,
    pub data_root: PathBuf,
    /// Test hook: use this provider for every session instead of the
    /// registry (no models, no keys, no network).
    pub provider_override: Option<Arc<dyn SttProvider>>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct StartParams {
    pub title: Option<String>,
    pub stt_provider: Option<String>,
    pub mic_device: Option<String>,
    pub loopback_device: Option<String>,
    #[serde(skip)]
    pub audio: Option<AudioSource>,
}

#[derive(Debug)]
pub enum StartError {
    /// Another session is active — HTTP 409.
    Busy(String),
    /// Bad provider/device names — HTTP 400.
    Invalid(String),
    Internal(Error),
}

#[derive(Debug)]
pub enum StopError {
    /// The named session is not the active one — HTTP 409.
    NotActive,
}

struct ActiveSession {
    id: String,
    stop: Arc<AtomicBool>,
    handles: Arc<Mutex<Vec<CaptureHandle>>>,
}

pub struct Engine {
    cfg: Config,
    data_root: PathBuf,
    store: Arc<Store>,
    lifecycle: Mutex<Lifecycle>,
    active: Mutex<Option<ActiveSession>>,
    important_tx: broadcast::Sender<WsEvent>,
    partial_tx: broadcast::Sender<WsEvent>,
    provider_override: Option<Arc<dyn SttProvider>>,
    /// Last constructed local provider, keyed by "provider-id|model".
    /// Loading a whisper model reads 148–500 MB from disk; without this
    /// every session start paid that again.
    local_provider: Mutex<Option<(String, Arc<dyn SttProvider>)>>,
}

impl Engine {
    pub fn new(opts: EngineOptions) -> Result<Arc<Engine>> {
        let store = Arc::new(Store::open(&opts.data_root.join("auricle.db"))?);
        // Crash recovery belongs to the lifecycle owner: only the engine may
        // declare a still-open session dead (read-only openers like
        // `auricle export` must not).
        store.recover_interrupted()?;
        let (important_tx, _) = broadcast::channel(IMPORTANT_CAPACITY);
        let (partial_tx, _) = broadcast::channel(PARTIAL_CAPACITY);
        Ok(Arc::new(Engine {
            cfg: opts.cfg,
            data_root: opts.data_root,
            store,
            lifecycle: Mutex::new(Lifecycle::new()),
            active: Mutex::new(None),
            important_tx,
            partial_tx,
            provider_override: opts.provider_override,
            local_provider: Mutex::new(None),
        }))
    }

    pub fn store(&self) -> Arc<Store> {
        self.store.clone()
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn subscribe_important(&self) -> broadcast::Receiver<WsEvent> {
        self.important_tx.subscribe()
    }

    /// Publish on the important lane (ask answer mirroring). No receivers
    /// is not an error — nobody may be watching the socket.
    pub(crate) fn publish_important(&self, ev: WsEvent) {
        let _ = self.important_tx.send(ev);
    }

    pub fn subscribe_partials(&self) -> broadcast::Receiver<WsEvent> {
        self.partial_tx.subscribe()
    }

    pub fn active_session(&self) -> Option<String> {
        self.lifecycle
            .lock()
            .unwrap()
            .active_session()
            .map(|s| s.to_string())
    }

    pub fn lifecycle_state(&self) -> &'static str {
        match self.lifecycle.lock().unwrap().state() {
            crate::lifecycle::State::Idle => "idle",
            crate::lifecycle::State::Recording(_) => "recording",
            crate::lifecycle::State::Stopping(_) => "stopping",
        }
    }

    /// Start a session: reserve the lifecycle slot, build the provider and
    /// capture, persist the session row, spawn the drive tasks.
    pub async fn start_session(
        self: &Arc<Self>,
        params: StartParams,
    ) -> std::result::Result<String, StartError> {
        let session_id = format!("s{:x}", unix_ms());
        if let Err(LifecycleError::Busy(active)) = self.lifecycle.lock().unwrap().start(&session_id)
        {
            return Err(StartError::Busy(active));
        }
        match self.start_session_inner(&session_id, params).await {
            Ok(()) => Ok(session_id.clone()),
            Err(e) => {
                self.lifecycle.lock().unwrap().finish_stop(&session_id);
                Err(e)
            }
        }
    }

    async fn start_session_inner(
        self: &Arc<Self>,
        session_id: &str,
        params: StartParams,
    ) -> std::result::Result<(), StartError> {
        // Settings stored via PUT /settings overlay the TOML config so the
        // web UI's Settings view actually controls behavior. Precedence:
        // request params > settings store > config file.
        let settings = self.store.all_settings().unwrap_or_default();
        let setting_str = |key: &str| {
            settings
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let provider_id = params
            .stt_provider
            .clone()
            .or_else(|| setting_str("default_provider"))
            .unwrap_or_else(|| self.cfg.stt.provider.clone());
        let retain_raw_audio = settings
            .get("retain_raw_audio")
            .and_then(|v| v.as_bool())
            .unwrap_or(self.cfg.audio.retain_raw_audio);
        let mut provider_cfg = self.cfg.clone();
        if let Some(model) = setting_str("whisper_model") {
            provider_cfg.stt.whisper_local.model = model;
        }

        let cache_key = format!("{provider_id}|{}", provider_cfg.stt.whisper_local.model);
        let cached = match &*self.local_provider.lock().unwrap() {
            Some((key, p)) if *key == cache_key => Some(p.clone()),
            _ => None,
        };
        let provider: Arc<dyn SttProvider> = match (&self.provider_override, cached) {
            (Some(p), _) => p.clone(),
            (None, Some(p)) => p,
            (None, None) => {
                let models_dir = self.data_root.join("models");
                // First run of a local model blocks this request on a large
                // download; tell WS clients so the UI isn't a silent hang.
                if provider_id == "whisper-local" {
                    let model_name = &provider_cfg.stt.whisper_local.model;
                    if let Some(m) = auricle_stt::available_models()
                        .iter()
                        .find(|m| m.name == *model_name)
                    {
                        if !models_dir.join(m.file_name).exists() {
                            let _ = self.important_tx.send(WsEvent::ModelDownloadStarted {
                                session: session_id.to_string(),
                                model: model_name.clone(),
                                size_mb: m.size_bytes / 1_048_576,
                            });
                        }
                    }
                }
                let p = auricle_stt::create_provider(&provider_id, &provider_cfg, &models_dir)
                    .await
                    .map_err(|e| match e {
                        Error::Stt(ref m) | Error::Model(ref m) if m.contains("unknown") => {
                            StartError::Invalid(e.to_string())
                        }
                        Error::Stt(ref m) if m.contains("not set") => {
                            StartError::Invalid(e.to_string())
                        }
                        _ => StartError::Internal(e),
                    })?;
                // Only local providers are worth caching (model load); cloud
                // ones are cheap and re-read their key env on construction.
                if p.kind() == SttKind::Local {
                    *self.local_provider.lock().unwrap() = Some((cache_key, p.clone()));
                }
                p
            }
        };

        let session_cfg = SessionCfg {
            session_id: session_id.to_string(),
            language: "en".to_string(),
        };
        let mic_session = provider
            .start_session(&session_cfg)
            .await
            .map_err(StartError::Internal)?;
        let loop_session = provider
            .start_session(&session_cfg)
            .await
            .map_err(StartError::Internal)?;

        let pipe_cfg = PipelineConfig::default();
        let mic_pipeline = ChannelPipeline::new(
            session_id,
            ChannelId::Mic,
            SileroDetector::new().map_err(StartError::Internal)?,
            &pipe_cfg,
        );
        let loop_pipeline = ChannelPipeline::new(
            session_id,
            ChannelId::Loopback,
            SileroDetector::new().map_err(StartError::Internal)?,
            &pipe_cfg,
        );

        // No explicit title: start as a placeholder and mark it auto-titled
        // — once the session stops, a 3–6 word title is generated from the
        // transcript (crate::title). User renames clear the flag.
        let auto_title = params.title.is_none();
        let title = params
            .title
            .clone()
            .unwrap_or_else(|| crate::title::UNTITLED.to_string());
        let source = params.audio.clone().unwrap_or(AudioSource::Devices);
        let stop = Arc::new(AtomicBool::new(false));
        let handles = Arc::new(Mutex::new(Vec::new()));
        let (mic_batch_tx, mic_batch_rx) = mpsc::unbounded_channel();
        let (loop_batch_tx, loop_batch_rx) = mpsc::unbounded_channel();

        // Raw-audio retention (privacy default: off).
        let mut meta = serde_json::json!({});
        if auto_title {
            meta["auto_title"] = serde_json::json!(true);
        }
        let mut mic_wav = None;
        let mut loop_wav = None;
        if retain_raw_audio {
            let dir = self.data_root.join("sessions").join(session_id);
            std::fs::create_dir_all(&dir).map_err(|e| StartError::Internal(Error::Io(e)))?;
            let mic_path = dir.join("mic_16k.wav");
            let loop_path = dir.join("loopback_16k.wav");
            mic_wav = Some(open_wav(&mic_path).map_err(StartError::Internal)?);
            loop_wav = Some(open_wav(&loop_path).map_err(StartError::Internal)?);
            meta["audio"] = serde_json::json!({
                "mic": mic_path.to_string_lossy(),
                "loopback": loop_path.to_string_lossy(),
            });
        }

        let mut drain_joins: Vec<std::thread::JoinHandle<()>> = Vec::new();
        let session_clock = Instant::now();
        match &source {
            AudioSource::Devices => {
                let mic_sel = params
                    .mic_device
                    .clone()
                    .or_else(|| setting_str("mic_device"))
                    .unwrap_or_else(|| self.cfg.audio.mic_device.clone());
                let loop_sel = params
                    .loopback_device
                    .clone()
                    .or_else(|| setting_str("loopback_device"))
                    .unwrap_or_else(|| self.cfg.audio.loopback_device.clone());
                let (mh, mc) = start_mic(&mic_sel).map_err(map_device_err)?;
                let (lh, lc) = start_loopback(&loop_sel).map_err(map_device_err)?;
                meta["devices"] = serde_json::json!({
                    "mic": mh.device_name,
                    "loopback": lh.device_name,
                });
                handles.lock().unwrap().extend([mh, lh]);
                let vu = VuSink {
                    session: session_id.to_string(),
                    tx: self.partial_tx.clone(),
                };
                drain_joins.push(spawn_drain(
                    mc,
                    stop.clone(),
                    session_clock,
                    mic_batch_tx,
                    mic_wav,
                    vu.clone(),
                ));
                drain_joins.push(spawn_drain(
                    lc,
                    stop.clone(),
                    session_clock,
                    loop_batch_tx,
                    loop_wav,
                    vu,
                ));
            }
            AudioSource::WavFile { path, channel } => {
                let (tx, _other_tx) = match channel {
                    ChannelId::Mic => (mic_batch_tx, loop_batch_tx),
                    ChannelId::Loopback => (loop_batch_tx, mic_batch_tx),
                };
                // The unused channel's sender drops here: its driver sees a
                // closed stream and finishes immediately.
                tokio::spawn(feed_wav(path.clone(), tx, stop.clone()));
            }
        }

        self.store
            .create_session(session_id, &title, unix_secs(), provider.id(), &meta)
            .map_err(StartError::Internal)?;
        let _ = self.important_tx.send(WsEvent::SessionStarted {
            session: session_id.to_string(),
            title,
            stt_provider: provider.id().to_string(),
        });

        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let mic_driver = tokio::spawn(drive_channel(
            mic_batch_rx,
            mic_pipeline,
            mic_session,
            ChannelId::Mic,
            ev_tx.clone(),
        ));
        let loop_driver = tokio::spawn(drive_channel(
            loop_batch_rx,
            loop_pipeline,
            loop_session,
            ChannelId::Loopback,
            ev_tx,
        ));
        let collector = tokio::spawn(collect_events(
            ev_rx,
            session_id.to_string(),
            provider.id().to_string(),
            self.store.clone(),
            self.important_tx.clone(),
            self.partial_tx.clone(),
            session_clock,
        ));

        *self.active.lock().unwrap() = Some(ActiveSession {
            id: session_id.to_string(),
            stop: stop.clone(),
            handles: handles.clone(),
        });

        // Supervisor: waits for the drive tasks, finalizes persistence and
        // lifecycle, announces the stop.
        let engine = self.clone();
        let session = session_id.to_string();
        tokio::spawn(async move {
            let _ = mic_driver.await;
            let _ = loop_driver.await;
            let _ = collector.await;
            for join in drain_joins {
                let _ = tokio::task::spawn_blocking(move || join.join()).await;
            }
            {
                let mut handles = handles.lock().unwrap();
                for h in handles.iter() {
                    if let Some(e) = h.stream_error() {
                        let _ = engine.important_tx.send(WsEvent::DeviceLost {
                            session: session.clone(),
                            channel: h.channel.to_string(),
                            message: e,
                        });
                    }
                }
                handles.clear(); // drop streams
            }
            if let Err(e) = engine.store.end_session(&session, unix_secs()) {
                let _ = engine.important_tx.send(WsEvent::Error {
                    session: session.clone(),
                    message: format!("persisting session end: {e}"),
                });
            }
            let _ = engine.important_tx.send(WsEvent::SessionStopped {
                session: session.clone(),
            });
            engine.lifecycle.lock().unwrap().finish_stop(&session);
            *engine.active.lock().unwrap() = None;
            // Post-stop: name the session from its transcript (async; the
            // sidebar learns about it via a session_updated event).
            tokio::spawn(crate::title::autotitle_session(
                engine.store.clone(),
                engine.cfg.clone(),
                session,
                engine.important_tx.clone(),
            ));
        });

        Ok(())
    }

    /// Begin stopping: pause capture, signal the drains; the supervisor
    /// completes persistence and flips the lifecycle back to idle. Returns
    /// as soon as the state is `stopping`.
    pub fn stop_session(&self, id: &str) -> std::result::Result<(), StopError> {
        self.lifecycle
            .lock()
            .unwrap()
            .begin_stop(id)
            .map_err(|_| StopError::NotActive)?;
        if let Some(active) = self.active.lock().unwrap().as_ref() {
            if active.id == id {
                for h in active.handles.lock().unwrap().iter() {
                    let _ = h.pause();
                }
                active.stop.store(true, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

fn map_device_err(e: Error) -> StartError {
    match &e {
        Error::DeviceNotFound { .. } | Error::NoDefaultDevice(_) => {
            StartError::Invalid(e.to_string())
        }
        _ => StartError::Internal(e),
    }
}

pub(crate) fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

type WavWriter = hound::WavWriter<std::io::BufWriter<std::fs::File>>;

/// Retained audio is 16-bit PCM: half the disk of f32 for inaudible loss at
/// 16 kHz, and the WAV codec browsers handle best (the playback bar streams
/// these files straight into an `<audio>` element).
fn open_wav(path: &std::path::Path) -> Result<WavWriter> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE_HZ,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    hound::WavWriter::create(path, spec)
        .map_err(|e| Error::Io(std::io::Error::other(format!("{}: {e}", path.display()))))
}

fn write_wav_samples(w: &mut WavWriter, samples: &[f32]) {
    for s in samples {
        let _ = w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16);
    }
}

/// Broadcasts per-channel RMS at ~10 Hz on the lossy partial lane
/// (architecture §4.10: VU meters).
#[derive(Clone)]
struct VuSink {
    session: String,
    tx: broadcast::Sender<WsEvent>,
}

const VU_INTERVAL: Duration = Duration::from_millis(100);

/// Drain thread: ring buffer → resampler → batch channel (+ optional
/// retained WAV, + VU events). Mirrors the Phase 2 CLI drain.
fn spawn_drain(
    mut c: CaptureConsumer,
    stop: Arc<AtomicBool>,
    clock: Instant,
    tx: mpsc::UnboundedSender<(Vec<f32>, u64)>,
    mut wav: Option<WavWriter>,
    vu: VuSink,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(mut resampler) = MonoResampler::new(c.native_rate_hz, TARGET_RATE_HZ) else {
            return;
        };
        let channel_name = c.channel.to_string();
        let mut scratch: Vec<f32> = Vec::new();
        let mut out: Vec<f32> = Vec::new();
        let mut sq_sum = 0f64;
        let mut sq_count = 0usize;
        let mut last_vu = Instant::now();
        loop {
            // ~10 Hz VU tick; RMS 0 when the window saw no samples (a
            // silent loopback delivers nothing at all — Phase 1 finding).
            if last_vu.elapsed() >= VU_INTERVAL {
                let rms = if sq_count > 0 {
                    (sq_sum / sq_count as f64).sqrt() as f32
                } else {
                    0.0
                };
                let _ = vu.tx.send(WsEvent::Vu {
                    session: vu.session.clone(),
                    channel: channel_name.clone(),
                    rms,
                });
                sq_sum = 0.0;
                sq_count = 0;
                last_vu = Instant::now();
            }
            scratch.clear();
            let n = drain_into(&mut c.consumer, &mut scratch);
            if n > 0 {
                for s in &scratch {
                    sq_sum += (*s as f64) * (*s as f64);
                }
                sq_count += scratch.len();
                out.clear();
                if resampler.process(&scratch, &mut out).is_err() {
                    break;
                }
                if !out.is_empty() {
                    if let Some(w) = wav.as_mut() {
                        write_wav_samples(w, &out);
                    }
                    if tx
                        .send((out.clone(), clock.elapsed().as_millis() as u64))
                        .is_err()
                    {
                        break;
                    }
                }
            } else if stop.load(Ordering::Relaxed) {
                out.clear();
                let _ = resampler.finish(&mut out);
                if !out.is_empty() {
                    if let Some(w) = wav.as_mut() {
                        write_wav_samples(w, &out);
                    }
                    let _ = tx.send((out.clone(), clock.elapsed().as_millis() as u64));
                }
                break;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if let Some(w) = wav {
            let _ = w.finalize();
        }
    })
}

/// Test/offline audio source: stream a 16 kHz mono WAV in 10 ms batches at
/// ~30× real time, then hold the channel open until stop (so the REST stop
/// path drives shutdown, exactly like a live session).
async fn feed_wav(
    path: PathBuf,
    tx: mpsc::UnboundedSender<(Vec<f32>, u64)>,
    stop: Arc<AtomicBool>,
) {
    let samples: Vec<f32> = match hound::WavReader::open(&path) {
        Ok(mut r) => match r.spec().sample_format {
            hound::SampleFormat::Float => r.samples::<f32>().filter_map(|s| s.ok()).collect(),
            hound::SampleFormat::Int => r
                .samples::<i16>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / 32768.0)
                .collect(),
        },
        Err(_) => return,
    };
    let clock = Instant::now();
    for batch in samples.chunks(160) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Ideal wall time: sample-position ms. The real clock lags behind
        // (we run faster than real time), which the Timeline treats as a
        // catch-up burst, not a gap.
        if tx
            .send((batch.to_vec(), clock.elapsed().as_millis() as u64))
            .is_err()
        {
            return;
        }
        if batch.len() == 160 {
            tokio::time::sleep(Duration::from_micros(300)).await;
        }
    }
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Bound on a stopping session's `finish()`. Generous — a slow CPU may
/// legitimately still be transcribing a full queue of finals — but finite:
/// a wedged provider (e.g. a network stall mid-close) must not stick the
/// lifecycle in `stopping` forever.
const FINISH_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound on draining trailing events after finish, for the same reason.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-channel driver: batches → VAD/chunker → STT session, with the
/// Phase 2 interim backpressure. (No runtime swap here: the provider is a
/// per-session setting in the API; the hotkey swap remains a CLI feature.)
async fn drive_channel(
    mut batch_rx: mpsc::UnboundedReceiver<(Vec<f32>, u64)>,
    mut pipeline: ChannelPipeline<SileroDetector>,
    mut session: Box<dyn SttSession>,
    channel: ChannelId,
    ev_tx: mpsc::UnboundedSender<(ChannelId, SttEvent)>,
) {
    let mut outstanding: i64 = 0;
    'feed: loop {
        // Wait for audio or an STT event, whichever is ready first — the
        // old 50 ms poll here held arriving audio back for up to a poll
        // interval, a real bite out of the capture→partial budget. The
        // event arm never touches the session, so the `next_event` borrow
        // ends with the select and feeding can proceed after it; every
        // implementation's next_event is a cancel-safe mpsc recv.
        let received = tokio::select! {
            batch = batch_rx.recv() => batch,
            ev = session.next_event() => {
                match ev {
                    Some(ev) => {
                        if matches!(ev, SttEvent::Final(_)) {
                            outstanding -= 1;
                        }
                        let _ = ev_tx.send((channel, ev));
                        continue 'feed;
                    }
                    // Worker gone mid-session: stop feeding, settle below.
                    None => break 'feed,
                }
            }
        };
        let Some(first) = received else {
            break 'feed; // capture side closed: session is stopping
        };
        let mut batches = vec![first];
        while let Ok(b) = batch_rx.try_recv() {
            batches.push(b);
        }
        for (samples, wall_ms) in batches {
            for chunk in pipeline.push_batch(&samples, wall_ms) {
                let should_feed = match chunk.kind {
                    ChunkKind::Final => {
                        outstanding += 1;
                        true
                    }
                    ChunkKind::Interim => outstanding <= 1,
                };
                if should_feed && session.feed(chunk).await.is_err() {
                    let _ = ev_tx.send((
                        channel,
                        SttEvent::Error("stt session rejected audio".to_string()),
                    ));
                    break 'feed;
                }
            }
        }
    }
    // Close any open span; feeding a dead session is a harmless no-op.
    for chunk in pipeline.flush() {
        if chunk.kind == ChunkKind::Final {
            let _ = session.feed(chunk).await;
        }
    }
    match tokio::time::timeout(FINISH_TIMEOUT, session.finish()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let _ = ev_tx.send((channel, SttEvent::Error(e.to_string())));
        }
        Err(_) => {
            // Abandon the wedged session so the stop can complete; its
            // worker task dies with the process.
            let _ = ev_tx.send((
                channel,
                SttEvent::Error(format!(
                    "stt session did not finish within {}s; abandoning it",
                    FINISH_TIMEOUT.as_secs()
                )),
            ));
            return;
        }
    }
    let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, session.next_event()).await {
        let _ = ev_tx.send((channel, ev));
    }
}

/// Single consumer for both channels: assembles (dedup, speaker tags),
/// persists finals, broadcasts finals + partials + errors.
#[allow(clippy::too_many_arguments)]
async fn collect_events(
    mut ev_rx: mpsc::UnboundedReceiver<(ChannelId, SttEvent)>,
    session_id: String,
    provider_id: String,
    store: Arc<Store>,
    important_tx: broadcast::Sender<WsEvent>,
    partial_tx: broadcast::Sender<WsEvent>,
    session_clock: Instant,
) {
    let mut assembler = Assembler::new(&session_id);
    while let Some((channel, ev)) = ev_rx.recv().await {
        if let SttEvent::Error(message) = &ev {
            let _ = important_tx.send(WsEvent::Error {
                session: session_id.clone(),
                message: format!("{channel}: {message}"),
            });
            continue;
        }
        let Some(PipelineEvent::Transcript(t)) = assembler.ingest(ev) else {
            continue;
        };
        if t.is_final {
            if let Err(e) = store.insert_segment(
                &session_id,
                t.channel,
                &t.speaker,
                t.t_start_ms as i64,
                t.t_end_ms as i64,
                &t.text,
                &provider_id,
            ) {
                let _ = important_tx.send(WsEvent::Error {
                    session: session_id.clone(),
                    message: format!("persisting segment: {e}"),
                });
            }
            let _ = important_tx.send(WsEvent::Final {
                session: session_id.clone(),
                channel: t.channel.to_string(),
                speaker: t.speaker,
                t_start_ms: t.t_start_ms,
                t_end_ms: t.t_end_ms,
                text: t.text,
                latency_ms: (session_clock.elapsed().as_millis() as u64).saturating_sub(t.t_end_ms),
            });
        } else {
            let _ = partial_tx.send(WsEvent::Partial {
                session: session_id.clone(),
                channel: t.channel.to_string(),
                speaker: t.speaker,
                t_start_ms: t.t_start_ms,
                t_end_ms: t.t_end_ms,
                text: t.text,
            });
        }
    }
}
