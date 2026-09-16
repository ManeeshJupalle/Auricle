//! The MCP endpoint: absent until the setting turns it on, and read-only
//! when it is on.

use std::sync::Arc;

use auricle_core::{ChannelId, Config};
use auricle_server::{build_router, Engine, EngineOptions};

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("auricle-mcp-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn(name: &str) -> (String, Arc<Engine>) {
    let engine = Engine::new(EngineOptions {
        cfg: Config::default(),
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

fn initialize_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "integration-test", "version": "0" }
        }
    })
}

/// A Streamable HTTP reply is either plain JSON or an SSE stream carrying the
/// same JSON on a `data:` line. Accept both, and skip the priming events the
/// stream opens with, so the test asserts on protocol content rather than on
/// the transport's framing choices.
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

async fn enable_mcp(base: &str, client: &reqwest::Client) {
    client
        .put(format!("{base}/api/v1/settings"))
        .json(&serde_json::json!({ "mcp_enabled": true }))
        .send()
        .await
        .unwrap();
}

fn post(client: &reqwest::Client, base: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
}

/// Run `initialize` + `notifications/initialized` and return the session id
/// every later request has to carry.
async fn handshake(client: &reqwest::Client, base: &str) -> String {
    let resp = post(client, base)
        .json(&initialize_body())
        .send()
        .await
        .unwrap();
    let session = resp
        .headers()
        .get("mcp-session-id")
        .expect("initialize must assign a session")
        .to_str()
        .unwrap()
        .to_string();
    let _ = resp.text().await;
    post(client, base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .send()
        .await
        .unwrap();
    session
}

#[tokio::test]
async fn mcp_is_absent_until_the_setting_turns_it_on() {
    let (base, _engine) = spawn("gate").await;
    let client = reqwest::Client::new();

    // Off by default. A 404 (not a 403) so a disabled endpoint is
    // indistinguishable from one that was never built.
    let resp = post(&client, &base)
        .json(&initialize_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "MCP must be off until opted into");

    enable_mcp(&base, &client).await;

    let resp = post(&client, &base)
        .json(&initialize_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let payload = rpc_payload(&resp.text().await.unwrap());
    assert_eq!(payload["result"]["serverInfo"]["name"], "auricle");

    // And back off again: the toggle takes effect without a restart.
    client
        .put(format!("{base}/api/v1/settings"))
        .json(&serde_json::json!({ "mcp_enabled": false }))
        .send()
        .await
        .unwrap();
    let resp = post(&client, &base)
        .json(&initialize_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "turning it off must close the endpoint");
}

#[tokio::test]
async fn every_exposed_tool_is_read_only() {
    let (base, _engine) = spawn("tools").await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;

    let session = handshake(&client, &base).await;

    let resp = post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list"
        }))
        .send()
        .await
        .unwrap();
    let payload = rpc_payload(&resp.text().await.unwrap());
    let tools = payload["result"]["tools"].as_array().expect("tools array");

    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "auricle.get_session",
            "auricle.list_sessions",
            "auricle.live_transcript",
            "auricle.search",
        ],
        "the MCP surface changed — if a tool was added, confirm it only reads"
    );

    // The read-only hint is the contract this whole surface rests on: an
    // agent is told up front that nothing here has a side effect.
    for t in tools {
        assert_eq!(
            t["annotations"]["readOnlyHint"], true,
            "{} must be marked read-only",
            t["name"]
        );
    }
}

#[tokio::test]
async fn a_tool_call_is_logged_as_an_agent_read_not_a_local_one() {
    let (base, engine) = spawn("ledger").await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;

    let store = engine.store();
    store
        .create_session(
            "s1",
            "Weekly sync",
            1_700_000_000,
            "whisper-local",
            &serde_json::json!({}),
        )
        .unwrap();
    store
        .insert_segment(
            "s1",
            ChannelId::Mic,
            "You",
            0,
            900,
            "budget approved",
            "whisper-local",
        )
        .unwrap();

    let session = handshake(&client, &base).await;
    let resp = post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "auricle.search",
                "arguments": { "query": "budget" }
            }
        }))
        .send()
        .await
        .unwrap();
    let payload = rpc_payload(&resp.text().await.unwrap());
    assert!(
        payload["result"].to_string().contains("budget approved"),
        "search should return the matching line, got: {}",
        payload["result"]
    );

    let ledger = client
        .get(format!("{base}/api/v1/egress"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let row = ledger["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["destination"] == "agent")
        .expect("an agent read must be on the ledger, not filed as local");
    assert_eq!(row["kind"], "transcript_search");
    // The client's self-reported name from the handshake, so the ledger can
    // say *which* agent read rather than just that one did.
    assert_eq!(row["provider"], "integration-test");
    // Size in characters of transcript, the unit the rest of the ledger uses.
    assert_eq!(row["items"], "budget approved".len() as i64);
    assert!(row["host"].is_null(), "an agent read has no remote host");
}
