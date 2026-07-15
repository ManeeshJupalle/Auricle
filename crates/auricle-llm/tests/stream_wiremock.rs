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
    assert!(err.contains("404"), "{err}");
    assert!(err.contains("not found"), "{err}");
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
