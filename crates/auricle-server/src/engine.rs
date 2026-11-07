//! Session engine: capture → pipeline → STT → assembler, driven by the
//! REST lifecycle. Finals are persisted as they assemble; partials are
//! broadcast only and never persisted.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use auricle_capture::{drain_into, start_loopback, start_mic, CaptureConsumer, MonoResampler};
use auricle_core::{AudioChunk, ChannelId, ChunkKind, Config, Error, Result, SttEvent};
use auricle_pipeline::{
    Assembler, ChannelPipeline, PipelineConfig, PipelineEvent, SileroDetector, SpeechDetector,
    Timeline,
};
use auricle_stt::{SessionCfg, SttDrops, SttKind, SttProvider, SttSession};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::ask::AskHistory;
use crate::events::WsEvent;
use crate::lifecycle::{Lifecycle, LifecycleError, SessionControl};
use crate::retention::{SweepClock, SECS_PER_DAY};
use crate::ring::TranscriptRing;
use crate::store::Store;

const TARGET_RATE_HZ: u32 = 16_000;
/// Finals + lifecycle events: sized so a consumer must fall an entire
/// meeting behind before anything important is lost.
const IMPORTANT_CAPACITY: usize = 1024;
/// Partials are transient by design: a slow consumer just misses some.
const PARTIAL_CAPACITY: usize = 16;
/// The setting a sweep's `SweepClock` is kept in. Shows up in `GET /settings`
/// like any other key; nothing but the sweep reads it.
const RETENTION_CLOCK_SETTING: &str = "retention_clock";

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
    /// A stop landed before capture began. The request was fine and the
    /// slot has been released, so this is a conflict, not a bad request.
    Stopped,
    /// The daemon is on its way out — HTTP 503.
    ShuttingDown,
    Internal(Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Busy(active) => write!(f, "session {active} is already active"),
            StartError::Invalid(message) => f.write_str(message),
            StartError::Stopped => f.write_str("session was stopped before capture began"),
            StartError::ShuttingDown => f.write_str("the daemon is shutting down"),
            StartError::Internal(e) => write!(f, "{e}"),
        }
    }
}

/// What a retention sweep did.
#[derive(Debug, Default)]
pub struct Sweep {
    pub deleted: usize,
    /// Why it did less than it was asked to, for the log. Never names a meeting.
    pub notice: Option<String>,
}

#[derive(Debug)]
pub enum StopError {
    /// The named session is not the active one — HTTP 409.
    NotActive,
}

/// Where a session's batches will come from once it is running.
enum Feed {
    Devices(CaptureConsumer, CaptureConsumer),
    File(PathBuf, ChannelId),
}

/// A session past everything that can fail: its row exists and its capture is
/// open, but no thread or task runs yet.
struct OpenCapture {
    title: String,
    clock: Instant,
    feed: Feed,
    mic_pipeline: ChannelPipeline<SileroDetector>,
    loop_pipeline: ChannelPipeline<SileroDetector>,
    mic_wav: Option<RetainedAudio>,
    loop_wav: Option<RetainedAudio>,
}

/// Holds the reserved slot for whatever owns the session right now, first its
/// start and then its supervisor: if that fails or panics, nothing else would
/// release it. Disarmed when the owner hands over or releases the slot itself.
struct SlotGuard {
    engine: Arc<Engine>,
    id: String,
    disarmed: bool,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        self.engine.lifecycle.lock().unwrap().finish_stop(&self.id);
        // Clients can know of this session without having asked for it (via
        // /health, after a reload mid-start), and this is their only signal
        // that it is over.
        let _ = self.engine.important_tx.send(WsEvent::SessionStopped {
            session: self.id.clone(),
        });
    }
}

pub struct Engine {
    cfg: Config,
    data_root: PathBuf,
    store: Arc<Store>,
    lifecycle: Mutex<Lifecycle>,
    important_tx: broadcast::Sender<WsEvent>,
    partial_tx: broadcast::Sender<WsEvent>,
    provider_override: Option<Arc<dyn SttProvider>>,
    /// Last constructed local provider, keyed by "provider-id|model".
    /// Loading a whisper model reads 148–500 MB from disk; without this
    /// every session start paid that again.
    local_provider: Mutex<Option<(String, Arc<dyn SttProvider>)>>,
    /// Post-stop auto-titles still running. Shutdown gives them a moment: a
    /// title is generated once, and exiting mid-call loses it for good.
    titles_pending: AtomicUsize,
    /// Rolling window of finals feeding /ask and MCP. Owned here, not by the
    /// router, so that every delete reaches it, retention's included.
    ring: Arc<TranscriptRing>,
    ask_history: Arc<AskHistory>,
    /// Holds the data root's lock for as long as this engine lives.
    _data_root_lock: std::fs::File,
}

/// Claim `root` for one daemon. The OS releases the lock however the process
/// ends, so a crash leaves nothing to clean up, and the file is never removed:
/// a second daemon would otherwise lock a new file at the same path.
fn lock_data_root(root: &std::path::Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(root)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("daemon.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Io(std::io::Error::other(format!(
            "another auricle daemon is already running on {}; stop it before starting this one",
            root.display()
        )))),
        Err(std::fs::TryLockError::Error(e)) => Err(Error::Io(e)),
    }
}

impl Engine {
    pub fn new(opts: EngineOptions) -> Result<Arc<Engine>> {
        // Before recovery, which would close a running daemon's live session:
        // a different port gets a second daemon this far.
        let data_root_lock = lock_data_root(&opts.data_root)?;
        let store = Arc::new(Store::open(&opts.data_root.join("auricle.db"))?);
        // Crash recovery belongs to the lifecycle owner: only the engine may
        // declare a still-open session dead (read-only openers like
        // `auricle export` must not).
        store.recover_interrupted()?;
        let (important_tx, _) = broadcast::channel(IMPORTANT_CAPACITY);
        let (partial_tx, _) = broadcast::channel(PARTIAL_CAPACITY);
        let ring = TranscriptRing::new(opts.cfg.copilot.transcript_window_min);
        Ok(Arc::new(Engine {
            cfg: opts.cfg,
            data_root: opts.data_root,
            store,
            lifecycle: Mutex::new(Lifecycle::new()),
            important_tx,
            partial_tx,
            provider_override: opts.provider_override,
            local_provider: Mutex::new(None),
            titles_pending: AtomicUsize::new(0),
            ring,
            ask_history: Arc::default(),
            _data_root_lock: data_root_lock,
        }))
    }

