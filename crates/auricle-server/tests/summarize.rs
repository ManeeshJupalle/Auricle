//! Summarize endpoint against a wiremock LLM: the "ollama" provider's
//! base_url is pointed at the mock, so the whole HTTP → store → LLM →
//! persistence → export path runs without a real model.

use std::sync::Arc;

use auricle_core::{ChannelId, Config};
use auricle_server::{build_router, Engine, EngineOptions};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("auricle-sum-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn_server(cfg: Config, name: &str) -> (String, Arc<Engine>) {
    let engine = Engine::new(EngineOptions {
        cfg,
        data_root: temp_root(name),
        provider_override: None,
    })
    .unwrap();
    let app = build_router(engine.clone(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), engine)
}

fn seed_session(engine: &Engine, id: &str) {
    let store = engine.store();
    store
        .create_session(
            id,
            "Planning sync",
            1_700_000_000,
            "deepgram",
            &serde_json::json!({}),
        )
        .unwrap();
    store
        .insert_segment(
            id,
            ChannelId::Loopback,
            "Them",
            0,
            4000,
            "Can we ship Friday?",
            "deepgram",
        )
        .unwrap();
    store
        .insert_segment(
            id,
            ChannelId::Mic,
            "You",
            4500,
            8000,
            "Yes, latency is fine.",
            "deepgram",
        )
        .unwrap();
    store.end_session(id, 1_700_000_060).unwrap();
}

#[tokio::test]
async fn summarize_persists_and_exports() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-x", "object": "chat.completion", "model": "mock-model",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant",
                            "content": "- Decision: ship on Friday (latency acceptable)."}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
        })))
        .expect(1)
        .mount(&llm)
        .await;

    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri();
    cfg.llm.ollama.model = "mock-model".to_string();
    let (base, engine) = spawn_server(cfg, "ok").await;
    seed_session(&engine, "sess1");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/sessions/sess1/summarize"))
        .json(&serde_json::json!({"template": "minutes", "provider": "ollama"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["template"], "minutes");
    assert_eq!(body["provider"], "ollama");
    assert_eq!(body["model"], "mock-model");
    assert!(body["content"].as_str().unwrap().contains("Friday"));

    // The LLM saw the speaker-labeled transcript under the template prompt.
    let reqs = llm.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let user = sent["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("[00:00] Them: Can we ship Friday?"), "{user}");
    assert!(
        user.contains("[00:04] You: Yes, latency is fine."),
        "{user}"
    );

    // Persisted: session detail carries it...
    let detail: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions/sess1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["summaries"].as_array().unwrap().len(), 1);
    assert_eq!(detail["summaries"][0]["template"], "minutes");

    // ...and the markdown export includes it after the transcript.
    let md = client
        .get(format!("{base}/api/v1/sessions/sess1/export?format=md"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(md.contains("## Summary — minutes (mock-model)"), "{md}");
    assert!(md.find("## Summary").unwrap() > md.find("## Transcript").unwrap());
}

#[tokio::test]
async fn ollama_model_setting_overrides_config() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "c", "object": "chat.completion", "model": "qwen3:8b",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&llm)
        .await;

    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri(); // config still says model llama3.1
    let (base, engine) = spawn_server(cfg, "override").await;
    seed_session(&engine, "sess1");
    engine
        .store()
        .set_setting("ollama_model", &serde_json::json!("qwen3:8b"))
        .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/sessions/sess1/summarize"))
        .json(&serde_json::json!({"provider": "ollama"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["model"], "qwen3:8b");

    // The request actually carried the overridden model.
    let reqs = llm.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(sent["model"], "qwen3:8b");
}

#[tokio::test]
async fn summarize_error_paths() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("model exploded"))
        .mount(&llm)
        .await;

    let mut cfg = Config::default();
    cfg.llm.ollama.base_url = llm.uri();
    let (base, engine) = spawn_server(cfg, "err").await;
    seed_session(&engine, "sess1");
    engine
        .store()
        .create_session("empty", "no speech", 1, "deepgram", &serde_json::json!({}))
        .unwrap();

    let client = reqwest::Client::new();
    // Unknown session -> 404.
    let r = client
        .post(format!("{base}/api/v1/sessions/nope/summarize"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    // No transcript -> 400.
    let r = client
        .post(format!("{base}/api/v1/sessions/empty/summarize"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    // Unknown template -> 400 naming the options.
    let r = client
        .post(format!("{base}/api/v1/sessions/sess1/summarize"))
        .json(&serde_json::json!({"template": "haiku"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("minutes"));
    // Unknown provider -> 400.
    let r = client
        .post(format!("{base}/api/v1/sessions/sess1/summarize"))
        .json(&serde_json::json!({"provider": "skynet"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    // LLM upstream failure -> 502 with the upstream detail.
    let r = client
        .post(format!("{base}/api/v1/sessions/sess1/summarize"))
        .json(&serde_json::json!({"provider": "ollama"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    assert!(r.text().await.unwrap().contains("model exploded"));
    // Nothing was persisted by the failures.
    assert!(engine.store().get_summaries("sess1").unwrap().is_empty());
}
