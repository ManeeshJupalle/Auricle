//! `auricle record` — live transcription of mic + loopback to the terminal,
//! with runtime provider swapping (hotkey `s`).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use auricle_capture::{drain_into, start_loopback, start_mic, CaptureConsumer, MonoResampler};
use auricle_core::{ChannelId, Config, Error, Result, Segment, SttEvent};
use auricle_pipeline::{
    Assembler, ChannelPipeline, PipelineConfig, PipelineEvent, SileroDetector, TranscriptEvent,
};
use auricle_stt::{provider_ids, provider_statuses, SessionCfg};
use tokio::sync::mpsc;

use crate::swap::{DriveState, DriverEvent, ProviderFactory, SwapController};

const TARGET_RATE_HZ: u32 = 16_000;

/// (channel, provider id, event, session-clock ms at emit)
type EventTuple = (ChannelId, &'static str, DriverEvent, u64);

pub struct RecordArgs {
    pub stt: Option<String>,
    pub model: Option<String>,
    pub config: Option<PathBuf>,
}

pub fn run(args: RecordArgs) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(args))
}

/// Monotonic session clock; t=0 is capture start.
#[derive(Clone)]
struct SessionClock(Instant);

impl SessionClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
}

async fn run_async(args: RecordArgs) -> Result<()> {
    enable_vt();

    let mut cfg = match &args.config {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };
    if let Some(m) = &args.model {
        cfg.stt.whisper_local.model = m.clone();
    }
    let initial_id = args.stt.unwrap_or_else(|| cfg.stt.provider.clone());
    let initial_static = provider_ids()
        .iter()
        .find(|i| **i == initial_id)
        .copied()
        .ok_or_else(|| {
            Error::Stt(format!(
                "unknown stt provider \"{initial_id}\" (available: {})",
                provider_ids().join(", ")
            ))
        })?;

    let data_dir = auricle_stt::default_data_dir()?;

    // Swap cycle: every ready provider, always including the requested one
    // (it may be "not ready" only because its model downloads on first use).
    let mut cycle: Vec<&'static str> = provider_statuses(&cfg, &data_dir)
        .iter()
        .filter(|s| s.ready)
        .map(|s| s.id)
        .collect();
    if !cycle.contains(&initial_static) {
        cycle.insert(0, initial_static);
    }
    let initial_idx = cycle.iter().position(|i| *i == initial_static).unwrap();

    let session_id = format!(
        "rec-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );
    let session_cfg = SessionCfg {
        session_id: session_id.clone(),
        language: "en".to_string(),
    };

    let factory = Arc::new(ProviderFactory::new(
        cfg.clone(),
        data_dir,
        cycle.clone(),
        session_cfg,
    ));
    let swap = Arc::new(SwapController::new(cycle.clone(), initial_idx));

    eprintln!("preparing stt provider {initial_static} ...");
    let mic_state = DriveState::start(factory.as_ref(), initial_idx).await?;
    let loop_state = DriveState::start(factory.as_ref(), initial_idx).await?;

    let pipe_cfg = PipelineConfig::default();
    let mic_pipeline = ChannelPipeline::new(
        &session_id,
        ChannelId::Mic,
        SileroDetector::new()?,
        &pipe_cfg,
    );
    let loop_pipeline = ChannelPipeline::new(
        &session_id,
        ChannelId::Loopback,
        SileroDetector::new()?,
        &pipe_cfg,
    );

    // Start capture last so the rings don't fill during provider setup.
    let (mic_handle, mic_consumer) = start_mic(&cfg.audio.mic_device)?;
    let (loop_handle, loop_consumer) = start_loopback(&cfg.audio.loopback_device)?;
    let clock = SessionClock(Instant::now());

    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<EventTuple>();
    let hotkey = spawn_hotkey_thread(swap.clone(), ev_tx.clone());

    eprintln!(
        "recording ({initial_static}). speak into '{}'; system audio from '{}'. Ctrl+C to stop.{}\n",
        mic_handle.device_name,
        loop_handle.device_name,
        if hotkey && cycle.len() > 1 {
            format!(" press 's' to cycle providers ({}).", cycle.join(" → "))
        } else {
            String::new()
        }
    );

    let stop = Arc::new(AtomicBool::new(false));
    let (mic_batch_tx, mic_batch_rx) = mpsc::unbounded_channel();
    let (loop_batch_tx, loop_batch_rx) = mpsc::unbounded_channel();
    let mic_drain = spawn_drain(mic_consumer, stop.clone(), clock.clone(), mic_batch_tx);
    let loop_drain = spawn_drain(loop_consumer, stop.clone(), clock.clone(), loop_batch_tx);

    let mic_driver = tokio::spawn(drive_channel(
        ChannelId::Mic,
        mic_batch_rx,
        mic_pipeline,
        factory.clone(),
        swap.clone(),
        mic_state,
        ev_tx.clone(),
        clock.clone(),
    ));
    let loop_driver = tokio::spawn(drive_channel(
        ChannelId::Loopback,
        loop_batch_rx,
        loop_pipeline,
        factory.clone(),
        swap.clone(),
        loop_state,
        ev_tx,
        clock.clone(),
    ));

    let collector = tokio::spawn(collect_events(ev_rx, session_id.clone()));

    tokio::signal::ctrl_c()
        .await
        .map_err(|e| Error::Io(std::io::Error::other(format!("ctrl-c handler: {e}"))))?;
    eprintln!("\r\x1b[K\nstopping (flushing spans and finishing inference)...");

    mic_handle.pause()?;
    loop_handle.pause()?;
    stop.store(true, Ordering::Relaxed);

    let mic_finals = mic_driver
        .await
        .map_err(|e| Error::Stt(format!("mic driver panicked: {e}")))??;
    let loop_finals = loop_driver
        .await
        .map_err(|e| Error::Stt(format!("loopback driver panicked: {e}")))??;
    let (transcript, latencies) = collector
        .await
        .map_err(|e| Error::Stt(format!("collector panicked: {e}")))?;

    let duration_ms = clock.now_ms();
    let mic_dropped = join_drain(mic_drain)?;
    let loop_dropped = join_drain(loop_drain)?;

    for handle in [&mic_handle, &loop_handle] {
        if let Some(e) = handle.stream_error() {
            eprintln!("warning: stream error on {}: {e}", handle.channel);
        }
    }

    print_summary(
        duration_ms,
        &transcript,
        &latencies,
        mic_finals.len(),
        loop_finals.len(),
        mic_dropped,
        loop_dropped,
    );
    Ok(())
}

fn spawn_drain(
    mut c: CaptureConsumer,
    stop: Arc<AtomicBool>,
    clock: SessionClock,
    tx: mpsc::UnboundedSender<(Vec<f32>, u64)>,
) -> std::thread::JoinHandle<Result<u64>> {
    std::thread::spawn(move || {
        let mut resampler = MonoResampler::new(c.native_rate_hz, TARGET_RATE_HZ)?;
        let mut scratch: Vec<f32> = Vec::new();
        let mut out: Vec<f32> = Vec::new();
        loop {
            scratch.clear();
            let n = drain_into(&mut c.consumer, &mut scratch);
            if n > 0 {
                out.clear();
                resampler.process(&scratch, &mut out)?;
                if !out.is_empty() && tx.send((out.clone(), clock.now_ms())).is_err() {
                    break;
                }
            } else if stop.load(Ordering::Relaxed) {
                out.clear();
                resampler.finish(&mut out)?;
                if !out.is_empty() {
                    let _ = tx.send((out.clone(), clock.now_ms()));
                }
                break;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(c.dropped.load(Ordering::Relaxed))
    })
}

fn join_drain(h: std::thread::JoinHandle<Result<u64>>) -> Result<u64> {
    h.join()
        .map_err(|_| Error::Audio("drain thread panicked".to_string()))?
}

/// Owns one channel's pipeline and STT session: feeds drained batches
/// through VAD/chunker into the session, applies provider swaps at chunk
/// boundaries, and forwards session events.
#[allow(clippy::too_many_arguments)]
async fn drive_channel(
    channel: ChannelId,
    mut batch_rx: mpsc::UnboundedReceiver<(Vec<f32>, u64)>,
    mut pipeline: ChannelPipeline<SileroDetector>,
    factory: Arc<ProviderFactory>,
    swap: Arc<SwapController>,
    mut state: DriveState,
    ev_tx: mpsc::UnboundedSender<EventTuple>,
    clock: SessionClock,
) -> Result<Vec<Segment>> {
    let mut feeding_done = false;
    while !feeding_done {
        loop {
            match batch_rx.try_recv() {
                Ok((samples, wall_ms)) => {
                    for chunk in pipeline.push_batch(&samples, wall_ms) {
                        let idx_before = state.current_idx;
                        {
                            let fw = &ev_tx;
                            let now = clock.now_ms();
                            state
                                .apply_swap_if_requested(&swap, factory.as_ref(), |pid, ev| {
                                    let _ = fw.send((channel, pid, DriverEvent::Stt(ev), now));
                                })
                                .await?;
                        }
                        if state.current_idx != idx_before {
                            let _ = ev_tx.send((
                                channel,
                                state.provider_id,
                                DriverEvent::Swapped {
                                    to: state.provider_id,
                                },
                                clock.now_ms(),
                            ));
                        }
                        state.feed_chunk(chunk).await?;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    for chunk in pipeline.flush() {
                        state.feed_chunk(chunk).await?;
                    }
                    feeding_done = true;
                    break;
                }
            }
        }
        if feeding_done {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(50), state.session.next_event()).await {
            Ok(Some(ev)) => {
                if matches!(ev, SttEvent::Final(_)) {
                    state.outstanding_finals -= 1;
                }
                let _ = ev_tx.send((
                    channel,
                    state.provider_id,
                    DriverEvent::Stt(ev),
                    clock.now_ms(),
                ));
            }
            Ok(None) => break,
            Err(_) => {} // timeout: loop back and ingest batches
        }
    }

    let finals = state.session.finish().await?;
    while let Some(ev) = state.session.next_event().await {
        let _ = ev_tx.send((
            channel,
            state.provider_id,
            DriverEvent::Stt(ev),
            clock.now_ms(),
        ));
    }
    state.collected_finals.extend(finals);
    Ok(state.collected_finals)
}

/// Single consumer of both channels' events: assembles the transcript,
/// drives the live display, and records chunk->final latency per provider.
async fn collect_events(
    mut ev_rx: mpsc::UnboundedReceiver<EventTuple>,
    session_id: String,
) -> (Vec<TranscriptEvent>, HashMap<&'static str, Vec<u64>>) {
    let mut assembler = Assembler::new(&session_id);
    let mut printer = Printer::new();
    let mut latencies: HashMap<&'static str, Vec<u64>> = HashMap::new();

    while let Some((channel, provider, ev, emit_wall_ms)) = ev_rx.recv().await {
        match ev {
            DriverEvent::SwapRequested { to } => {
                printer.notice(&format!("provider swap requested → {to} (at next chunk)"));
            }
            DriverEvent::Swapped { to } => {
                printer.notice(&format!("[{channel}] now using {to}"));
            }
            DriverEvent::Stt(SttEvent::Error(message)) => {
                printer.print(&PipelineEvent::SttError {
                    channel,
                    message: format!("{provider}: {message}"),
                });
            }
            DriverEvent::Stt(ev) => {
                if let SttEvent::Final(seg) = &ev {
                    latencies
                        .entry(provider)
                        .or_default()
                        .push(emit_wall_ms.saturating_sub(seg.t_end_ms));
                }
                if let Some(out) = assembler.ingest(ev) {
                    printer.print(&out);
                }
            }
        }
    }
    printer.clear_partial();
    (assembler.transcript().to_vec(), latencies)
}

struct Printer {
    partial_active: bool,
}

impl Printer {
    fn new() -> Self {
        Printer {
            partial_active: false,
        }
    }

    fn print(&mut self, ev: &PipelineEvent) {
        match ev {
            PipelineEvent::Transcript(t) if t.is_final => {
                self.clear_partial();
                println!(
                    "\x1b[1m[{}] [{}]\x1b[0m {}",
                    fmt_ts(t.t_start_ms),
                    t.speaker,
                    t.text
                );
            }
            PipelineEvent::Transcript(t) => {
                let line = format!("[{}] {}", t.speaker, t.text);
                print!("\r\x1b[K\x1b[2m{}\x1b[0m", tail(&line, 110));
                let _ = std::io::stdout().flush();
                self.partial_active = true;
            }
            PipelineEvent::SttError { channel, message } => {
                self.clear_partial();
                eprintln!("\x1b[33mwarning ({channel}): {message}\x1b[0m");
            }
        }
    }

    fn notice(&mut self, msg: &str) {
        self.clear_partial();
        println!("\x1b[36m── {msg}\x1b[0m");
    }

    fn clear_partial(&mut self) {
        if self.partial_active {
            print!("\r\x1b[K");
            let _ = std::io::stdout().flush();
            self.partial_active = false;
        }
    }
}

/// Last `max` chars of a partial line (the growing tail is the news).
fn tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let skip = count - max + 1;
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

fn fmt_ts(ms: u64) -> String {
    format!("{:02}:{:02}", ms / 60_000, (ms / 1000) % 60)
}

#[allow(clippy::too_many_arguments)]
fn print_summary(
    duration_ms: u64,
    transcript: &[TranscriptEvent],
    latencies: &HashMap<&'static str, Vec<u64>>,
    mic_finals: usize,
    loop_finals: usize,
    mic_dropped: u64,
    loop_dropped: u64,
) {
    let you = transcript.iter().filter(|t| t.speaker == "You").count();
    let them = transcript.iter().filter(|t| t.speaker == "Them").count();

    println!("\nsession summary");
    println!("  duration: {:.1} s", duration_ms as f64 / 1000.0);
    println!(
        "  transcript segments: {} (You: {you}, Them: {them}; stt finals mic {mic_finals} / loopback {loop_finals})",
        transcript.len()
    );
    if latencies.is_empty() {
        println!("  chunk→final latency: n/a (no final segments)");
    } else {
        let mut providers: Vec<_> = latencies.keys().collect();
        providers.sort();
        for provider in providers {
            let values = &latencies[provider];
            if let Some((p50, p95)) = percentiles(values) {
                println!(
                    "  chunk→final latency [{provider}]: p50 {p50} ms, p95 {p95} ms (n={})",
                    values.len()
                );
            }
        }
    }
    println!("  ring overruns: mic {mic_dropped} samples, loopback {loop_dropped} samples");
}

fn percentiles(values: &[u64]) -> Option<(u64, u64)> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let p50 = v[(v.len() - 1) / 2];
    let p95 = v[((v.len() as f64 * 0.95).ceil() as usize - 1).min(v.len() - 1)];
    Some((p50, p95))
}

/// Enable ANSI escape processing on legacy conhost (Windows Terminal already
/// handles it). Failures (redirected output, older builds) are ignored — the
/// worst case is visible escape codes.
fn enable_vt() {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    unsafe {
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if let Ok(handle) = GetStdHandle(which) {
                let mut mode = CONSOLE_MODE(0);
                if GetConsoleMode(handle, &mut mode).is_ok() {
                    let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                }
            }
        }
    }
}

/// Debug hotkey: `s` cycles the STT provider. Reads raw console key events
/// on a dedicated thread; returns false (no hotkey) when stdin is not an
/// interactive console (redirected/piped) or only one provider is usable.
fn spawn_hotkey_thread(
    swap: Arc<SwapController>,
    ev_tx: mpsc::UnboundedSender<EventTuple>,
) -> bool {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, CONSOLE_MODE, STD_INPUT_HANDLE,
    };
    if swap.len() < 2 {
        return false;
    }
    unsafe {
        let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) else {
            return false;
        };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(handle, &mut mode).is_err() {
            return false; // stdin is not a console
        }
    }
    std::thread::spawn(move || hotkey_loop(swap, ev_tx));
    true
}

