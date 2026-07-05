use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Devices => cmd_devices(),
        Command::Capture { seconds, out_dir } => {
            cmd_capture(cli.config.as_deref(), seconds, &out_dir)
        }
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
