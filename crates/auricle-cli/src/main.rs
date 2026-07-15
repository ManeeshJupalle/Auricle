use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

mod record;
mod swap;

use auricle_capture::{
    drain_into, enumerate, start_loopback, start_mic, CaptureConsumer, CaptureHandle, DeviceKind,
    MonoResampler,
};
use auricle_core::{ChannelId, Config, Error, Result};
use clap::{Parser, Subcommand};

/// Target rate for the STT pipeline (Whisper/Parakeet/Silero native rate).
const TARGET_RATE_HZ: u32 = 16_000;

#[derive(Parser)]
#[command(
    name = "auricle",
    version,
    about = "Local-first meeting transcription engine"
)]
struct Cli {
    /// Path to auricle.toml (built-in defaults are used when omitted)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List capture-capable audio devices and their formats
    Devices,
    /// Capture mic + system loopback simultaneously and write WAV files
    Capture {
        /// Capture duration in seconds
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Directory for mic_raw.wav, mic_16k.wav, loopback_raw.wav, loopback_16k.wav
        #[arg(long, default_value = "./captures")]
        out_dir: PathBuf,
    },
    /// Live transcription of mic + system audio to the terminal
    Record {
        /// STT provider id (defaults to [stt].provider from config)
        #[arg(long)]
        stt: Option<String>,
        /// Whisper model for whisper-local: base.en | small.en
        #[arg(long)]
        model: Option<String>,
    },
    /// List STT providers and their readiness (model/key present)
    Providers,
    /// Run the Auricle daemon (REST + WebSocket API)
    Serve {
        /// Listen address override (default: [server].bind from config)
        #[arg(long)]
        bind: Option<String>,
    },
    /// Export a stored session transcript
    Export {
        /// Session id (see GET /api/v1/sessions or the sessions table)
        id: String,
        /// Export as markdown (the only supported format)
        #[arg(long)]
        md: bool,
    },
    /// Capture the active window and print its text (screen OCR)
    Peek {
        /// Print the full ScreenContext as JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Devices => cmd_devices(),
        Command::Capture { seconds, out_dir } => {
            cmd_capture(cli.config.as_deref(), seconds, &out_dir)
        }
        Command::Record { stt, model } => record::run(record::RecordArgs {
            stt,
            model,
            config: cli.config,
        }),
        Command::Providers => cmd_providers(cli.config.as_deref()),
        Command::Serve { bind } => cmd_serve(cli.config.as_deref(), bind),
        Command::Export { id, md } => cmd_export(&id, md),
        Command::Peek { json } => cmd_peek(json),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn load_config(path: Option<&Path>) -> Result<Config> {
    match path {
        Some(p) => Config::load(p),
        None => Ok(Config::default()),
    }
}

fn cmd_devices() -> Result<()> {
    let devices = enumerate()?;

    println!("Input devices (microphone capture):");
    let mut any = false;
    for d in devices.iter().filter(|d| d.kind == DeviceKind::Input) {
        any = true;
        println!(
            "  {} {:<45} {} Hz, {} ch, {}{}",
            if d.is_default { "*" } else { " " },
            d.name,
            d.sample_rate_hz,
            d.channels,
            d.sample_format,
            if d.is_default { "  [default]" } else { "" },
        );
    }
    if !any {
        println!("  (none)");
    }

    println!("\nLoopback devices (system audio via WASAPI loopback on outputs):");
    any = false;
    for d in devices.iter().filter(|d| d.kind == DeviceKind::Loopback) {
        any = true;
        println!(
            "  {} {:<45} {} Hz, {} ch, {}{}",
            if d.is_default { "*" } else { " " },
            d.name,
            d.sample_rate_hz,
            d.channels,
            d.sample_format,
            if d.is_default { "  [default]" } else { "" },
        );
    }
    if !any {
        println!("  (none)");
    }

    Ok(())
}

fn cmd_providers(config: Option<&Path>) -> Result<()> {
    let cfg = load_config(config)?;
    let data_dir = auricle_stt::default_data_dir()?;
    let statuses = auricle_stt::provider_statuses(&cfg, &data_dir);

    println!("stt providers (default: {}):", cfg.stt.provider);
    for s in statuses {
        println!(
            "  {} {:<15} {:<16} {:<10} {}",
            if s.id == cfg.stt.provider { "*" } else { " " },
            s.id,
            s.kind.to_string(),
            if s.ready { "ready" } else { "not ready" },
            s.detail
        );
    }
    Ok(())
}

fn cmd_serve(config: Option<&Path>, bind: Option<String>) -> Result<()> {
    let cfg = load_config(config)?;
    let data_root = auricle_server::default_data_root()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(auricle_server::serve(cfg, data_root, bind))
}

fn cmd_peek(json: bool) -> Result<()> {
    use auricle_vision::{ScreenReader, WindowsOcrReader};

    let reader = Arc::new(WindowsOcrReader::new());

    // Warm the D3D device + OCR engine (~300 ms) while the user focuses
    // the target window, so the printed timing is the true capture→text
    // cost. Warm-up errors are ignored here: the capture below re-runs
    // initialization and surfaces the same typed error.
    let warm = {
        let reader = reader.clone();
        thread::spawn(move || {
            let _ = reader.warm_up();
        })
    };
    for s in (1..=3).rev() {
        eprint!("\rcapturing the active window in {s}\u{2026} ");
        let _ = std::io::stderr().flush();
        thread::sleep(Duration::from_secs(1));
    }
    eprintln!();
    let _ = warm.join();

    let started = std::time::Instant::now();
    let ctx = reader
        .capture_active_window(None)
        .map_err(|e| Error::Vision(e.to_string()))?;
    let total_ms = started.elapsed().as_millis() as u64;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ctx).map_err(|e| Error::Vision(e.to_string()))?
        );
        eprintln!(
            "capture\u{2192}text: {total_ms} ms ({} ms capture, {} ms ocr)",
            total_ms.saturating_sub(ctx.ocr_ms),
            ctx.ocr_ms
        );
    } else {
        println!(
            "\u{2500}\u{2500} {} ({}) \u{2500}\u{2500}",
            ctx.window_title, ctx.app_name
        );
        println!("{}", ctx.text);
        println!(
            "\ncapture\u{2192}text: {total_ms} ms ({} ms capture, {} ms ocr)",
            total_ms.saturating_sub(ctx.ocr_ms),
            ctx.ocr_ms
        );
    }
    Ok(())
}