fn hotkey_loop(swap: Arc<SwapController>, ev_tx: mpsc::UnboundedSender<EventTuple>) {
    use windows::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    };
    unsafe {
        let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) else {
            return;
        };
        let mut records: [INPUT_RECORD; 16] = std::mem::zeroed();
        loop {
            let mut read = 0u32;
            if ReadConsoleInputW(handle, &mut records, &mut read).is_err() {
                return;
            }
            for record in &records[..read as usize] {
                if record.EventType as u32 == KEY_EVENT {
                    let key = record.Event.KeyEvent;
                    let ch = key.uChar.UnicodeChar;
                    if key.bKeyDown.as_bool() && (ch == u16::from(b's') || ch == u16::from(b'S')) {
                        let to = swap.request_next();
                        if ev_tx
                            .send((ChannelId::Mic, to, DriverEvent::SwapRequested { to }, 0))
                            .is_err()
                        {
                            return; // session over
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_math() {
        assert_eq!(percentiles(&[]), None);
        assert_eq!(percentiles(&[100]), Some((100, 100)));
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentiles(&v), Some((50, 95)));
    }

    #[test]
    fn timestamp_format() {
        assert_eq!(fmt_ts(0), "00:00");
        assert_eq!(fmt_ts(61_500), "01:01");
        assert_eq!(fmt_ts(3_599_000), "59:59");
    }

    #[test]
    fn tail_truncates_from_the_front() {
        assert_eq!(tail("short", 10), "short");
        let t = tail("abcdefghij", 5);
        assert_eq!(t, "…ghij");
    }
}