    pub fn store(&self) -> Arc<Store> {
        self.store.clone()
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub(crate) fn ring(&self) -> Arc<TranscriptRing> {
        self.ring.clone()
    }

    pub(crate) fn ask_history(&self) -> Arc<AskHistory> {
        self.ask_history.clone()
    }

    /// The data root this engine writes under (DB, models/, sessions/).
    pub fn data_root(&self) -> &std::path::Path {
        &self.data_root
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

    /// Refuse every start from here on. The daemon is leaving, so a session
    /// begun now would be cut off and recorded as a crash.
    pub(crate) fn begin_shutdown(&self) {
        self.lifecycle.lock().unwrap().close();
    }

    /// Whether `id` got as far as opening capture, so that it has a session to
    /// wind down. A start stopped before this point just ends.
    pub(crate) fn capture_began(&self, id: &str) -> bool {
        self.lifecycle.lock().unwrap().capture_began(id)
    }

    /// Resolves once no post-stop auto-title is still running.
    pub(crate) async fn titles_settled(&self) {
        while self.titles_pending.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Delete a session: its retained audio, which lives outside the database,
    /// then its rows, then what memory still holds of it. Returns false when
    /// the id does not exist. Blocks on all of it, so call it off the async
    /// runtime.
    ///
    /// Audio goes first so that no delete can succeed and leave it behind. One
    /// that fails keeps the rows: the meeting is still listed, and deleting it
    /// again works, where a row already gone would answer only 404.
    pub(crate) fn delete_session(&self, id: &str) -> Result<bool> {
        if self.store.get_session(id)?.is_none() {
            return Ok(false);
        }
        // Build the dir from this engine's own data root (not the process
        // default — an engine on a custom root would otherwise orphan its
        // WAVs), and only when the id is a single path component so a
        // tampered id cannot escape the sessions dir via `..` or separators.
        let audio_dir = {
            let mut comps = std::path::Path::new(id).components();
            match (comps.next(), comps.next()) {
                (Some(std::path::Component::Normal(_)), None) => {
                    Some(self.data_root.join("sessions").join(id))
                }
                _ => None,
            }
        };
        if let Some(dir) = audio_dir {
            match std::fs::remove_dir_all(dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(Error::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "could not remove the retained audio, so the meeting was kept: {e}"
                        ),
                    )));
                }
                _ => {}
            }
        }
        let deleted = self.store.delete_session(id)?;
        self.ring.purge(id);
        self.ask_history.clear(id);
        Ok(deleted)
    }

    /// Delete every session that ended more than `retention_days` ago, with
    /// its retained audio, and say how it went. Zero, unset, or anything that
    /// is not a positive integer means retention is off: a negative value would
    /// put the cutoff in the future and take everything.
    ///
    /// Deletes nothing while the clock looks wrong (see `crate::retention`).
    /// The clock is read even with retention off, so that switching it on
    /// doesn't find a stale record and take that for a jump.
    ///
    /// Blocks on the store, so call it off the async runtime.
    pub fn sweep_retention(&self) -> Result<Sweep> {
        self.sweep_retention_at(unix_secs())
    }

    /// `sweep_retention` reading `now` as the time. Exposed for the
    /// integration tests, which have no other way to move the clock.
    pub fn sweep_retention_at(&self, now: i64) -> Result<Sweep> {
        let settings = self.store.all_settings()?;
        let kept = settings
            .get(RETENTION_CLOCK_SETTING)
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        // With no record, the newest meeting is the last time known to be good.
        let clock = match kept {
            Some(clock) => clock,
            None => SweepClock::last_read(self.store.latest_session_end()?.unwrap_or(now)),
        };
        let (clock, held) = clock.observe(now);
        // Ahead of any delete: a crash mid-sweep must not forget a hold.
        self.store
            .set_setting(RETENTION_CLOCK_SETTING, &serde_json::json!(clock))?;

        let days = settings.get("retention_days").and_then(|v| v.as_i64());
        let Some(days) = days.filter(|days| *days > 0) else {
            return Ok(Sweep::default());
        };
        if held.is_some() {
            return Ok(Sweep {
                deleted: 0,
                notice: held,
            });
        }
        let cutoff = now.saturating_sub(days.saturating_mul(SECS_PER_DAY));
        let active = self.active_session();
        let mut sweep = Sweep::default();
        let mut failed = 0;
        for (id, _) in self.store.sessions_older_than(cutoff, now)? {
            if active.as_deref() == Some(id.as_str()) {
                continue;
            }
            // One meeting that won't go must not keep the rest past their date.
            match self.delete_session(&id) {
                Ok(true) => sweep.deleted += 1,
                Ok(false) => {}
                Err(e) => {
                    failed += 1;
                    sweep.notice = Some(format!(
                        "retention: {failed} session(s) could not be deleted, next sweep retries: {e}"
                    ));
                }
            }
        }
        Ok(sweep)
    }

    /// Start a session: reserve the lifecycle slot, then build the provider
    /// and capture, persist the session row, and spawn the drive tasks.
    ///
    /// The work runs in its own task. axum drops a handler's future when its
    /// client disconnects, and a dashboard reload during a long model
    /// download must not abandon a half-built session that holds the slot:
    /// dropping this future only stops waiting for the outcome.
    pub async fn start_session(
        self: &Arc<Self>,
        params: StartParams,
    ) -> std::result::Result<String, StartError> {
        let session_id = format!("s{:x}", unix_ms());
        let control = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            match lifecycle.start(&session_id) {
                Ok(()) => lifecycle.control(),
                Err(LifecycleError::Busy(active)) => return Err(StartError::Busy(active)),
                Err(_) => return Err(StartError::ShuttingDown),
            }
        };
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let engine = self.clone();
        let id = session_id.clone();
        tokio::spawn(async move {
            let mut guard = SlotGuard {
                engine: engine.clone(),
                id: id.clone(),
                disarmed: false,
            };
            let outcome = engine.start_session_inner(&id, params, control).await;
            guard.disarmed = outcome.is_ok();
            // Before replying: the slot must be free when the caller hears why.
            drop(guard);
            if let Err(Err(e)) = outcome_tx.send(outcome) {
                // The caller hung up, so this is the failure's only report.
                engine.publish_important(WsEvent::Error {
                    session: id,
                    message: e.to_string(),
                });
            }
        });
        match outcome_rx.await {
            Ok(outcome) => outcome.map(|()| session_id),
            // The task panicked; its guard has released the slot.
            Err(_) => Err(StartError::Internal(Error::Stt(
                "session start panicked".to_string(),
            ))),
        }
    }

    async fn start_session_inner(
        self: &Arc<Self>,
        session_id: &str,
        params: StartParams,
        control: SessionControl,
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
        // PII redaction: off unless the setting turns it on. Read once here
        // so a mid-session toggle doesn't split a transcript.
        let redact_pii = settings
            .get("redact_pii")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut provider_cfg = self.cfg.clone();
        if let Some(model) = setting_str("whisper_model") {
            provider_cfg.stt.whisper_local.model = model;
        }
        // Checked here, ahead of any model load: a setting skips the config
        // file's validation, and this value ends up in a provider's URL.
        let language = setting_str("stt_language").unwrap_or_else(|| self.cfg.stt.language.clone());
        auricle_stt::check_language(&provider_id, &language)
            .map_err(|e| StartError::Invalid(e.to_string()))?;

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
                let mut download_watch = None;
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
                            download_watch = Some(tokio::spawn(report_download_progress(
                                models_dir.join(format!("{}.part", m.file_name)),
                                session_id.to_string(),
                                model_name.clone(),
                                m.size_bytes,
                                self.important_tx.clone(),
                                DOWNLOAD_PROGRESS_EVERY,
                            )));
                        }
                    }
                }
                let created =
                    auricle_stt::create_provider(&provider_id, &provider_cfg, &models_dir).await;
                if let Some(watch) = download_watch {
                    watch.abort();
                }
                let p = created.map_err(|e| match e {
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
            language: language.clone(),
        };
        let mic_session = provider
            .start_session(&session_cfg)
            .await
            .map_err(StartError::Internal)?;
        let loop_session = match provider.start_session(&session_cfg).await {
            Ok(session) => session,
            Err(e) => {
                close_unused_session(mic_session).await;
                return Err(StartError::Internal(e));
            }
        };

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
        let opened = self.open_capture(
            session_id,
            &params,
            (mic_sel.as_str(), loop_sel.as_str()),
            retain_raw_audio,
            provider.id(),
            &control,
        );
        let OpenCapture {
            title,
            clock: session_clock,
            feed,
            mic_pipeline,
            loop_pipeline,
            mic_wav,
            loop_wav,
        } = match opened {
            Ok(opened) => opened,
            Err(e) => {
                close_unused_session(mic_session).await;
                close_unused_session(loop_session).await;
                return Err(e);
            }
        };

        let stop = control.stop.clone();
        let handles = control.handles.clone();
        let (mic_batch_tx, mic_batch_rx) = mpsc::unbounded_channel();
        let (loop_batch_tx, loop_batch_rx) = mpsc::unbounded_channel();
        let mut drain_joins: Vec<std::thread::JoinHandle<()>> = Vec::new();
        let mut capture_drops: Vec<(ChannelId, Arc<AtomicU64>, u32)> = Vec::new();
        match feed {
            Feed::Devices(mc, lc) => {
                for c in [&mc, &lc] {
                    capture_drops.push((c.channel, c.dropped.clone(), c.native_rate_hz));
                }
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
                    self.important_tx.clone(),
                ));
                drain_joins.push(spawn_drain(
                    lc,
                    stop.clone(),
                    session_clock,
                    loop_batch_tx,
                    loop_wav,
                    vu,
                    self.important_tx.clone(),
                ));
            }
            Feed::File(path, channel) => {
                let (tx, _other_tx) = match channel {
                    ChannelId::Mic => (mic_batch_tx, loop_batch_tx),
                    ChannelId::Loopback => (loop_batch_tx, mic_batch_tx),
                };
                // The unused channel's sender drops here: its driver sees a
                // closed stream and finishes immediately.
                tokio::spawn(feed_wav(path, tx, stop.clone(), session_clock));
            }
        }

        // Egress ledger: record where this session's audio goes. Local
        // providers (on-device Whisper) are logged as "local" so the ledger
        // shows the full picture, not just cloud traffic.
        let dest = if provider.kind() == SttKind::Local {
            ("local", None)
        } else {
            crate::egress::stt_egress(provider.id(), &provider_cfg)
        };
        crate::egress::record(
            &self.store,
            Some(session_id),
            "audio",
            dest,
            provider.id(),
            None,
            Some("live capture"),
        );
        let _ = self.important_tx.send(WsEvent::SessionStarted {
            session: session_id.to_string(),
            title,
            stt_provider: provider.id().to_string(),
        });
        if let Some(message) = auricle_stt::english_only_warning(
            &provider_id,
            &provider_cfg.stt.whisper_local.model,
            &language,
        ) {
            let _ = self.important_tx.send(WsEvent::Error {
                session: session_id.to_string(),
                message,
            });
        }

        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let mic_driver = tokio::spawn(drive_channel(
            mic_batch_rx,
            mic_pipeline,
            mic_session,
            ChannelId::Mic,
            ev_tx.clone(),
            session_clock,
        ));
        let loop_driver = tokio::spawn(drive_channel(
            loop_batch_rx,
            loop_pipeline,
            loop_session,
            ChannelId::Loopback,
            ev_tx,
            session_clock,
        ));
        let collector = tokio::spawn(collect_events(
            ev_rx,
            session_id.to_string(),
            provider.id().to_string(),
            self.store.clone(),
            self.important_tx.clone(),
            self.partial_tx.clone(),
            session_clock,
            redact_pii,
            self.cfg.stt.drop_mic_bleed,
        ));

        // Supervisor: waits for the drive tasks while watching the devices,
        // finalizes persistence and lifecycle, announces the stop.
        let engine = self.clone();
        let session = session_id.to_string();
        tokio::spawn(async move {
            let mut guard = SlotGuard {
                engine: engine.clone(),
                id: session.clone(),
                disarmed: false,
            };
            let drive_tasks = async {
                let mic_shed = mic_driver.await.unwrap_or_default();
                let loop_shed = loop_driver.await.unwrap_or_default();
                let _ = collector.await;
                for join in drain_joins {
                    let _ = tokio::task::spawn_blocking(move || join.join()).await;
                }
                HashMap::from([(ChannelId::Mic, mic_shed), (ChannelId::Loopback, loop_shed)])
            };
            let shed = watch_devices(
                drive_tasks,
                DEVICE_POLL_INTERVAL,
                || {
                    let handles = handles.lock().unwrap();
                    handles
                        .iter()
                        .filter_map(|h| h.stream_error().map(|e| (h.channel, e)))
                        .collect()
                },
                |channel, message| {
                    let _ = engine.important_tx.send(WsEvent::DeviceLost {
                        session: session.clone(),
                        channel: channel.to_string(),
                        message,
                    });
                },
            )
            .await;
            handles.lock().unwrap().clear(); // drop streams
            let capture_ms = capture_drops
                .iter()
                .map(|(channel, dropped, rate_hz)| {
                    let samples = dropped.load(Ordering::Relaxed);
                    (*channel, (samples * 1000).div_ceil(*rate_hz as u64))
                })
                .collect();
            engine.record_losses(&session, &capture_ms, &shed);
            if let Err(e) = engine.store.end_session(&session, unix_secs()) {
                let _ = engine.important_tx.send(WsEvent::Error {
                    session: session.clone(),
                    message: format!("persisting session end: {e}"),
                });
            }
            let _ = engine.important_tx.send(WsEvent::SessionStopped {
                session: session.clone(),
            });
            // Counted before the slot frees, so whoever sees this session
            // stopped also sees that its title is still to come.
            engine.titles_pending.fetch_add(1, Ordering::SeqCst);
            guard.disarmed = true;
            engine.lifecycle.lock().unwrap().finish_stop(&session);
            // Post-stop: name the session from its transcript (async; the
            // sidebar learns about it via a session_updated event).
            crate::title::autotitle_session(
                engine.store.clone(),
                engine.cfg.clone(),
                session,
                engine.important_tx.clone(),
            )
            .await;
            engine.titles_pending.fetch_sub(1, Ordering::SeqCst);
        });

        Ok(())
    }

    /// Everything about a start that can still fail once its STT sessions
    /// are open. It all happens before any thread or task exists, so a failure
    /// leaves the caller only those sessions to close: streams and WAV writers
    /// opened here drop with the error.
    fn open_capture(
        &self,
        session_id: &str,
        params: &StartParams,
        (mic_sel, loop_sel): (&str, &str),
        retain_raw_audio: bool,
        provider_id: &str,
        control: &SessionControl,
    ) -> std::result::Result<OpenCapture, StartError> {
        // Every await of the start is behind us, so this is the last moment a
        // stop can prevent capture outright. One landing later still works:
        // the drains see the flag as soon as they run.
        if !self.lifecycle.lock().unwrap().begin_capture(session_id) {
            return Err(StartError::Stopped);
        }

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
        let mut meta = serde_json::json!({});
        if auto_title {
            meta["auto_title"] = serde_json::json!(true);
        }

        let session_clock = Instant::now();
        let (feed, streams) = match params.audio.clone().unwrap_or(AudioSource::Devices) {
            AudioSource::Devices => {
                let (mh, mc) = start_mic(mic_sel).map_err(map_device_err)?;
                let (lh, lc) = start_loopback(loop_sel).map_err(map_device_err)?;
                meta["devices"] = serde_json::json!({
                    "mic": mh.device_name,
                    "loopback": lh.device_name,
                });
                (Feed::Devices(mc, lc), vec![mh, lh])
            }
            AudioSource::WavFile { path, channel } => (Feed::File(path, channel), Vec::new()),
        };

        // Raw-audio retention (privacy default: off). After the devices, so
        // the commonest failure leaves no empty files behind.
        let mut mic_wav = None;
        let mut loop_wav = None;
        if retain_raw_audio {
            let dir = self.data_root.join("sessions").join(session_id);
            std::fs::create_dir_all(&dir).map_err(|e| StartError::Internal(Error::Io(e)))?;
            let mic_path = dir.join("mic_16k.wav");
            let loop_path = dir.join("loopback_16k.wav");
            mic_wav =
                Some(RetainedAudio::open(&mic_path, &pipe_cfg).map_err(StartError::Internal)?);
            loop_wav =
                Some(RetainedAudio::open(&loop_path, &pipe_cfg).map_err(StartError::Internal)?);
            meta["audio"] = serde_json::json!({
                "mic": mic_path.to_string_lossy(),
                "loopback": loop_path.to_string_lossy(),
            });
        }

        self.store
            .create_session(session_id, &title, unix_secs(), provider_id, &meta)
            .map_err(StartError::Internal)?;
        // Only now: streams parked in the control would outlive a failed start.
        control.handles.lock().unwrap().extend(streams);
        Ok(OpenCapture {
            title,
            clock: session_clock,
            feed,
            mic_pipeline,
            loop_pipeline,
            mic_wav,
            loop_wav,
        })
    }

    /// Begin stopping: signal the drains, pause capture; the supervisor
    /// completes persistence and flips the lifecycle back to idle. Returns
    /// as soon as the state is `stopping`.
    ///
    /// A session still starting has nothing to pause yet; the same signal
    /// ends it before it opens any capture (see `open_capture`).
    pub fn stop_session(&self, id: &str) -> std::result::Result<(), StopError> {
        let control = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            lifecycle.begin_stop(id).map_err(|_| StopError::NotActive)?;
            lifecycle.control()
        };
        for h in control.handles.lock().unwrap().iter() {
            let _ = h.pause();
        }
        Ok(())
    }

    /// Put what the session lost on record, so a hole in the transcript can
    /// be told apart from silence after the live warnings are gone.
    fn record_losses(
        &self,
        session: &str,
        capture_ms: &HashMap<ChannelId, u64>,
        shed: &HashMap<ChannelId, SttDrops>,
    ) {
        let Some(serde_json::Value::Object(dropped)) = dropped_record(capture_ms, shed) else {
            return;
        };
        // Per channel: `bleed_finals` shares this object and isn't ours to replace.
        for (channel, record) in &dropped {
            let key = format!("dropped.{channel}");
            if let Err(e) = self.store.set_session_meta(session, &key, record) {
                let _ = self.important_tx.send(WsEvent::Error {
                    session: session.to_string(),
                    message: format!("persisting dropped audio: {e}"),
                });
                return;
            }
        }
    }
}

