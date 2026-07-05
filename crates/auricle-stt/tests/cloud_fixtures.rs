//! Fixture-driven cloud provider tests — no network. Every payload here was
//! captured verbatim from the real APIs (see docs/PHASE3_PAYLOAD_REPORT.md).

#![cfg(feature = "cloud")]

use auricle_core::{AudioChunk, ChannelId, ChunkKind, SttEvent};
use auricle_stt::{OpenAiBatchProvider, SessionCfg, SttProvider};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn fixture_dir(sub: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(sub)
}

// ---------- Deepgram frames ----------

#[test]
fn every_captured_deepgram_frame_parses() {
    let dir = fixture_dir("deepgram");
    let mut frames = 0;
    let mut results = 0;
    let mut metadata = 0;
    let mut finals_with_text = Vec::new();

    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixtures/deepgram exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert!(paths.len() >= 18, "expected the full captured session");

    for p in paths {
        let text = std::fs::read_to_string(&p).unwrap();
        let v: Value = serde_json::from_str(&text).expect("valid JSON");
        frames += 1;
        match v["type"].as_str() {
            Some("Results") => {
                results += 1;
                // The provider's own types must accept every real frame.
                assert!(v["start"].is_number() && v["duration"].is_number());
                assert!(v["is_final"].is_boolean());
                let transcript = v["channel"]["alternatives"][0]["transcript"]
                    .as_str()
                    .expect("transcript field");
                if v["is_final"].as_bool().unwrap() && !transcript.is_empty() {
                    finals_with_text.push(transcript.to_string());
                }
            }
            Some("Metadata") => metadata += 1,
            other => panic!("unexpected frame type {other:?} in {}", p.display()),
        }
    }

    assert_eq!(frames, results + metadata);
    assert_eq!(metadata, 1, "exactly one Metadata frame per session");
    assert_eq!(
        finals_with_text.len(),
        4,
        "four spoken sentences -> four non-empty finals"
    );
    assert!(finals_with_text[0].contains("Quick brown fox"));
    // Real capture includes empty-transcript Results (silence padding).
    assert!(results > finals_with_text.len());
}

// ---------- Groq / OpenAI-compatible batch ----------

#[test]
fn groq_default_fixture_parses() {
    let text = std::fs::read_to_string(fixture_dir("groq").join("transcription.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let transcript = v["text"].as_str().unwrap();
    assert!(transcript.contains("streaming pipeline"));
    // Doc-vs-reality: Groq attaches an undocumented x_groq extension.
    assert!(v["x_groq"]["id"].as_str().unwrap().starts_with("req_"));
}

#[test]
fn groq_verbose_fixture_parses() {
    let text =
        std::fs::read_to_string(fixture_dir("groq").join("transcription_verbose.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    // Doc-vs-reality: language is the full word, not an ISO code.
    assert_eq!(v["language"].as_str().unwrap(), "English");
    assert_eq!(v["segments"].as_array().unwrap().len(), 4);
    assert!(v["duration"].as_f64().unwrap() > 14.0);
}

// ---------- retry / backoff via wiremock ----------

fn test_chunk() -> AudioChunk {
    AudioChunk {
        channel: ChannelId::Loopback,
        session_id: "t".to_string(),
        seq: 0,
        kind: ChunkKind::Final,
        sample_rate_hz: 16_000,
        t_start_ms: 1000,
        t_end_ms: 2000,
        samples: vec![0.1; 16_000],
    }
}

/// Responds 429, then 500, then the captured Groq fixture body.
struct FlakyThenFixture {
    counter: std::sync::atomic::AtomicU32,
}

impl Respond for FlakyThenFixture {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match n {
            0 => ResponseTemplate::new(429),
            1 => ResponseTemplate::new(500),
            _ => {
                let body = std::fs::read_to_string(fixture_dir("groq").join("transcription.json"))
                    .unwrap();
                ResponseTemplate::new(200).set_body_raw(body, "application/json")
            }
        }
    }
}

#[tokio::test]
async fn batch_retries_on_429_and_5xx_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(FlakyThenFixture {
            counter: std::sync::atomic::AtomicU32::new(0),
        })
        .expect(3) // exactly 3 attempts: 429 -> 500 -> 200
        .mount(&server)
        .await;

    let provider = OpenAiBatchProvider::with_key(
        "groq-whisper",
        "test-key".to_string(),
        &server.uri(),
        "whisper-large-v3-turbo",
    );
    let mut session = provider
        .start_session(&SessionCfg {
            session_id: "t".to_string(),
            language: "en".to_string(),
        })
        .await
        .unwrap();

    session.feed(test_chunk()).await.unwrap();
    let ev = session.next_event().await.expect("an event");
    match ev {
        SttEvent::Final(seg) => {
            assert!(seg.text.contains("streaming pipeline"));
            assert_eq!(seg.t_start_ms, 1000, "chunk timestamps preserved");
            assert_eq!(seg.t_end_ms, 2000);
            assert_eq!(seg.channel, ChannelId::Loopback);
        }
        other => panic!("expected final, got {other:?}"),
    }
    let finals = session.finish().await.unwrap();
    assert_eq!(finals.len(), 1);
    // Mock::expect(3) verifies the attempt count on drop.
}

#[tokio::test]
async fn batch_does_not_retry_client_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1) // exactly one attempt: 400 is not retryable
        .mount(&server)
        .await;

    let provider = OpenAiBatchProvider::with_key(
        "openai-compat",
        "test-key".to_string(),
        &server.uri(),
        "whisper-1",
    );
    let mut session = provider
        .start_session(&SessionCfg {
            session_id: "t".to_string(),
            language: "en".to_string(),
        })
        .await
        .unwrap();

    session.feed(test_chunk()).await.unwrap();
    match session.next_event().await {
        Some(SttEvent::Error(e)) => {
            assert!(e.contains("400"), "{e}");
            assert!(e.contains("not retryable"), "{e}");
        }
        other => panic!("expected error event, got {other:?}"),
    }
    let finals = session.finish().await.unwrap();
    assert!(finals.is_empty());
}

