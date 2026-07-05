//! Latency benchmark: feeds the fixture WAV through the real pipeline
//! (Silero VAD → chunker → STT provider) at exactly real-time pacing and
//! measures, per event, wall-clock arrival minus the audio time it covers:
//!
//!   capture→partial = recv_wall_ms - t_end_ms(partial)
//!   capture→final   = recv_wall_ms - t_end_ms(final)
//!
//! Because feeding is real-time paced (10 ms batches on a wall-clock
//! schedule), these are honest end-to-end numbers for everything after
//! capture. WASAPI adds ~10 ms callback granularity on top (Phase 1).
//!
//! Usage: latency-bench --stt <whisper-local|deepgram|groq-whisper> \
//!            [--model base.en] [--wav path] [--repeat 2]

use std::time::{Duration, Instant};

use auricle_core::{ChannelId, Config, SttEvent};
use auricle_pipeline::{ChannelPipeline, PipelineConfig, SileroDetector};
use auricle_stt::{create_provider, SessionCfg};

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[idx]
}

struct Args {
    stt: String,
    model: Option<String>,
    wav: String,
    repeat: usize,
}

fn parse_args() -> Args {
    let mut args = Args {
        stt: "whisper-local".to_string(),
        model: None,
        wav: "fixtures/tts_loopback_16k.wav".to_string(),
        repeat: 2,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().expect("flag value");
        match flag.as_str() {
            "--stt" => args.stt = val(),
            "--model" => args.model = Some(val()),
            "--wav" => args.wav = val(),
            "--repeat" => args.repeat = val().parse().expect("repeat count"),
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let mut cfg = Config::default();
    if let Some(m) = &args.model {
        cfg.stt.whisper_local.model = m.clone();
    }

    let mut reader = hound::WavReader::open(&args.wav).expect("fixture wav");
    assert_eq!(reader.spec().sample_rate, 16_000, "need 16 kHz fixture");
    let samples: Vec<f32> = match reader.spec().sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
    };
    let audio_secs = samples.len() as f64 / 16_000.0;

    let models_dir = std::env::var("LOCALAPPDATA")
        .map(|b| std::path::PathBuf::from(b).join("auricle").join("models"))
        .expect("LOCALAPPDATA");
    eprintln!("preparing provider {} ...", args.stt);
    let provider = create_provider(&args.stt, &cfg, &models_dir)
        .await
        .expect("provider");

    let mut partial_lat: Vec<u64> = Vec::new();
    let mut final_lat: Vec<u64> = Vec::new();
    let mut finals_count = 0usize;

    for run in 0..args.repeat {
        eprintln!(
            "run {}/{}: {audio_secs:.1}s of audio at real time",
            run + 1,
            args.repeat
        );
        let mut session = provider
            .start_session(&SessionCfg {
                session_id: format!("bench-{run}"),
                language: "en".to_string(),
            })
            .await
            .expect("session");
        let mut pipeline = ChannelPipeline::new(
            "bench",
            ChannelId::Loopback,
            SileroDetector::new().expect("vad"),
            &PipelineConfig::default(),
        );

        let start = Instant::now();
        let mut fed_batches = 0u64;
        let mut done_feeding = false;
        let mut outstanding: i64 = 0;
        let total_batches = samples.len().div_ceil(160);

        loop {
            // Feed every batch exactly on its wall-clock schedule.
            while !done_feeding {
                let due = Duration::from_millis(fed_batches * 10);
                if start.elapsed() < due {
                    break;
                }
                let lo = (fed_batches as usize) * 160;
                let hi = (lo + 160).min(samples.len());
                let wall_ms = start.elapsed().as_millis() as u64;
                for chunk in pipeline.push_batch(&samples[lo..hi], wall_ms) {
                    let feed = match chunk.kind {
                        auricle_core::ChunkKind::Final => {
                            outstanding += 1;
                            true
                        }
                        auricle_core::ChunkKind::Interim => outstanding <= 1,
                    };
                    if feed {
                        session.feed(chunk).await.expect("feed");
                    }
                }
                fed_batches += 1;
                if fed_batches as usize >= total_batches {
                    for chunk in pipeline.flush() {
                        if chunk.kind == auricle_core::ChunkKind::Final {
                            session.feed(chunk).await.expect("feed");
                        }
                    }
                    done_feeding = true;
                }
            }

            if done_feeding {
                break;
            }
            // Poll events between batch deadlines.
            if let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(5), session.next_event()).await
            {
                let now = start.elapsed().as_millis() as u64;
                match ev {
                    SttEvent::Partial(seg) => {
                        partial_lat.push(now.saturating_sub(seg.t_end_ms));
                    }
                    SttEvent::Final(seg) => {
                        outstanding -= 1;
                        finals_count += 1;
                        final_lat.push(now.saturating_sub(seg.t_end_ms));
                    }
                    SttEvent::Error(e) => eprintln!("  stt notice: {e}"),
                }
            }
        }

        // Tail: keep polling with accurate arrival times until the session
        // has been quiet for a while (in-flight inference completing), then
        // finish and drain whatever only flushes on close (Deepgram's
        // CloseStream results) with drain-time stamps — a slight
        // overstatement for those few events, noted in RESULTS.md.
        let mut quiet = Instant::now();
        while quiet.elapsed() < Duration::from_secs(4) {
            match tokio::time::timeout(Duration::from_millis(100), session.next_event()).await {
                Ok(Some(ev)) => {
                    let now = start.elapsed().as_millis() as u64;
                    match ev {
                        SttEvent::Partial(seg) => {
                            partial_lat.push(now.saturating_sub(seg.t_end_ms));
                        }
                        SttEvent::Final(seg) => {
                            finals_count += 1;
                            final_lat.push(now.saturating_sub(seg.t_end_ms));
                        }
                        SttEvent::Error(e) => eprintln!("  stt notice: {e}"),
                    }
                    quiet = Instant::now();
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        session.finish().await.expect("finish");
        let now = start.elapsed().as_millis() as u64;
        while let Some(ev) = session.next_event().await {
            match ev {
                SttEvent::Partial(seg) => partial_lat.push(now.saturating_sub(seg.t_end_ms)),
                SttEvent::Final(seg) => {
                    finals_count += 1;
                    final_lat.push(now.saturating_sub(seg.t_end_ms));
                }
                SttEvent::Error(e) => eprintln!("  stt notice: {e}"),
            }
        }
    }

    partial_lat.sort_unstable();
    final_lat.sort_unstable();
    println!(
        "| {} | {} | {} | {} | {} | {} | {} |",
        args.stt,
        if partial_lat.is_empty() {
            "—".to_string()
        } else {
            format!("{}", percentile(&partial_lat, 0.5))
        },
        if partial_lat.is_empty() {
            "—".to_string()
        } else {
            format!("{}", percentile(&partial_lat, 0.95))
        },
        percentile(&final_lat, 0.5),
        percentile(&final_lat, 0.95),
        partial_lat.len(),
        finals_count,
    );
}