/// How often a first-run model download reports how far it has got.
const DOWNLOAD_PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// Report a model download's progress from the size of its `.part` file, until
/// the caller aborts this. The file is the only record of how far it has got,
/// and reading it keeps the download code free of any notion of a client.
async fn report_download_progress(
    part: PathBuf,
    session: String,
    model: String,
    size_bytes: u64,
    events: broadcast::Sender<WsEvent>,
    every: Duration,
) {
    let size_mb = size_bytes / 1_048_576;
    let mut last = None;
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let Ok(file) = tokio::fs::metadata(&part).await else {
            continue; // not started yet
        };
        let done_mb = (file.len() / 1_048_576).min(size_mb);
        if last != Some(done_mb) {
            last = Some(done_mb);
            let _ = events.send(WsEvent::ModelDownloadProgress {
                session: session.clone(),
                model: model.clone(),
                done_mb,
                size_mb,
            });
        }
    }
}

/// Per-channel loss summary for session meta, or None for a clean session.
/// `capture_ms` is audio the ring buffer overflowed; `stt_chunks` is chunks
/// the STT session shed, and `stt_finals` the finals among them: transcript
/// actually lost, where an interim is only a partial skipped.
fn dropped_record(
    capture_ms: &HashMap<ChannelId, u64>,
    shed: &HashMap<ChannelId, SttDrops>,
) -> Option<serde_json::Value> {
    let mut record = serde_json::Map::new();
    for channel in [ChannelId::Mic, ChannelId::Loopback] {
        let capture = capture_ms.get(&channel).copied().unwrap_or(0);
        let stt = shed.get(&channel).copied().unwrap_or_default();
        if capture > 0 || stt.chunks > 0 {
            record.insert(
                channel.to_string(),
                serde_json::json!({
                    "capture_ms": capture,
                    "stt_chunks": stt.chunks,
                    "stt_finals": stt.finals,
                }),
            );
        }
    }
    (!record.is_empty()).then_some(serde_json::Value::Object(record))
}

/// Bound on closing a session that never received audio: nothing is queued,
/// so only a wedged provider gets near it.
const CLOSE_UNUSED_TIMEOUT: Duration = Duration::from_secs(5);

/// Close a session a failed start opened but never fed; its worker or socket
/// would otherwise outlive the start.
async fn close_unused_session(mut session: Box<dyn SttSession>) {
    let _ = tokio::time::timeout(CLOSE_UNUSED_TIMEOUT, session.finish()).await;
}