#[tokio::test]
async fn rate_limited_final_waits_out_retry_after_and_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(FlakyThenFixture429 {
            counter: std::sync::atomic::AtomicU32::new(0),
        })
        .expect(2) // 429 then success — the old fixed ladder burned attempts
        .mount(&server)
        .await;

    let provider = OpenAiBatchProvider::with_key(
        "groq-whisper",
        "test-key".to_string(),
        &server.uri(),
        "whisper-large-v3-turbo",
    );
    let mut session = provider
        .start_session(&SessionCfg {
            session_id: "t".to_string(),
            language: "en".to_string(),
        })
        .await
        .unwrap();

    let started = std::time::Instant::now();
    session.feed(test_chunk()).await.unwrap();
    match session.next_event().await {
        Some(SttEvent::Final(seg)) => assert!(seg.text.contains("streaming pipeline")),
        other => panic!("expected final, got {other:?}"),
    }
    // The retry honored the server's 1 s retry-after (fixed backoff for
    // attempt 1 is only ~0.5 s).
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(1),
        "waited only {:?}",
        started.elapsed()
    );
    session.finish().await.unwrap();
}

/// 429 with retry-after: 1, then the captured fixture.
struct FlakyThenFixture429 {
    counter: std::sync::atomic::AtomicU32,
}

impl Respond for FlakyThenFixture429 {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            ResponseTemplate::new(429).insert_header("retry-after", "1")
        } else {
            let body =
                std::fs::read_to_string(fixture_dir("groq").join("transcription.json")).unwrap();
            ResponseTemplate::new(200).set_body_raw(body, "application/json")
        }
    }
}

#[tokio::test]
async fn interims_are_shed_during_rate_limit_cooldown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(FlakyThenFixture429 {
            counter: std::sync::atomic::AtomicU32::new(0),
        })
        .mount(&server)
        .await;

    let provider = OpenAiBatchProvider::with_key(
        "groq-whisper",
        "test-key".to_string(),
        &server.uri(),
        "whisper-large-v3-turbo",
    );
    let mut session = provider
        .start_session(&SessionCfg {
            session_id: "t".to_string(),
            language: "en".to_string(),
        })
        .await
        .unwrap();

    // Final #1 hits the 429 (entering cooldown), retries, succeeds: 2 reqs.
    session.feed(test_chunk()).await.unwrap();
    assert!(matches!(
        session.next_event().await,
        Some(SttEvent::Final(_))
    ));

    // An interim during cooldown must be shed without an upload.
    let mut interim = test_chunk();
    interim.kind = ChunkKind::Interim;
    interim.seq = 1;
    session.feed(interim).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A final during cooldown still uploads: 3rd request.
    let mut final2 = test_chunk();
    final2.seq = 2;
    final2.t_start_ms = 3000;
    final2.t_end_ms = 4000;
    session.feed(final2).await.unwrap();
    assert!(matches!(
        session.next_event().await,
        Some(SttEvent::Final(_))
    ));
    session.finish().await.unwrap();

    let calls = server.received_requests().await.unwrap();
    assert_eq!(
        calls.len(),
        3,
        "2 for the rate-limited final + 1 for the second final; the interim \
         must not have been uploaded"
    );
}

#[tokio::test]
async fn batch_gives_up_after_max_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(4) // MAX_ATTEMPTS
        .mount(&server)
        .await;

    let provider = OpenAiBatchProvider::with_key(
        "groq-whisper",
        "test-key".to_string(),
        &server.uri(),
        "whisper-large-v3-turbo",
    );
    let mut session = provider
        .start_session(&SessionCfg {
            session_id: "t".to_string(),
            language: "en".to_string(),
        })
        .await
        .unwrap();

    session.feed(test_chunk()).await.unwrap();
    match session.next_event().await {
        Some(SttEvent::Error(e)) => {
            assert!(e.contains("giving up after 4 attempts"), "{e}");
        }
        other => panic!("expected error event, got {other:?}"),
    }
    session.finish().await.unwrap();
}
