//! Map-reduce against a mock OpenAI-compatible server: verifies the code
//! path selection (single call vs map+reduce) and the call counts.

use auricle_llm::{summarize, OpenAiChatProvider};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn chat_body(content: &str) -> serde_json::Value {
    // Shape mirrors fixtures/llm/groq_chat.json.
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

#[tokio::test]
async fn short_transcript_is_a_single_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("the summary")))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenAiChatProvider::new("test", &server.uri(), "test-model", None);
    let out = summarize(&provider, "summarize this", "short transcript")
        .await
        .unwrap();
    assert_eq!(out, "the summary");
}

#[tokio::test]
async fn long_transcript_maps_then_reduces() {
    let server = MockServer::start().await;
    // Distinguish map calls (mention "Portion") from the reduce call.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let user = body["messages"][1]["content"].as_str().unwrap_or_default();
            let system = body["messages"][0]["content"].as_str().unwrap_or_default();
            if system.contains("THIS PORTION ONLY") {
                let tag = user.split(':').next().unwrap_or("portion");
                ResponseTemplate::new(200).set_body_json(chat_body(&format!("summary of {tag}")))
            } else {
                assert!(
                    user.contains("--- portion 1"),
                    "reduce input carries the map outputs"
                );
                ResponseTemplate::new(200).set_body_json(chat_body("merged summary"))
            }
        })
        .mount(&server)
        .await;

    // ~40k chars => ~10k estimated tokens => over the 6k threshold; with
    // 16k-char chunks this yields 3 map calls + 1 reduce.
    let line = format!("[00:00] Them: {}", "word ".repeat(75));
    let transcript = vec![line; 100].join("\n");

    let provider = OpenAiChatProvider::new("test", &server.uri(), "test-model", None);
    let out = summarize(&provider, "make minutes", &transcript)
        .await
        .unwrap();
    assert_eq!(out, "merged summary");

    let calls = server.received_requests().await.unwrap();
    assert_eq!(calls.len(), 4, "3 map + 1 reduce");
}

#[tokio::test]
async fn http_error_surfaces_with_provider_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_string("model not found"))
        .mount(&server)
        .await;

    let provider = OpenAiChatProvider::new("ollama", &server.uri(), "missing-model", None);
    let err = summarize(&provider, "sys", "short").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ollama"), "{msg}");
    assert!(msg.contains("404"), "{msg}");
    assert!(msg.contains("model not found"), "{msg}");
}