/// How often a running session's capture streams are checked for failure.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Await `session`, reporting each channel's stream failure as it appears and
/// at most once. One last look at the end catches a device that died after
/// the final tick.
async fn watch_devices<T>(
    session: impl Future<Output = T>,
    interval: Duration,
    mut errors: impl FnMut() -> Vec<(ChannelId, String)>,
    mut report: impl FnMut(ChannelId, String),
) -> T {
    let mut reported = HashSet::new();
    let mut check = move || {
        for (channel, message) in errors() {
            if reported.insert(channel) {
                report(channel, message);
            }
        }
    };
    tokio::pin!(session);
    let mut tick = tokio::time::interval(interval);
    let result = loop {
        tokio::select! {
            result = &mut session => break result,
            _ = tick.tick() => check(),
        }
    };
    check();
    result
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

type WavFile = std::io::BufWriter<std::fs::File>;

/// Retained audio is 16-bit PCM: half the disk of f32 for inaudible loss at
/// 16 kHz, and the WAV codec browsers handle best (the playback bar streams
/// these files straight into an `<audio>` element).
const WAV_SPEC: hound::WavSpec = hound::WavSpec {
    channels: 1,
    sample_rate: TARGET_RATE_HZ,
    bits_per_sample: 16,
    sample_format: hound::SampleFormat::Int,
};

fn open_wav(path: &Path) -> Result<hound::WavWriter<WavFile>> {
    hound::WavWriter::create(path, WAV_SPEC)
        .map_err(|e| Error::Io(std::io::Error::other(format!("{}: {e}", path.display()))))
}

/// How often a retained WAV's header is rewritten. hound leaves the data
/// length at 0 until flush or finalize, so this bounds what a hard kill costs:
/// the file stays playable up to the last flush.
const WAV_FLUSH_INTERVAL_MS: u64 = 5_000;

const SAMPLES_PER_HOUR: u64 = TARGET_RATE_HZ as u64 * 3_600;
/// Silence written per turn of a drain loop: well under a millisecond's work,
/// so the ring buffer never waits on it, and at 200 turns a second, hours of
/// silence still land within a minute or so.
const PAD_SLICE_SAMPLES: u64 = TARGET_RATE_HZ as u64;
/// The most silence one track will hold, at 115 MB an hour. Past this a session
/// has been left running idle, and the disk matters more than the track.
const MAX_PAD_SAMPLES: u64 = 12 * SAMPLES_PER_HOUR;
/// The longest track. hound counts a file's data bytes in an unchecked `u32`,
/// which wraps a little past 37 hours of this format.
const MAX_TRACK_SAMPLES: u64 = 36 * SAMPLES_PER_HOUR;

/// `Err` carries the notice for the user, and means this track is over.
type WavResult = std::result::Result<(), String>;

fn wav_failed(e: hound::Error) -> String {
    format!("retained audio stopped, the transcript continues: {e}")
}

fn ms_to_samples(ms: u64) -> u64 {
    ms * TARGET_RATE_HZ as u64 / 1000
}

/// What the file is still owed, in order.
enum Pending {
    Silence(u64),
    Audio(Vec<f32>),
}

/// One channel's retained WAV, kept on the session timeline: the playback bar
/// seeks both files to transcript time, but capture delivers nothing during
/// system silence, so a plain sample count would drift off it.
///
/// Nothing here may hold up the drain thread, which has the ring buffer to
/// empty and the pipeline to feed. So writes queue, and `pump` works through
/// them a slice at a time: live audio that lands behind a long silence waits in
/// the queue, not in the ring.
struct RetainedAudio<W: Write + Seek = WavFile> {
    writer: hound::WavWriter<W>,
    /// Fed exactly what the channel pipeline's clock is fed, so the file jumps
    /// where transcript timestamps do, and jitter below the gap threshold
    /// never injects zeros.
    timeline: Timeline,
    /// Where the track has reached on the timeline, queued samples included.
    written: u64,
    /// Silence in the track so far, queued samples included.
    padded: u64,
    queue: VecDeque<Pending>,
    next_flush_ms: u64,
}

impl RetainedAudio {
    fn open(path: &Path, cfg: &PipelineConfig) -> Result<Self> {
        Ok(RetainedAudio::new(open_wav(path)?, cfg))
    }
}

impl<W: Write + Seek> RetainedAudio<W> {
    fn new(writer: hound::WavWriter<W>, cfg: &PipelineConfig) -> Self {
        RetainedAudio {
            writer,
            timeline: Timeline::new(cfg.sample_rate_hz, cfg.gap_threshold_ms),
            written: 0,
            padded: 0,
            queue: VecDeque::new(),
            next_flush_ms: 0,
        }
    }

    /// Append a batch drained at `wall_ms`, first padding silence if it opens
    /// the file or follows a stream gap.
    fn write(&mut self, samples: &[f32], wall_ms: u64) -> WavResult {
        let timing = self.timeline.on_batch(samples.len(), wall_ms);
        if self.written == 0 || timing.gap_before {
            self.pad_to(ms_to_samples(timing.t_start_ms))?;
        }
        let end = self.written + samples.len() as u64;
        check_track_length(end)?;
        self.queue.push_back(Pending::Audio(samples.to_vec()));
        self.written = end;
        self.pump()?;
        if wall_ms >= self.next_flush_ms {
            self.writer.flush().map_err(wav_failed)?;
            self.next_flush_ms = wall_ms + WAV_FLUSH_INTERVAL_MS;
        }
        Ok(())
    }

    /// Queue zeros up to sample `end`. A gap past the cap ends the track where
    /// the audio ended: resuming without it would put everything after it at
    /// the wrong transcript time, and that fails silently in the playback bar.
    fn pad_to(&mut self, end: u64) -> WavResult {
        let owed = end.saturating_sub(self.written);
        if owed == 0 {
            return Ok(());
        }
        check_track_length(end)?;
        if self.padded + owed > MAX_PAD_SAMPLES {
            return Err(format!(
                "retained audio stopped, the transcript continues: more than {} h of silence",
                MAX_PAD_SAMPLES / SAMPLES_PER_HOUR
            ));
        }
        self.queue.push_back(Pending::Silence(owed));
        self.padded += owed;
        self.written = end;
        Ok(())
    }

    /// Write what is queued, but no more than one slice of silence.
    fn pump(&mut self) -> WavResult {
        let mut budget = PAD_SLICE_SAMPLES;
        while let Some(next) = self.queue.front_mut() {
            match next {
                Pending::Audio(samples) => {
                    for s in samples.iter() {
                        self.writer
                            .write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)
                            .map_err(wav_failed)?;
                    }
                }
                Pending::Silence(owed) => {
                    let slice = budget.min(*owed);
                    for _ in 0..slice {
                        self.writer.write_sample(0i16).map_err(wav_failed)?;
                    }
                    budget -= slice;
                    *owed -= slice;
                    if *owed > 0 {
                        return Ok(());
                    }
                }
            }
            self.queue.pop_front();
        }
        Ok(())
    }

    /// Pad to the end of the session and close the file, so tracks end
    /// together. Past the caps the track just ends early: all that is missing
    /// is silence.
    fn finish(mut self, wall_ms: u64) -> WavResult {
        let room = self.written + (MAX_PAD_SAMPLES - self.padded);
        self.pad_to(ms_to_samples(wall_ms).min(room).min(MAX_TRACK_SAMPLES))?;
        self.close()
    }

    /// Write out whatever is queued and close the file.
    fn close(mut self) -> WavResult {
        while !self.queue.is_empty() {
            self.pump()?;
        }
        self.writer.finalize().map_err(wav_failed)
    }
}

fn check_track_length(end: u64) -> WavResult {
    if end > MAX_TRACK_SAMPLES {
        return Err(format!(
            "retained audio stopped at the {} h limit of one file, the transcript continues",
            MAX_TRACK_SAMPLES / SAMPLES_PER_HOUR
        ));
    }
    Ok(())
}