fn cmd_export(id: &str, md: bool) -> Result<()> {
    if !md {
        return Err(Error::Config(
            "specify --md (markdown is the only supported export format)".to_string(),
        ));
    }
    let data_root = auricle_server::default_data_root()?;
    let store = auricle_server::Store::open(&data_root.join("auricle.db"))?;
    let session = store
        .get_session(id)?
        .ok_or_else(|| Error::Config(format!("no session \"{id}\"")))?;
    let segments = store.get_segments(id)?;
    let summaries = store.get_summaries(id)?;
    print!(
        "{}",
        auricle_server::render_markdown_full(&session, &segments, &summaries)
    );
    Ok(())
}

/// Everything a drain thread accumulated for one channel.
struct DrainResult {
    channel: ChannelId,
    native_rate_hz: u32,
    dropped: u64,
    raw: Vec<f32>,
    resampled: Vec<f32>,
}

fn spawn_drain(
    mut c: CaptureConsumer,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<Result<DrainResult>> {
    thread::spawn(move || {
        let mut resampler = MonoResampler::new(c.native_rate_hz, TARGET_RATE_HZ)?;
        let mut raw: Vec<f32> = Vec::new();
        let mut resampled: Vec<f32> = Vec::new();
        let mut scratch: Vec<f32> = Vec::new();
        loop {
            scratch.clear();
            let n = drain_into(&mut c.consumer, &mut scratch);
            if n > 0 {
                raw.extend_from_slice(&scratch);
                resampler.process(&scratch, &mut resampled)?;
            } else if stop.load(Ordering::Relaxed) {
                // Streams are paused before `stop` is set, so an empty ring
                // here means everything has been drained.
                break;
            } else {
                thread::sleep(Duration::from_millis(5));
            }
        }
        resampler.finish(&mut resampled)?;
        Ok(DrainResult {
            channel: c.channel,
            native_rate_hz: c.native_rate_hz,
            dropped: c.dropped.load(Ordering::Relaxed),
            raw,
            resampled,
        })
    })
}

fn cmd_capture(config: Option<&Path>, seconds: u64, out_dir: &Path) -> Result<()> {
    let cfg = load_config(config)?;
    std::fs::create_dir_all(out_dir)?;

    let (mic_handle, mic_consumer) = start_mic(&cfg.audio.mic_device)?;
    let (loop_handle, loop_consumer) = start_loopback(&cfg.audio.loopback_device)?;

    println!("capturing for {seconds}s:");
    print_stream_config(&mic_handle);
    print_stream_config(&loop_handle);
    println!("note: loopback only delivers samples while system audio is playing\n");

    let stop = Arc::new(AtomicBool::new(false));
    let mic_worker = spawn_drain(mic_consumer, stop.clone());
    let loop_worker = spawn_drain(loop_consumer, stop.clone());

    for s in 1..=seconds {
        thread::sleep(Duration::from_secs(1));
        print!("\r  {s}/{seconds}s");
        let _ = std::io::stdout().flush();
    }
    println!();

    // Stop callbacks first, then let the drain threads empty the rings.
    mic_handle.pause()?;
    loop_handle.pause()?;
    stop.store(true, Ordering::Relaxed);

    let mic = join_drain(mic_worker)?;
    let lp = join_drain(loop_worker)?;

    write_channel(out_dir, "mic", &mic)?;
    write_channel(out_dir, "loopback", &lp)?;

    for handle in [&mic_handle, &loop_handle] {
        if let Some(e) = handle.stream_error() {
            eprintln!(
                "warning: stream error on {} channel during capture: {e}",
                handle.channel
            );
        }
    }
    if lp.raw.is_empty() {
        eprintln!(
            "warning: loopback captured 0 samples — was any system audio playing during capture?"
        );
    }

    Ok(())
}

fn print_stream_config(h: &CaptureHandle) {
    println!(
        "  {:<9} {} ({} Hz, {} ch, {})",
        format!("{}:", h.channel),
        h.device_name,
        h.native_rate_hz,
        h.native_channels,
        h.sample_format
    );
}

fn join_drain(worker: thread::JoinHandle<Result<DrainResult>>) -> Result<DrainResult> {
    worker
        .join()
        .map_err(|_| Error::Audio("drain thread panicked".to_string()))?
}

fn write_channel(out_dir: &Path, prefix: &str, r: &DrainResult) -> Result<()> {
    let raw_path = out_dir.join(format!("{prefix}_raw.wav"));
    let k16_path = out_dir.join(format!("{prefix}_16k.wav"));
    write_wav(&raw_path, r.native_rate_hz, &r.raw)?;
    write_wav(&k16_path, TARGET_RATE_HZ, &r.resampled)?;

    println!("── {} ──", r.channel);
    println!(
        "  raw: {:>9} samples @ {} Hz = {:>6.2} s -> {}",
        r.raw.len(),
        r.native_rate_hz,
        r.raw.len() as f64 / r.native_rate_hz as f64,
        raw_path.display()
    );
    println!(
        "  16k: {:>9} samples @ {} Hz = {:>6.2} s -> {}",
        r.resampled.len(),
        TARGET_RATE_HZ,
        r.resampled.len() as f64 / TARGET_RATE_HZ as f64,
        k16_path.display()
    );
    println!("  dropped samples (ring overrun): {}", r.dropped);
    Ok(())
}

fn write_wav(path: &Path, sample_rate_hz: u32, samples: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate_hz,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| Error::Io(std::io::Error::other(format!("{}: {e}", path.display()))))?;
    for s in samples {
        writer
            .write_sample(*s)
            .map_err(|e| Error::Io(std::io::Error::other(format!("{}: {e}", path.display()))))?;
    }
    writer
        .finalize()
        .map_err(|e| Error::Io(std::io::Error::other(format!("{}: {e}", path.display()))))?;
    Ok(())
}
