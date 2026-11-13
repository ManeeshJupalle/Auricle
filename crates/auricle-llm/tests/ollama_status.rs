//! Ollama has no key to look for, so readiness means asking it. These run the
//! check, and the errors a summary or an ask would give, against a wiremock
//! endpoint and against a port nothing listens on.

use auricle_core::Config;
use auricle_llm::{ollama_status, LlmProvider, OpenAiChatProvider};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config_for(base_url: &str, model: &str) -> Config {
    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = base_url.to_string();
    cfg.llm.ollama.model = model.to_string();
    cfg
}

async fn listing(models: &[&str]) -> MockServer {
    let data: Vec<_> = models
        .iter()
        .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "library"}))
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object": "list", "data": data})),
        )
        .mount(&server)
        .await;
    server
}

/// A local URL nothing is listening on.
fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

#[tokio::test]
async fn ready_when_the_model_is_pulled() {
    let server = listing(&["qwen3:8b", "llama3.1:latest"]).await;
    let status = ollama_status(&config_for(&server.uri(), "llama3.1")).await;
    assert!(status.ready, "{}", status.detail);
    assert_eq!(status.model, "llama3.1");
}

#[tokio::test]
async fn not_ready_when_the_model_was_never_pulled() {
    let server = listing(&["qwen3:8b"]).await;
    let status = ollama_status(&config_for(&server.uri(), "llama3.1")).await;
    assert!(!status.ready);
    assert!(
        status.detail.contains("ollama pull llama3.1"),
        "{}",
        status.detail
    );
}

#[tokio::test]
async fn not_ready_when_nothing_answers() {
    let url = closed_port();
    let status = ollama_status(&config_for(&url, "llama3.1")).await;
    assert!(!status.ready);
    assert!(status.detail.contains("ollama.com"), "{}", status.detail);
    assert!(status.detail.contains(&url), "{}", status.detail);
}

#[tokio::test]
async fn an_answer_that_is_not_a_model_list_is_not_held_against_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>hello</html>"))
        .mount(&server)
        .await;
    let status = ollama_status(&config_for(&server.uri(), "llama3.1")).await;
    assert!(status.ready, "{}", status.detail);
}

#[tokio::test]
async fn a_summary_says_what_to_install_when_nothing_answers() {
    let url = closed_port();
    let provider = OpenAiChatProvider::new("ollama", &url, "llama3.1", None);
    let err = provider
        .complete("sys", "user")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("ollama.com"), "{err}");
    assert!(err.contains("ollama pull llama3.1"), "{err}");
}

#[tokio::test]
async fn a_summary_says_which_model_to_pull_when_it_is_missing() {
    let body = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/llm-stream/ollama_badmodel_error.txt"),
    )
    .expect("fixture present");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(body, "application/json"))
        .mount(&server)
        .await;
    let provider = OpenAiChatProvider::new("ollama", &server.uri(), "llama3.1", None);
    let err = provider
        .complete("sys", "user")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("ollama pull llama3.1"), "{err}");
    assert!(!err.contains("HTTP 404"), "{err}");
}