/// Run `step` on a channel's WAV, if it still has one. The first failure is the
/// last: the file closes with what it has, the notice goes out once, and the
/// session carries on without it.
fn retain<W: Write + Seek>(
    wav: &mut Option<RetainedAudio<W>>,
    report: &impl Fn(String),
    step: impl FnOnce(&mut RetainedAudio<W>) -> WavResult,
) {
    let Some(track) = wav.as_mut() else {
        return;
    };
    if let Err(message) = step(track) {
        report(message);
        if let Some(track) = wav.take() {
            let _ = track.close();
        }
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
fn spawn_drain<W: Write + Seek + Send + 'static>(
    mut c: CaptureConsumer,
    stop: Arc<AtomicBool>,
    clock: Instant,
    tx: mpsc::UnboundedSender<(Vec<f32>, u64)>,
    mut wav: Option<RetainedAudio<W>>,
    vu: VuSink,
    errors: broadcast::Sender<WsEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(mut resampler) = MonoResampler::new(c.native_rate_hz, TARGET_RATE_HZ) else {
            return;
        };
        let channel_name = c.channel.to_string();
        let report = |message: String| {
            let _ = errors.send(WsEvent::Error {
                session: vu.session.clone(),
                message: format!("{channel_name}: {message}"),
            });
        };
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
                    let wall_ms = clock.elapsed().as_millis() as u64;
                    retain(&mut wav, &report, |w| w.write(&out, wall_ms));
                    if tx.send((out.clone(), wall_ms)).is_err() {
                        break;
                    }
                }
            } else if stop.load(Ordering::Relaxed) {
                out.clear();
                let _ = resampler.finish(&mut out);
                if !out.is_empty() {
                    let wall_ms = clock.elapsed().as_millis() as u64;
                    retain(&mut wav, &report, |w| w.write(&out, wall_ms));
                    let _ = tx.send((out.clone(), wall_ms));
                }
                break;
            } else {
                // Nothing to drain: the time to catch up on owed silence.
                retain(&mut wav, &report, RetainedAudio::pump);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if let Some(w) = wav {
            if let Err(message) = w.finish(clock.elapsed().as_millis() as u64) {
                report(message);
            }
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
    clock: Instant,
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
/// How long `finish()` itself is awaited; see `settle_session`.
const FINISH_GRACE: Duration = Duration::from_secs(1);

/// How often a channel looks for a stream that stopped mid-sentence. The gap
/// threshold (250 ms) sets when one counts as stalled; this only adds latency.
const STALL_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Feed `chunks` to the session; false once it refuses them.
async fn feed_chunks(
    session: &mut dyn SttSession,
    chunks: Vec<AudioChunk>,
    channel: ChannelId,
    ev_tx: &mpsc::UnboundedSender<(ChannelId, SttEvent)>,
) -> bool {
    for chunk in chunks {
        if session.feed(chunk).await.is_err() {
            let _ = ev_tx.send((
                channel,
                SttEvent::Error("stt session rejected audio".to_string()),
            ));
            return false;
        }
    }
    true
}

/// Per-channel driver: batches → VAD/chunker → STT session. Everything is
/// fed: interim backpressure belongs to the sessions that queue work, since
/// only they know when a chunk is done. Returns what the session shed.
/// (No runtime swap here: the provider is a per-session setting in the API;
/// the hotkey swap remains a CLI feature.)
async fn drive_channel<D: SpeechDetector>(
    mut batch_rx: mpsc::UnboundedReceiver<(Vec<f32>, u64)>,
    mut pipeline: ChannelPipeline<D>,
    mut session: Box<dyn SttSession>,
    channel: ChannelId,
    ev_tx: mpsc::UnboundedSender<(ChannelId, SttEvent)>,
    clock: Instant,
) -> SttDrops {
    let mut stall_check = tokio::time::interval(STALL_CHECK_INTERVAL);
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
                        let _ = ev_tx.send((channel, ev));
                        continue 'feed;
                    }
                    // Worker gone mid-session: stop feeding, settle below.
                    None => break 'feed,
                }
            }
            _ = stall_check.tick() => {
                // Queued batches mean this task ran late, not that the
                // stream stalled, and they're about to say so themselves.
                if batch_rx.is_empty() {
                    let wall_ms = clock.elapsed().as_millis() as u64;
                    let chunks = pipeline.tick(wall_ms);
                    if !feed_chunks(session.as_mut(), chunks, channel, &ev_tx).await {
                        break 'feed;
                    }
                }
                continue 'feed;
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
            let chunks = pipeline.push_batch(&samples, wall_ms);
            if !feed_chunks(session.as_mut(), chunks, channel, &ev_tx).await {
                break 'feed;
            }
        }
    }
    // Close any open span; feeding a dead session is a harmless no-op.
    for chunk in pipeline.flush() {
        if chunk.kind == ChunkKind::Final {
            let _ = session.feed(chunk).await;
        }
    }
    settle_session(
        session.as_mut(),
        channel,
        &ev_tx,
        FINISH_GRACE,
        FINISH_TIMEOUT,
    )
    .await;
    session.drops()
}

