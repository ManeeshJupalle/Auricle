//! Map-reduce against a mock OpenAI-compatible server: verifies the code
//! path selection (single call vs map+reduce) and the call counts.

use std::sync::atomic::{AtomicU32, Ordering};

use auricle_llm::{estimate_tokens, summarize, OpenAiChatProvider, MAP_REDUCE_TOKEN_THRESHOLD};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

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

/// ~40k chars: 3 map calls, as in `long_transcript_maps_then_reduces`.
fn long_transcript() -> String {
    let line = format!("[00:00] Them: {}", "word ".repeat(75));
    vec![line; 100].join("\n")
}

/// 429 with `retry-after: 1` on the first request only.
struct RateLimitedOnce(AtomicU32);

impl Respond for RateLimitedOnce {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("slow down")
        } else {
            ResponseTemplate::new(200).set_body_json(chat_body("ok"))
        }
    }
}

#[tokio::test]
async fn rate_limited_portion_waits_and_the_summary_survives() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(RateLimitedOnce(AtomicU32::new(0)))
        .expect(5) // 3 map + 1 reduce + the one retry
        .mount(&server)
        .await;

    let provider = OpenAiChatProvider::new("groq", &server.uri(), "test-model", None);
    let started = std::time::Instant::now();
    let out = summarize(&provider, "make minutes", &long_transcript())
        .await
        .unwrap();

    assert_eq!(out, "ok");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(1),
        "honored retry-after, waited only {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn persistent_rate_limit_gives_up_with_the_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string("quota exhausted"),
        )
        .expect(4) // MAX_ATTEMPTS
        .mount(&server)
        .await;

    let provider = OpenAiChatProvider::new("groq", &server.uri(), "test-model", None);
    let err = summarize(&provider, "sys", "short").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("429"), "{msg}");
    assert!(msg.contains("quota exhausted"), "{msg}");
}

#[tokio::test]
async fn reduce_input_stays_bounded_when_partials_are_large() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let system = body["messages"][0]["content"].as_str().unwrap_or_default();
            // Map output is almost as long as its input, as a chatty local
            // model's can be: the partials alone blow the reduce budget.
            let reply = if system.contains("THIS PORTION ONLY") {
                "p".repeat(11_000)
            } else {
                "merged".to_string()
            };
            ResponseTemplate::new(200).set_body_json(chat_body(&reply))
        })
        .mount(&server)
        .await;

    // ~156k chars => 10 map portions => ~27k tokens of partials.
    let line = format!("[00:00] Them: {}", "word ".repeat(75));
    let transcript = vec![line; 400].join("\n");

    let provider = OpenAiChatProvider::new("test", &server.uri(), "test-model", None);
    let out = summarize(&provider, "make minutes", &transcript)
        .await
        .unwrap();
    assert_eq!(out, "merged");

    let calls = server.received_requests().await.unwrap();
    for call in &calls {
        let body: serde_json::Value = serde_json::from_slice(&call.body).unwrap();
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(
            estimate_tokens(user) <= MAP_REDUCE_TOKEN_THRESHOLD,
            "a {}-token request went out",
            estimate_tokens(user)
        );
    }
}

#[tokio::test]
async fn truncated_completion_keeps_the_text_and_says_so() {
    let server = MockServer::start().await;
    let mut body = chat_body("the summary so f");
    body["choices"][0]["finish_reason"] = "length".into();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let provider = OpenAiChatProvider::new("test", &server.uri(), "test-model", None);
    let out = summarize(&provider, "sys", "short").await.unwrap();

    assert!(out.starts_with("the summary so f"), "{out}");
    assert!(out.contains("Truncated"), "{out}");
}

#[tokio::test]
async fn truncated_portion_is_flagged_even_if_the_reduce_drops_the_notice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let system = body["messages"][0]["content"].as_str().unwrap_or_default();
            if system.contains("THIS PORTION ONLY") {
                let mut cut_off = chat_body("portion summary");
                cut_off["choices"][0]["finish_reason"] = "length".into();
                ResponseTemplate::new(200).set_body_json(cut_off)
            } else {
                ResponseTemplate::new(200).set_body_json(chat_body("clean merged summary"))
            }
        })
        .mount(&server)
        .await;

    let provider = OpenAiChatProvider::new("test", &server.uri(), "test-model", None);
    let out = summarize(&provider, "make minutes", &long_transcript())
        .await
        .unwrap();

    assert!(out.starts_with("clean merged summary"), "{out}");
    assert!(out.contains("Truncated"), "{out}");
}

#[tokio::test]
async fn http_error_surfaces_with_provider_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_string("model not found"))
        .mount(&server)
        .await;

    // Not Ollama: its missing-model 404 is put in words (tests/ollama_status.rs).
    let provider = OpenAiChatProvider::new("groq", &server.uri(), "missing-model", None);
    let err = summarize(&provider, "sys", "short").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("groq"), "{msg}");
    assert!(msg.contains("404"), "{msg}");
    assert!(msg.contains("model not found"), "{msg}");
}
