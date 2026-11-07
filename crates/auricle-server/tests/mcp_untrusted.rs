//! What the MCP server tells an agent about the text it hands back. "Them"
//! is whoever is on the call, so a transcript can carry words written to
//! steer the agent reading it: that warning has to reach the agent both up
//! front and on the tools that return such text.

use auricle_core::Config;
use auricle_server::{build_router, Engine, EngineOptions};

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "auricle-mcp-untrusted-it-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn_enabled(name: &str) -> String {
    let engine = Engine::new(EngineOptions {
        cfg: Config::default(),
        data_root: temp_root(name),
        provider_override: None,
    })
    .unwrap();
    engine
        .store()
        .set_setting("mcp_enabled", &serde_json::json!(true))
        .unwrap();
    let app = build_router(engine, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn rpc_payload(body: &str) -> serde_json::Value {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        return v;
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        .find(|v| v.get("jsonrpc").is_some())
        .unwrap_or_else(|| panic!("no JSON-RPC payload in: {body}"))
}

fn post(client: &reqwest::Client, base: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
}

#[tokio::test]
async fn server_and_tools_warn_that_transcript_text_is_untrusted() {
    let base = spawn_enabled("warning").await;
    let client = reqwest::Client::new();

    let resp = post(&client, &base)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "integration-test", "version": "0"}
            }
        }))
        .send()
        .await
        .unwrap();
    let session = resp.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let init = rpc_payload(&resp.text().await.unwrap());
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("untrusted"), "{instructions}");
    assert!(instructions.contains("instructions"), "{instructions}");

    post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .send()
        .await
        .unwrap();
    let resp = post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    let list = rpc_payload(&resp.text().await.unwrap());
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    for tool in tools {
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("untrusted"), "{tool}");
    }
}
