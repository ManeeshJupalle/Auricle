//! Integration test: a Phase-1-captured WAV (Windows TTS speech recorded via
//! WASAPI loopback) through timeline -> real Silero VAD -> chunker ->
//! assembler, asserting non-empty, ordered, speaker-tagged finals.
//!
//! STT is simulated deterministically per chunk (real whisper end-to-end is
//! the `#[ignore]`d test in auricle-stt, which needs the model file).

use auricle_core::{ChannelId, ChunkKind, Segment, SttEvent};
use auricle_pipeline::{Assembler, ChannelPipeline, PipelineConfig, PipelineEvent, SileroDetector};

fn load_fixture() -> Vec<f32> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/tts_loopback_16k.wav"
    );
    let mut reader = hound::WavReader::open(path).expect("fixture wav present");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16_000);
    assert_eq!(spec.channels, 1);
    reader
        .samples::<f32>()
        .map(|s| s.expect("valid sample"))
        .collect()
}

#[test]
fn fixture_speech_produces_ordered_tagged_finals() {
    let samples = load_fixture();
    assert!(
        samples.len() > 10 * 16_000,
        "fixture should be >10 s of audio"
    );

    let cfg = PipelineConfig::default();
    let mut pipeline = ChannelPipeline::new(
        "fixture-session",
        ChannelId::Loopback,
        SileroDetector::new().expect("silero loads"),
        &cfg,
    );

    // Feed as 10 ms batches with ideal wall timing, like the drain loop does.
    let mut chunks = Vec::new();
    for (i, batch) in samples.chunks(160).enumerate() {
        let wall_ms = (i as u64 + 1) * 10;
        chunks.extend(pipeline.push_batch(batch, wall_ms));
    }
    chunks.extend(pipeline.flush());

    let finals: Vec<_> = chunks
        .iter()
        .filter(|c| c.kind == ChunkKind::Final)
        .collect();
    let interims: Vec<_> = chunks
        .iter()
        .filter(|c| c.kind == ChunkKind::Interim)
        .collect();
    assert!(
        !finals.is_empty(),
        "real VAD must find speech in the TTS fixture"
    );
    assert!(
        !interims.is_empty(),
        "spans of continuous TTS speech must produce interim (partial) chunks"
    );

    // The fixture is ~15 s of near-continuous speech; VAD should attribute
    // most of it to spans.
    let speech_ms: u64 = finals.iter().map(|c| c.t_end_ms - c.t_start_ms).sum();
    assert!(
        speech_ms > 8_000,
        "expected >8 s of speech chunks, got {speech_ms} ms"
    );

    // Chunk invariants: bounded size, timestamps consistent with samples.
    for c in &finals {
        assert!(c.duration_ms() <= cfg.max_chunk_ms + 50);
        assert_eq!(c.duration_ms(), c.t_end_ms - c.t_start_ms);
        assert_eq!(c.session_id, "fixture-session");
    }

    // Simulate STT per final chunk and assemble the transcript.
    let mut assembler = Assembler::new("fixture-session");
    let mut broadcast_finals = 0;
    for (i, c) in finals.iter().enumerate() {
        let ev = assembler.ingest(SttEvent::Final(Segment {
            channel: c.channel,
            t_start_ms: c.t_start_ms,
            t_end_ms: c.t_end_ms,
            text: format!("segment {i} text"),
        }));
        if let Some(PipelineEvent::Transcript(t)) = ev {
            assert!(t.is_final);
            broadcast_finals += 1;
        }
    }
    assert_eq!(broadcast_finals, finals.len());

    let transcript = assembler.transcript();
    assert!(!transcript.is_empty(), "non-empty transcript");
    assert!(
        transcript
            .windows(2)
            .all(|w| w[0].t_start_ms <= w[1].t_start_ms),
        "transcript ordered by t_start"
    );
    assert!(
        transcript.iter().all(|t| t.speaker == "Them"),
        "loopback channel is speaker-tagged Them"
    );
    assert!(transcript.iter().all(|t| !t.text.is_empty()));
}

/// The same fixture fed with a 20 s hole in the middle (wall clock advances,
/// no samples): the pipeline must not fabricate spans inside the hole, and
/// post-gap chunks must carry re-anchored timestamps.
#[test]
fn fixture_with_simulated_stream_gap_reanchors_timestamps() {
    let samples = load_fixture();
    let cfg = PipelineConfig::default();
    let mut pipeline = ChannelPipeline::new(
        "gap-session",
        ChannelId::Loopback,
        SileroDetector::new().expect("silero loads"),
        &cfg,
    );

    let half = samples.len() / 2;
    let mut chunks = Vec::new();

    // First half delivered normally.
    let mut wall_ms = 0;
    for batch in samples[..half].chunks(160) {
        wall_ms += 10;
        chunks.extend(pipeline.push_batch(batch, wall_ms));
    }
    let pre_gap_end = wall_ms;

    // 20 s of loopback silence: NO batches at all, wall clock moves on.
    wall_ms += 20_000;

    // Second half resumes.
    for batch in samples[half..].chunks(160) {
        wall_ms += 10;
        chunks.extend(pipeline.push_batch(batch, wall_ms));
    }
    chunks.extend(pipeline.flush());

    let finals: Vec<_> = chunks
        .iter()
        .filter(|c| c.kind == ChunkKind::Final)
        .collect();
    assert!(finals.len() >= 2, "speech on both sides of the gap");

    // No chunk may lie inside the silent hole.
    for c in &finals {
        let inside_hole = c.t_start_ms > pre_gap_end && c.t_end_ms < pre_gap_end + 19_000;
        assert!(
            !inside_hole,
            "phantom chunk inside the gap: {}..{}",
            c.t_start_ms, c.t_end_ms
        );
    }

    // At least one post-gap chunk with re-anchored (wall-clock) timing.
    assert!(
        finals.iter().any(|c| c.t_start_ms >= pre_gap_end + 19_000),
        "post-gap speech must carry re-anchored timestamps: {:?}",
        finals
            .iter()
            .map(|c| (c.t_start_ms, c.t_end_ms))
            .collect::<Vec<_>>()
    );

    // And no timestamp regressions across the whole sequence.
    let starts: Vec<_> = chunks.iter().map(|c| c.t_start_ms).collect();
    assert!(
        starts.windows(2).all(|w| w[0] <= w[1]),
        "chunk emission order must be time-monotonic per channel"
    );
}
