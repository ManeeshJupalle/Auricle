//! chat_stream against a wiremock endpoint replaying the captured SSE
//! fixtures byte-for-byte: the full HTTP → SSE → delta-channel path runs
//! without a live model, for both providers' shapes.

use auricle_llm::{LlmProvider, OpenAiChatProvider, TokenUsage};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture_bytes(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/llm-stream")
        .join(name);
    std::fs::read(p).expect("fixture present")
}

async fn stream_all(
    provider: &OpenAiChatProvider,
) -> (Vec<String>, auricle_core::Result<Option<TokenUsage>>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let collector = tokio::spawn(async move {
        let mut deltas = Vec::new();
        while let Some(d) = rx.recv().await {
            deltas.push(d);
        }
        deltas
    });
    let result = provider.chat_stream("system", "user", tx).await;
    (collector.await.unwrap(), result)
}

#[tokio::test]
async fn ollama_fixture_streams_deltas_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            fixture_bytes("ollama_qwen25_usage_stream.txt"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let provider = OpenAiChatProvider::new("ollama", &server.uri(), "qwen2.5:14b", None);

    let (deltas, result) = stream_all(&provider).await;
    assert_eq!(deltas.first().map(String::as_str), Some("A"));
    assert_eq!(
        deltas.concat(),
        "A meeting transcription tool converts spoken words in a meeting into written text."
    );
    let usage = result.unwrap().expect("usage frame present");
    assert_eq!(usage.total_tokens, 47);

    // The request asked for streaming with usage (Ollama sends no usage
    // frame otherwise — fixture-verified).
    let sent: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn qwen3_reasoning_stream_yields_only_the_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            fixture_bytes("ollama_qwen3_usage_stream.txt"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let provider = OpenAiChatProvider::new("ollama", &server.uri(), "qwen3:8b", None);

    let (deltas, result) = stream_all(&provider).await;
    let answer = deltas.concat();
    assert!(
        answer.starts_with("A meeting transcription tool"),
        "{answer}"
    );
    assert!(!answer.contains("Okay"), "chain of thought leaked");
    assert!(deltas.iter().all(|d| !d.is_empty()));
    assert!(result.unwrap().is_some());
}

#[tokio::test]
async fn groq_fixture_without_stream_options_still_reports_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture_bytes("groq_plain_stream.txt"), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = OpenAiChatProvider::new(
        "groq",
        &server.uri(),
        "llama-3.3-70b-versatile",
        Some("k".into()),
    );

    let (deltas, result) = stream_all(&provider).await;
    assert!(!deltas.is_empty());
    let usage = result
        .unwrap()
        .expect("Groq puts usage on the finish chunk");
    assert_eq!(usage.prompt_tokens, 54);
    assert_eq!(usage.completion_tokens, 21);
}

#[tokio::test]
async fn upstream_error_is_a_clean_error_not_a_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(
            fixture_bytes("ollama_badmodel_error.txt"),
            "application/json",
        ))
        .mount(&server)
        .await;
    let provider = OpenAiChatProvider::new("ollama", &server.uri(), "nope:doesnotexist", None);

    let (deltas, result) = stream_all(&provider).await;
    assert!(deltas.is_empty());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("ollama pull nope:doesnotexist"), "{err}");
}

/// Serve a hand-written SSE body and collect what the provider makes of it.
async fn stream_body(body: &str) -> (Vec<String>, auricle_core::Result<Option<TokenUsage>>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.to_string(), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = OpenAiChatProvider::new("ollama", &server.uri(), "m", None);
    stream_all(&provider).await
}

const DELTA: &str = r#"data: {"choices":[{"delta":{"content":"partial answer"}}]}"#;

#[tokio::test]
async fn length_finish_keeps_the_answer_and_adds_a_notice() {
    let frames = [
        DELTA,
        r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        "data: [DONE]",
    ];
    let (deltas, result) = stream_body(&(frames.join("\n\n") + "\n\n")).await;

    result.expect("a truncated answer is still an answer");
    assert_eq!(deltas[0], "partial answer");
    assert!(deltas.concat().contains("Truncated"), "{deltas:?}");
}

#[tokio::test]
async fn stream_cut_off_before_done_is_an_error() {
    let (deltas, result) = stream_body(&format!("{DELTA}\n\n")).await;

    assert_eq!(deltas, vec!["partial answer"]);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("before the answer finished"), "{err}");
}

#[tokio::test]
async fn finish_reason_without_done_counts_as_complete() {
    // Some compatible servers end the body right after the finish chunk.
    let frames = [
        DELTA,
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ];
    let (deltas, result) = stream_body(&(frames.join("\n\n") + "\n\n")).await;

    result.unwrap();
    assert_eq!(deltas, vec!["partial answer"], "no notice on a clean stop");
}

#[tokio::test]
async fn mid_stream_error_frame_is_an_error() {
    let frames = [
        DELTA,
        r#"data: {"error":{"message":"model runner crashed"}}"#,
        "data: [DONE]",
    ];
    let (_, result) = stream_body(&(frames.join("\n\n") + "\n\n")).await;

    let err = result.unwrap_err().to_string();
    assert!(err.contains("model runner crashed"), "{err}");
}

#[tokio::test]
async fn partial_usage_frame_does_not_abort_the_answer() {
    let frames = [
        DELTA,
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5}}"#,
        "data: [DONE]",
    ];
    let (deltas, result) = stream_body(&(frames.join("\n\n") + "\n\n")).await;

    assert_eq!(result.unwrap(), None, "unusable usage is simply absent");
    assert_eq!(deltas, vec!["partial answer"]);
}

#[tokio::test]
async fn rate_limited_stream_retries_before_any_answer_starts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            fixture_bytes("ollama_qwen25_usage_stream.txt"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let provider = OpenAiChatProvider::new("groq", &server.uri(), "m", None);

    let (deltas, result) = stream_all(&provider).await;

    assert!(result.unwrap().is_some());
    assert!(!deltas.is_empty());
}

#[tokio::test]
async fn default_trait_fallback_wraps_complete() {
    // A provider that never implemented chat_stream still streams: one
    // fragment carrying the whole non-streaming answer.
    struct OneShot;
    #[async_trait::async_trait]
    impl LlmProvider for OneShot {
        fn id(&self) -> &str {
            "oneshot"
        }
        fn model(&self) -> &str {
            "m"
        }
        async fn complete(&self, _s: &str, _u: &str) -> auricle_core::Result<String> {
            Ok("whole answer".to_string())
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let usage = OneShot.chat_stream("s", "u", tx).await.unwrap();
    assert_eq!(usage, None);
    assert_eq!(rx.recv().await.as_deref(), Some("whole answer"));
    assert_eq!(rx.recv().await, None);
}
