//! Real-whisper end-to-end test: fixture WAV -> timeline -> Silero VAD ->
//! chunker -> whisper-local -> assembler.
//!
//! Ignored by default: needs `ggml-base.en.bin` in `%LOCALAPPDATA%\auricle\
//! models` (run `auricle record --model base.en` once, or let ensure_model
//! download it) and ~10 s of CPU. Run with:
//!
//! ```text
//! cargo test -p auricle-stt --test whisper_e2e -- --ignored --nocapture
//! ```

use auricle_core::{ChannelId, ChunkKind, SttEvent};
use auricle_pipeline::{Assembler, ChannelPipeline, PipelineConfig, SileroDetector};
use auricle_stt::{available_models, SessionCfg, SttProvider, WhisperLocalProvider};

#[tokio::test]
#[ignore = "needs ggml-base.en.bin in %LOCALAPPDATA%\\auricle\\models and ~10 s of CPU"]
async fn fixture_speech_transcribes_end_to_end() {
    let model = available_models()
        .iter()
        .find(|m| m.name == "base.en")
        .unwrap();
    let model_path = auricle_stt::default_data_dir()
        .expect("data dir")
        .join(model.file_name);
    assert!(
        model_path.exists(),
        "model not present: {} — download it first",
        model_path.display()
    );

    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/tts_loopback_16k.wav"
    );
    let mut reader = hound::WavReader::open(fixture).expect("fixture present");
    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();

    let provider = tokio::task::spawn_blocking(move || WhisperLocalProvider::load(&model_path))
        .await
        .unwrap()
        .expect("model loads");
    let mut session = provider
        .start_session(&SessionCfg {
            session_id: "e2e".to_string(),
            language: "en".to_string(),
        })
        .await
        .expect("session starts");

    // Full pipeline on the loopback channel.
    let cfg = PipelineConfig::default();
    let mut pipeline = ChannelPipeline::new(
        "e2e",
        ChannelId::Loopback,
        SileroDetector::new().unwrap(),
        &cfg,
    );
    let mut chunks = Vec::new();
    for (i, batch) in samples.chunks(160).enumerate() {
        chunks.extend(pipeline.push_batch(batch, (i as u64 + 1) * 10));
    }
    chunks.extend(pipeline.flush());

    // Feed only the finals (the transcript path). This test bursts the whole
    // fixture in one go — far faster than real time — so interim chunks,
    // which exist for live partial display and are sheddable by design,
    // would only fight the bounded queue here.
    let finals_fed: Vec<_> = chunks
        .into_iter()
        .filter(|c| c.kind == ChunkKind::Final)
        .collect();
    assert!(!finals_fed.is_empty());

    for chunk in finals_fed {
        session.feed(chunk).await.expect("feed");
    }
    let finals = session.finish().await.expect("finish");

    let mut assembler = Assembler::new("e2e");
    while let Some(ev) = session.next_event().await {
        if let SttEvent::Error(e) = &ev {
            println!("stt notice: {e}");
        }
        assembler.ingest(ev);
    }

    let transcript = assembler.transcript();
    for t in transcript {
        println!(
            "  final {}..{} [{}] {:?}",
            t.t_start_ms, t.t_end_ms, t.speaker, t.text
        );
    }
    assert!(!finals.is_empty(), "whisper produced final segments");
    assert!(!transcript.is_empty(), "assembled transcript is non-empty");
    assert!(
        transcript
            .windows(2)
            .all(|w| w[0].t_start_ms <= w[1].t_start_ms),
        "ordered by start time"
    );
    assert!(transcript.iter().all(|t| t.speaker == "Them"));

    // The TTS fixture says: "The quick brown fox jumps over the lazy dog.
    // Auricle is a local first meeting transcription engine. ..."
    let all_text = transcript
        .iter()
        .map(|t| t.text.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    println!("transcript: {all_text}");
    assert!(
        all_text.contains("fox") || all_text.contains("transcription"),
        "recognizable words from the fixture, got: {all_text}"
    );
}