/// Close a session and forward everything it still produces.
///
/// `finish()` and `next_event()` both borrow the session mutably, so they can
/// never be polled together, and anything awaiting `finish()` alone leaves
/// finished text unread. But every `finish()` tells its worker to wind down on
/// first poll and then only waits for it, so it gets a short `grace`: past
/// that the future is dropped (the worker lives on, detached) and its output
/// is read here until the channel closes or `finish_timeout` runs out.
async fn settle_session(
    session: &mut dyn SttSession,
    channel: ChannelId,
    ev_tx: &mpsc::UnboundedSender<(ChannelId, SttEvent)>,
    grace: Duration,
    finish_timeout: Duration,
) {
    let give_up = tokio::time::Instant::now() + finish_timeout;
    let finished = match tokio::time::timeout(grace, session.finish()).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            let _ = ev_tx.send((channel, SttEvent::Error(e.to_string())));
            true
        }
        Err(_) => false,
    };
    let deadline = if finished {
        tokio::time::Instant::now() + DRAIN_TIMEOUT
    } else {
        give_up
    };
    loop {
        match tokio::time::timeout_at(deadline, session.next_event()).await {
            Ok(Some(ev)) => {
                let _ = ev_tx.send((channel, ev));
            }
            Ok(None) => return,
            Err(_) => break,
        }
    }
    if !finished {
        // Abandon the wedged session so the stop can complete; its worker
        // task dies with the process.
        let _ = ev_tx.send((
            channel,
            SttEvent::Error(format!(
                "stt session did not finish within {}s; abandoning it",
                finish_timeout.as_secs()
            )),
        ));
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
    redact_pii: bool,
    drop_mic_bleed: bool,
) {
    let mut assembler = Assembler::new(&session_id);
    assembler.set_drop_mic_bleed(drop_mic_bleed);
    while let Some((channel, ev)) = ev_rx.recv().await {
        if let SttEvent::Error(message) = &ev {
            let _ = important_tx.send(WsEvent::Error {
                session: session_id.clone(),
                message: format!("{channel}: {message}"),
            });
            continue;
        }
        let was_final = matches!(ev, SttEvent::Final(_));
        let Some(PipelineEvent::Transcript(mut t)) = assembler.ingest(ev) else {
            // No final will replace the partial showing this audio. Shed
            // under lag, that row would stay on screen, so it can't be lossy.
            if was_final {
                let _ = important_tx.send(WsEvent::PartialCleared {
                    session: session_id.clone(),
                    channel: channel.to_string(),
                });
            }
            continue;
        };
        // Single choke point: scrub here and every consumer — the database,
        // the live view, the transcript ring feeding asks, and summaries —
        // sees only redacted text.
        if redact_pii {
            t.text = crate::redact::redact(&t.text);
        }
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
    // A hole left by the filter must be tellable from speech never heard.
    let bleed_finals = assembler.bleed_finals();
    if bleed_finals > 0 {
        let saved = store.set_session_meta(
            &session_id,
            "dropped.bleed_finals",
            &serde_json::Value::from(bleed_finals),
        );
        if let Err(e) = saved {
            let _ = important_tx.send(WsEvent::Error {
                session: session_id.clone(),
                message: format!("persisting dropped lines: {e}"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use auricle_core::Segment;

    // ---------- retained audio ----------

    /// 10 ms at 16 kHz.
    const BATCH: usize = 160;
    const SAMPLES_PER_MS: usize = 16;
    const WAV_HEADER_BYTES: u64 = 44;

    fn retained(name: &str) -> (RetainedAudio, PathBuf) {
        let path =
            std::env::temp_dir().join(format!("auricle-engine-{name}-{}.wav", std::process::id()));
        let wav = RetainedAudio::open(&path, &PipelineConfig::default()).unwrap();
        (wav, path)
    }

    fn tone() -> Vec<f32> {
        vec![0.5; BATCH]
    }

    fn samples_in(path: &std::path::Path) -> Vec<i16> {
        hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(|s| s.unwrap())
            .collect()
    }

    #[test]
    fn audio_after_a_silent_stretch_lands_at_its_session_time() {
        let (mut wav, path) = retained("gap");
        wav.write(&tone(), 10).unwrap();
        wav.write(&tone(), 20).unwrap();
        // Loopback delivers nothing for 30 s.
        wav.write(&tone(), 30_020).unwrap();
        wav.finish(30_020).unwrap();

        let samples = samples_in(&path);
        assert_eq!(samples.len(), 30_020 * SAMPLES_PER_MS);
        let resumed = 30_010 * SAMPLES_PER_MS;
        assert!(samples[2 * BATCH..resumed].iter().all(|s| *s == 0));
        assert!(samples[resumed..].iter().all(|s| *s != 0));
    }

    #[test]
    fn normal_jitter_injects_no_zeros() {
        let (mut wav, path) = retained("jitter");
        let batches = 1_000u64;
        for i in 0..batches {
            // Up to 120 ms late, well inside the gap threshold.
            wav.write(&tone(), (i + 1) * 10 + (i % 7) * 20).unwrap();
        }
        wav.finish(batches * 10).unwrap();

        let samples = samples_in(&path);
        assert_eq!(samples.len(), batches as usize * BATCH);
        assert!(samples.iter().all(|s| *s != 0), "a click was injected");
    }

    #[test]
    fn a_late_first_batch_starts_at_its_session_time() {
        // Exact even below the gap threshold: nothing precedes it to click.
        for (name, first_ms) in [("start-short", 100), ("start-long", 1_000)] {
            let (mut wav, path) = retained(name);
            wav.write(&tone(), first_ms).unwrap();
            wav.finish(first_ms).unwrap();

            let samples = samples_in(&path);
            let first_sample = (first_ms as usize - 10) * SAMPLES_PER_MS;
            assert_eq!(samples.len(), first_ms as usize * SAMPLES_PER_MS);
            assert!(samples[..first_sample].iter().all(|s| *s == 0));
            assert!(samples[first_sample..].iter().all(|s| *s != 0));
        }
    }

    #[test]
    fn tracks_end_together_however_little_one_delivered() {
        let (mut mic, mic_path) = retained("tail-mic");
        let (silent, silent_path) = retained("tail-silent");
        for i in 0..1_000u64 {
            mic.write(&tone(), (i + 1) * 10).unwrap();
        }
        mic.finish(10_000).unwrap();
        silent.finish(10_000).unwrap();

        let silent_samples = samples_in(&silent_path);
        assert_eq!(samples_in(&mic_path).len(), silent_samples.len());
        assert_eq!(silent_samples.len(), 10_000 * SAMPLES_PER_MS);
    }

    #[test]
    fn the_file_follows_the_pipeline_clock() {
        let cfg = PipelineConfig::default();
        let mut pipeline_clock = Timeline::new(cfg.sample_rate_hz, cfg.gap_threshold_ms);
        let (mut wav, _path) = retained("clock");
        for wall_ms in [500, 510, 525, 530, 900, 5_000, 5_010, 5_400, 5_410] {
            let t_start_ms = pipeline_clock.on_batch(BATCH, wall_ms).t_start_ms;
            wav.write(&tone(), wall_ms).unwrap();
            let batch_at_ms = (wav.written - BATCH as u64) / SAMPLES_PER_MS as u64;
            assert!(batch_at_ms.abs_diff(t_start_ms) <= 1, "at {wall_ms}");
        }
    }

    #[test]
    fn a_long_silence_is_not_written_in_one_burst() {
        let (mut wav, path) = retained("burst");
        wav.write(&tone(), 10).unwrap();
        // Ten minutes of nothing: 19 MB of zeros owed.
        wav.write(&tone(), 600_010).unwrap();

        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert!(on_disk < 1_000_000, "{on_disk} bytes in one call");
        drop(wav);
    }

    #[test]
    fn each_pump_writes_one_slice_of_silence() {
        let (mut wav, _) = track_on_disk(u64::MAX);
        wav.write(&tone(), 10).unwrap();
        wav.write(&tone(), 600_010).unwrap();
        let before = wav.writer.len() as u64;

        wav.pump().unwrap();

        assert_eq!(wav.writer.len() as u64, before + PAD_SLICE_SAMPLES);
    }

    #[test]
    fn audio_landing_during_padding_keeps_its_place() {
        let (mut wav, path) = retained("mid-pad");
        wav.write(&tone(), 10).unwrap();
        wav.write(&tone(), 600_010).unwrap();
        for i in 1..=100u64 {
            wav.write(&tone(), 600_010 + i * 10).unwrap();
        }
        assert!(!wav.queue.is_empty(), "padding finished too soon to tell");

        wav.finish(601_010).unwrap();

        let samples = samples_in(&path);
        let resumed = 600_000 * SAMPLES_PER_MS;
        assert_eq!(samples.len(), 601_010 * SAMPLES_PER_MS);
        assert!(samples[..BATCH].iter().all(|s| *s != 0));
        assert!(samples[BATCH..resumed].iter().all(|s| *s == 0));
        assert!(samples[resumed..].iter().all(|s| *s != 0));
    }

    /// A disk with `room` bytes free, counting the writes it turns down.
    struct LimitedDisk {
        file: std::io::Cursor<Vec<u8>>,
        room: u64,
        refused: Arc<AtomicUsize>,
    }

    impl Write for LimitedDisk {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.file.position() + buf.len() as u64 > self.room {
                self.refused.fetch_add(1, Ordering::SeqCst);
                return Err(std::io::Error::other("disk full"));
            }
            self.file.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for LimitedDisk {
        fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
            self.file.seek(to)
        }
    }

    fn track_on_disk(room: u64) -> (RetainedAudio<LimitedDisk>, Arc<AtomicUsize>) {
        let refused = Arc::new(AtomicUsize::new(0));
        let disk = LimitedDisk {
            file: std::io::Cursor::new(Vec::new()),
            room,
            refused: refused.clone(),
        };
        let writer = hound::WavWriter::new(disk, WAV_SPEC).unwrap();
        let track = RetainedAudio::new(writer, &PipelineConfig::default());
        (track, refused)
    }

    #[test]
    fn the_first_failed_write_ends_the_track() {
        let (mut wav, refused) = track_on_disk(WAV_HEADER_BYTES + 2 * BATCH as u64);
        wav.write(&tone(), 10).unwrap();

        let message = wav.write(&tone(), 60_010).unwrap_err();

        assert!(message.contains("disk full"), "{message}");
        assert_eq!(refused.load(Ordering::SeqCst), 1, "it kept trying");
    }

    #[test]
    fn silence_past_the_cap_ends_the_track_instead_of_padding_it() {
        let (mut wav, _) = track_on_disk(u64::MAX);
        wav.write(&tone(), 10).unwrap();
        wav.padded = MAX_PAD_SAMPLES - 1_000;
        let end_of_audio = wav.written;

        let message = wav.write(&tone(), 60_010).unwrap_err();

        assert!(message.contains("silence"), "{message}");
        assert_eq!(wav.written, end_of_audio);
        assert!(wav.queue.is_empty(), "padded up to the cap regardless");
    }

    #[test]
    fn a_tail_past_the_cap_just_ends_the_track_early() {
        let (mut wav, path) = retained("tail-cap");
        wav.write(&tone(), 10).unwrap();
        wav.padded = MAX_PAD_SAMPLES - 1_000;

        wav.finish(60_000).unwrap();

        assert_eq!(samples_in(&path).len(), BATCH + 1_000);
    }

    #[test]
    fn a_track_stops_short_of_the_wav_size_limit() {
        // hound adds the header to the data size, in a u32.
        assert!(MAX_TRACK_SAMPLES * 2 + WAV_HEADER_BYTES < u32::MAX as u64);

        let (mut wav, _) = track_on_disk(u64::MAX);
        wav.write(&tone(), 10).unwrap();
        wav.written = MAX_TRACK_SAMPLES - 10;
        let message = wav.write(&tone(), 20).unwrap_err();
        assert!(message.contains("limit"), "{message}");

        // Reached by silence, which never gets as far as the padding cap.
        let (mut wav, _) = track_on_disk(u64::MAX);
        wav.write(&tone(), 10).unwrap();
        let message = wav.write(&tone(), 37 * 3_600_000).unwrap_err();
        assert!(message.contains("limit"), "{message}");
    }

    fn ring_capture() -> (rtrb::Producer<f32>, CaptureConsumer) {
        let (producer, consumer) = rtrb::RingBuffer::new(1 << 16);
        let capture = CaptureConsumer {
            channel: ChannelId::Loopback,
            consumer,
            dropped: Arc::default(),
            native_rate_hz: TARGET_RATE_HZ,
        };
        (producer, capture)
    }

    fn vu_sink() -> VuSink {
        VuSink {
            session: "s1".to_string(),
            tx: broadcast::channel(PARTIAL_CAPACITY).0,
        }
    }

    fn push_batch(ring: &mut rtrb::Producer<f32>) {
        for _ in 0..BATCH {
            ring.push(0.5).unwrap();
        }
    }

    #[test]
    fn the_pipeline_gets_live_audio_while_silence_is_still_owed() {
        let (mut ring, capture) = ring_capture();
        let (wav, path) = retained("drain-gap");
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Ten minutes in, so the first batch owes 19 MB of silence.
        let clock = Instant::now() - Duration::from_secs(600);
        let errors = broadcast::channel(IMPORTANT_CAPACITY).0;
        let drain = spawn_drain(
            capture,
            stop.clone(),
            clock,
            tx,
            Some(wav),
            vu_sink(),
            errors,
        );

        push_batch(&mut ring);
        rx.blocking_recv()
            .expect("the batch never reached the pipeline");
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert!(on_disk < 9_000_000, "{on_disk} bytes went down first");

        stop.store(true, Ordering::Relaxed);
        drain.join().unwrap();
        let samples = samples_in(&path);
        let audio_at = samples.iter().position(|s| *s != 0).unwrap();
        assert!(audio_at > 590_000 * SAMPLES_PER_MS, "at sample {audio_at}");
    }

    #[test]
    fn a_failing_disk_is_reported_once_and_audio_keeps_flowing() {
        let (mut ring, capture) = ring_capture();
        let (wav, _) = track_on_disk(WAV_HEADER_BYTES);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (errors, mut reported) = broadcast::channel(IMPORTANT_CAPACITY);
        let drain = spawn_drain(
            capture,
            stop.clone(),
            Instant::now(),
            tx,
            Some(wav),
            vu_sink(),
            errors,
        );

        for _ in 0..3 {
            push_batch(&mut ring);
            rx.blocking_recv()
                .expect("a dead disk stopped the pipeline");
        }
        stop.store(true, Ordering::Relaxed);
        drain.join().unwrap();

        let Ok(WsEvent::Error { session, message }) = reported.try_recv() else {
            panic!("the failure was never reported");
        };
        assert_eq!(session, "s1");
        assert!(message.starts_with("loopback: "), "{message}");
        assert!(message.contains("disk full"), "{message}");
        assert!(reported.try_recv().is_err(), "reported more than once");
    }

    #[test]
    fn a_hard_kill_leaves_a_readable_file() {
        let (mut wav, path) = retained("kill");
        for i in 0..700u64 {
            wav.write(&tone(), (i + 1) * 10).unwrap();
        }
        // Still open and never finalized: this is what a crash leaves on disk.
        let on_disk = samples_in(&path).len();
        assert!(on_disk >= 500 * BATCH, "only {on_disk} samples readable");
        drop(wav);
    }

    // ---------- device loss ----------

    #[tokio::test]
    async fn a_lost_device_is_reported_mid_session_and_only_once() {
        let reports = Mutex::new(Vec::new());
        let session = async {
            while reports.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            // Several more polls, and the end-of-session look, still to come.
            tokio::time::sleep(Duration::from_millis(30)).await;
            "done"
        };
        let watched = watch_devices(
            session,
            Duration::from_millis(5),
            || vec![(ChannelId::Mic, "device invalidated".to_string())],
            |channel, _| reports.lock().unwrap().push(channel),
        );

        let result = tokio::time::timeout(Duration::from_secs(5), watched).await;
        assert_eq!(result, Ok("done"), "loss was not reported mid-session");
        assert_eq!(*reports.lock().unwrap(), vec![ChannelId::Mic]);
    }

    #[tokio::test]
    async fn each_channel_is_reported_separately() {
        let reports = Mutex::new(Vec::new());
        let mut polls = 0;
        watch_devices(
            tokio::time::sleep(Duration::from_millis(40)),
            Duration::from_millis(5),
            || {
                polls += 1;
                let mut errors = vec![(ChannelId::Mic, "gone".to_string())];
                if polls > 2 {
                    errors.push((ChannelId::Loopback, "gone".to_string()));
                }
                errors
            },
            |channel, _| reports.lock().unwrap().push(channel),
        )
        .await;
        assert_eq!(
            *reports.lock().unwrap(),
            vec![ChannelId::Mic, ChannelId::Loopback]
        );
    }

    #[tokio::test]
    async fn a_device_lost_after_the_last_poll_is_still_reported() {
        let reports = Mutex::new(Vec::new());
        let mut polls = 0;
        watch_devices(
            tokio::time::sleep(Duration::from_millis(20)),
            Duration::from_secs(3600),
            || {
                polls += 1;
                // The immediate first tick sees a healthy device.
                if polls > 1 {
                    vec![(ChannelId::Loopback, "gone".to_string())]
                } else {
                    Vec::new()
                }
            },
            |channel, _| reports.lock().unwrap().push(channel),
        )
        .await;
        assert_eq!(*reports.lock().unwrap(), vec![ChannelId::Loopback]);
    }

    // ---------- settling a stopping session ----------

    /// A session whose `finish()` never returns, like a wedged provider.
    struct HungSession {
        events: mpsc::UnboundedReceiver<SttEvent>,
    }

    #[async_trait]
    impl SttSession for HungSession {
        async fn feed(&mut self, _chunk: AudioChunk) -> Result<()> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SttEvent> {
            self.events.recv().await
        }
        async fn finish(&mut self) -> Result<Vec<Segment>> {
            std::future::pending().await
        }
    }

    fn final_text(text: &str) -> SttEvent {
        SttEvent::Final(Segment {
            channel: ChannelId::Mic,
            t_start_ms: 0,
            t_end_ms: 1_000,
            text: text.to_string(),
        })
    }

    /// Run a settle to completion: the finals forwarded, and any error texts.
    async fn settle(session: &mut HungSession, timeout: Duration) -> (Vec<String>, Vec<String>) {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let grace = Duration::from_millis(10);
        settle_session(session, ChannelId::Mic, &ev_tx, grace, timeout).await;
        drop(ev_tx);
        let (mut finals, mut errors) = (Vec::new(), Vec::new());
        while let Some((_, ev)) = ev_rx.recv().await {
            match ev {
                SttEvent::Final(segment) => finals.push(segment.text),
                SttEvent::Error(message) => errors.push(message),
                other => panic!("unexpected event {other:?}"),
            }
        }
        (finals, errors)
    }

    #[tokio::test]
    async fn finals_survive_a_finish_that_never_returns() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(final_text("queued before stop")).unwrap();
        let late = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            late.send(final_text("finished during the wait")).unwrap();
        });

        let mut session = HungSession { events: rx };
        let (finals, errors) = settle(&mut session, Duration::from_millis(200)).await;

        assert_eq!(finals, ["queued before stop", "finished during the wait"]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("did not finish"), "{errors:?}");
        drop(tx);
    }

    #[tokio::test]
    async fn a_worker_outlasting_the_grace_gets_the_whole_window() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            // Well past the 10 ms grace, then exits as a real worker does.
            tokio::time::sleep(Duration::from_millis(60)).await;
            tx.send(final_text("slow but fine")).unwrap();
        });

        let mut session = HungSession { events: rx };
        let (finals, errors) = settle(&mut session, Duration::from_secs(5)).await;

        assert_eq!(finals, ["slow but fine"]);
        assert!(errors.is_empty(), "{errors:?}");
    }

    // ---------- driving a channel ----------

    /// Speech iff the frame's peak exceeds 0.5.
    struct EnergyDetector;

    impl SpeechDetector for EnergyDetector {
        fn frame_len(&self) -> usize {
            512
        }
        fn predict(&mut self, frame: &[f32]) -> f32 {
            if frame.iter().any(|s| s.abs() > 0.5) {
                0.9
            } else {
                0.1
            }
        }
        fn reset(&mut self) {}
    }

    /// Records what it is fed and answers nothing, the way a streaming
    /// provider owes no event per chunk.
    struct SilentSession {
        fed: Arc<Mutex<Vec<AudioChunk>>>,
        _open: mpsc::UnboundedSender<SttEvent>,
        events: mpsc::UnboundedReceiver<SttEvent>,
    }

    #[async_trait]
    impl SttSession for SilentSession {
        async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
            self.fed.lock().unwrap().push(chunk);
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SttEvent> {
            self.events.recv().await
        }
        async fn finish(&mut self) -> Result<Vec<Segment>> {
            self.events.close();
            Ok(Vec::new())
        }
    }

    fn silent_session() -> (Box<dyn SttSession>, Arc<Mutex<Vec<AudioChunk>>>) {
        let fed = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let session = SilentSession {
            fed: fed.clone(),
            _open: tx,
            events: rx,
        };
        (Box::new(session), fed)
    }

    fn channel_pipeline() -> ChannelPipeline<EnergyDetector> {
        ChannelPipeline::new(
            "t",
            ChannelId::Mic,
            EnergyDetector,
            &PipelineConfig::default(),
        )
    }

    /// `ms` of speech in 10 ms batches, on a wall clock starting at `from_ms`.
    fn send_speech(tx: &mpsc::UnboundedSender<(Vec<f32>, u64)>, from_ms: u64, ms: u64) {
        for i in 1..=ms / 10 {
            tx.send((vec![0.8; BATCH], from_ms + i * 10)).unwrap();
        }
    }

    #[tokio::test]
    async fn interims_keep_reaching_a_session_that_answers_no_finals() {
        let (session, fed) = silent_session();
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        send_speech(&batch_tx, 0, 15_000);
        drop(batch_tx);

        let clock = Instant::now();
        drive_channel(
            batch_rx,
            channel_pipeline(),
            session,
            ChannelId::Mic,
            ev_tx,
            clock,
        )
        .await;

        let kinds: Vec<_> = fed.lock().unwrap().iter().map(|c| c.kind).collect();
        let second_final = kinds
            .iter()
            .enumerate()
            .filter(|(_, kind)| **kind == ChunkKind::Final)
            .nth(1)
            .expect("15 s of speech holds several finals")
            .0;
        assert!(
            kinds[second_final..].contains(&ChunkKind::Interim),
            "{kinds:?}"
        );
    }

    /// Waits for the session to have been fed a final.
    async fn final_fed(fed: &Mutex<Vec<AudioChunk>>) -> AudioChunk {
        loop {
            let last_final = fed
                .lock()
                .unwrap()
                .iter()
                .rfind(|c| c.kind == ChunkKind::Final)
                .cloned();
            if let Some(chunk) = last_final {
                return chunk;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_stream_that_stalls_mid_sentence_is_finalized_with_no_more_audio() {
        let (session, fed) = silent_session();
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        // The session began 2 s ago: wall time lines up with the audio below.
        let clock = Instant::now() - Duration::from_secs(2);
        send_speech(&batch_tx, 0, 2_000);

        let driver = tokio::spawn(drive_channel(
            batch_rx,
            channel_pipeline(),
            session,
            ChannelId::Mic,
            ev_tx,
            clock,
        ));

        // The sender stays open: this is a stall, not a stop.
        let chunk = tokio::time::timeout(Duration::from_secs(5), final_fed(&fed))
            .await
            .expect("the open span must close on its own");
        assert!(chunk.t_end_ms >= 1_900, "{}", chunk.t_end_ms);
        assert!(!driver.is_finished());
        drop(batch_tx);
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn a_flowing_stream_is_not_mistaken_for_a_stall() {
        let (session, fed) = silent_session();
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let clock = Instant::now();
        let driver = tokio::spawn(drive_channel(
            batch_rx,
            channel_pipeline(),
            session,
            ChannelId::Mic,
            ev_tx,
            clock,
        ));

        // Batches stamped with real time, across several stall checks.
        while clock.elapsed() < STALL_CHECK_INTERVAL * 5 {
            let wall_ms = clock.elapsed().as_millis() as u64;
            batch_tx.send((vec![0.8; BATCH], wall_ms)).unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let finals = fed
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.kind == ChunkKind::Final)
            .count();
        assert_eq!(finals, 0, "the open span was closed under running speech");
        drop(batch_tx);
        driver.await.unwrap();
    }

    // ---------- releasing the slot ----------

    struct QuietProvider;

    #[async_trait]
    impl SttProvider for QuietProvider {
        fn id(&self) -> &'static str {
            "quiet"
        }
        fn kind(&self) -> SttKind {
            SttKind::Local
        }
        async fn start_session(&self, _cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
            Ok(silent_session().0)
        }
    }

    fn quiet_engine(name: &str) -> Arc<Engine> {
        let data_root =
            std::env::temp_dir().join(format!("auricle-engine-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_root);
        Engine::new(EngineOptions {
            cfg: Config::default(),
            data_root,
            provider_override: Some(Arc::new(QuietProvider)),
        })
        .unwrap()
    }

    fn fixture_params() -> StartParams {
        StartParams {
            audio: Some(AudioSource::WavFile {
                path: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/tts_loopback_16k.wav"),
                channel: ChannelId::Loopback,
            }),
            ..StartParams::default()
        }
    }

    #[tokio::test]
    async fn a_panicking_supervisor_still_frees_the_slot() {
        let engine = quiet_engine("supervisor-panic");
        let mut events = engine.subscribe_important();
        let id = engine.start_session(fixture_params()).await.unwrap();

        // The device poll locks this, so poisoning it panics the supervisor.
        let handles = engine.lifecycle.lock().unwrap().control().handles;
        let _ = std::thread::spawn(move || {
            let _held = handles.lock().unwrap();
            panic!("poisoning the capture handles");
        })
        .join();

        let deadline = Instant::now() + Duration::from_secs(5);
        while engine.active_session().is_some() {
            assert!(Instant::now() < deadline, "the slot stayed taken");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        loop {
            match events.try_recv() {
                Ok(WsEvent::SessionStopped { session }) => {
                    assert_eq!(session, id);
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("clients never heard it stopped: {e}"),
            }
        }
    }

    #[tokio::test]
    async fn waiting_for_a_stop_is_about_that_session_alone() {
        let engine = quiet_engine("wait-by-id");
        engine.lifecycle.lock().unwrap().start("next").unwrap();

        let waited = crate::api::stop_and_wait(&engine, "stopped-already");

        tokio::time::timeout(Duration::from_secs(2), waited)
            .await
            .expect("it waited on a session it was not stopping");
    }

    // ---------- collecting events ----------

    fn segment(channel: ChannelId, text: &str) -> Segment {
        Segment {
            channel,
            t_start_ms: 0,
            t_end_ms: 3_000,
            text: text.to_string(),
        }
    }

    /// Everything `collect_events` left behind for one run.
    struct Collected {
        important: Vec<WsEvent>,
        partial: Vec<WsEvent>,
        store: Arc<Store>,
    }

    impl Collected {
        fn cleared(&self) -> Vec<(&str, &str)> {
            self.important
                .iter()
                .filter_map(|event| match event {
                    WsEvent::PartialCleared { session, channel } => {
                        Some((session.as_str(), channel.as_str()))
                    }
                    _ => None,
                })
                .collect()
        }

        fn meta(&self) -> serde_json::Value {
            self.store.get_session("s1").unwrap().unwrap().meta
        }
    }

    async fn collect(
        events: Vec<(ChannelId, SttEvent)>,
        name: &str,
        drop_mic_bleed: bool,
    ) -> Collected {
        let dir = std::env::temp_dir().join(format!(
            "auricle-engine-collect-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(&dir.join("auricle.db")).unwrap());
        store
            .create_session("s1", "t", 0, "fake", &serde_json::json!({}))
            .unwrap();
        let (important_tx, mut important_rx) = broadcast::channel(IMPORTANT_CAPACITY);
        let (partial_tx, mut partial_rx) = broadcast::channel(PARTIAL_CAPACITY);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        for event in events {
            ev_tx.send(event).unwrap();
        }
        drop(ev_tx);

        collect_events(
            ev_rx,
            "s1".to_string(),
            "fake".to_string(),
            store.clone(),
            important_tx,
            partial_tx,
            Instant::now(),
            false,
            drop_mic_bleed,
        )
        .await;

        let (mut important, mut partial) = (Vec::new(), Vec::new());
        while let Ok(event) = important_rx.try_recv() {
            important.push(event);
        }
        while let Ok(event) = partial_rx.try_recv() {
            partial.push(event);
        }
        Collected {
            important,
            partial,
            store,
        }
    }

    /// A mic line repeating a loopback line, with its partial on screen.
    fn bleed_events() -> Vec<(ChannelId, SttEvent)> {
        vec![
            (
                ChannelId::Loopback,
                SttEvent::Final(segment(ChannelId::Loopback, SENTENCE)),
            ),
            (
                ChannelId::Mic,
                SttEvent::Partial(segment(ChannelId::Mic, SENTENCE)),
            ),
            (
                ChannelId::Mic,
                SttEvent::Final(segment(ChannelId::Mic, SENTENCE)),
            ),
        ]
    }

    const SENTENCE: &str = "the quarterly numbers look good to me";

    #[tokio::test]
    async fn a_final_dropped_as_bleed_clears_the_partial_it_would_have_replaced() {
        let run = collect(bleed_events(), "bleed", true).await;

        assert_eq!(run.cleared(), [("s1", "mic")], "{:?}", run.important);
        // The lossy lane could shed it, so it must not ride there.
        assert_eq!(run.partial.len(), 1, "{:?}", run.partial);
        assert!(matches!(run.partial[0], WsEvent::Partial { .. }));
    }

    #[tokio::test]
    async fn dropped_bleed_is_counted_in_the_session_meta() {
        let run = collect(bleed_events(), "bleed-count", true).await;
        assert_eq!(run.meta()["dropped"]["bleed_finals"], 1);
    }

    #[tokio::test]
    async fn a_model_download_reports_how_far_its_part_file_has_got() {
        let dir = std::env::temp_dir().join(format!("auricle-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("model.bin.part");
        let (tx, mut rx) = broadcast::channel(16);
        let watch = tokio::spawn(report_download_progress(
            part.clone(),
            "s1".to_string(),
            "base".to_string(),
            4 * 1_048_576,
            tx,
            Duration::from_millis(5),
        ));

        // Nothing to report until the file exists.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(rx.try_recv().is_err());

        std::fs::write(&part, vec![0u8; 2 * 1_048_576]).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            WsEvent::ModelDownloadProgress {
                session,
                model,
                done_mb,
                size_mb,
            } => assert_eq!(
                (session.as_str(), model.as_str(), done_mb, size_mb),
                ("s1", "base", 2, 4)
            ),
            other => panic!("{other:?}"),
        }

        // An unchanged file says nothing new.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(rx.try_recv().is_err());
        watch.abort();
    }

    #[tokio::test]
    async fn a_loss_record_leaves_the_bleed_count_standing() {
        // Both live under `dropped` and come from different tasks.
        let engine = quiet_engine("loss-record");
        let store = engine.store();
        store
            .create_session("s1", "t", 0, "fake", &serde_json::json!({}))
            .unwrap();
        store
            .set_session_meta("s1", "dropped.bleed_finals", &serde_json::json!(1))
            .unwrap();

        let capture = HashMap::from([(ChannelId::Mic, 250)]);
        engine.record_losses("s1", &capture, &HashMap::new());

        let meta = store.get_session("s1").unwrap().unwrap().meta;
        assert_eq!(meta["dropped"]["bleed_finals"], 1);
        assert_eq!(meta["dropped"]["mic"]["capture_ms"], 250);
    }

    #[tokio::test]
    async fn with_the_filter_off_nothing_is_dropped_or_counted() {
        let run = collect(bleed_events(), "bleed-off", false).await;

        assert!(run.cleared().is_empty(), "{:?}", run.important);
        let mic_finals = run
            .important
            .iter()
            .filter(|e| matches!(e, WsEvent::Final { channel, .. } if channel == "mic"))
            .count();
        assert_eq!(mic_finals, 1, "{:?}", run.important);
        assert!(run.meta().get("dropped").is_none(), "{:?}", run.meta());
    }

    #[tokio::test]
    async fn a_kept_final_clears_nothing_and_neither_does_a_stale_partial() {
        let run = collect(
            vec![
                (
                    ChannelId::Mic,
                    SttEvent::Final(segment(ChannelId::Mic, SENTENCE)),
                ),
                // Older than the final above, so the assembler suppresses it.
                (
                    ChannelId::Mic,
                    SttEvent::Partial(segment(ChannelId::Mic, SENTENCE)),
                ),
            ],
            "kept",
            true,
        )
        .await;

        assert!(run.cleared().is_empty(), "{:?}", run.important);
        assert!(run.partial.is_empty(), "{:?}", run.partial);
        assert!(run.meta().get("dropped").is_none(), "{:?}", run.meta());
    }

    // ---------- loss records ----------

    #[test]
    fn the_loss_record_covers_only_channels_that_lost_something() {
        assert_eq!(dropped_record(&HashMap::new(), &HashMap::new()), None);

        let capture = HashMap::from([(ChannelId::Mic, 40), (ChannelId::Loopback, 0)]);
        let shed = HashMap::from([(
            ChannelId::Mic,
            SttDrops {
                chunks: 5,
                finals: 2,
            },
        )]);
        assert_eq!(
            dropped_record(&capture, &shed),
            Some(serde_json::json!({
                "mic": {"capture_ms": 40, "stt_chunks": 5, "stt_finals": 2}
            }))
        );
    }

    #[tokio::test]
    async fn the_driver_hands_back_what_its_session_shed() {
        struct SheddingSession(Box<dyn SttSession>);

        #[async_trait]
        impl SttSession for SheddingSession {
            async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
                self.0.feed(chunk).await
            }
            async fn next_event(&mut self) -> Option<SttEvent> {
                self.0.next_event().await
            }
            async fn finish(&mut self) -> Result<Vec<Segment>> {
                self.0.finish().await
            }
            fn drops(&self) -> SttDrops {
                SttDrops {
                    chunks: 7,
                    finals: 2,
                }
            }
        }

        let (inner, _fed) = silent_session();
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        drop(batch_tx);

        let shed = drive_channel(
            batch_rx,
            channel_pipeline(),
            Box::new(SheddingSession(inner)),
            ChannelId::Mic,
            ev_tx,
            Instant::now(),
        )
        .await;

        assert_eq!(
            shed,
            SttDrops {
                chunks: 7,
                finals: 2
            }
        );
    }
}
