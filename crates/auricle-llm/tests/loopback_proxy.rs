//! A proxy set for the whole machine must not catch a request to this one.
//! reqwest reads `HTTP_PROXY` and the Windows proxy setting, and exempts only
//! the hosts those name, so without a bypass a local Ollama goes through the
//! proxy. Its own test binary: the variable is process-wide.

use auricle_core::Config;
use auricle_llm::{ollama_status, LlmProvider, OpenAiChatProvider};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A proxy nothing listens on, so anything sent through it fails.
fn dead_proxy() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy = format!("http://127.0.0.1:{port}");
    std::env::set_var("HTTP_PROXY", &proxy);
    std::env::set_var("http_proxy", &proxy);
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");
}

#[tokio::test]
async fn a_local_ollama_is_reached_behind_a_proxy() {
    dead_proxy();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{"id": "llama3.1:latest", "object": "model", "owned_by": "library"}]
        })))
        .mount(&server)
        .await;
    let body = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/llm/ollama_chat.json"),
    )
    .expect("fixture present");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .mount(&server)
        .await;

    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = server.uri();
    cfg.llm.ollama.model = "llama3.1".to_string();
    let status = ollama_status(&cfg).await;
    assert!(status.ready, "{}", status.detail);

    let provider = OpenAiChatProvider::new("ollama", &server.uri(), "llama3.1", None);
    let answer = provider.complete("sys", "user").await.unwrap();
    assert!(answer.contains("Friday"), "{answer}");
}
