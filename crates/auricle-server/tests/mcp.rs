//! The MCP endpoint: absent until the setting turns it on, and read-only
//! when it is on.

use std::sync::Arc;

use async_trait::async_trait;
use auricle_core::{AudioChunk, ChannelId, ChunkKind, Config, Result, Segment, SttEvent};
use auricle_server::{
    build_router, AudioSource, DictationParams, Engine, EngineOptions, StartParams,
};
use auricle_stt::{SessionCfg, SttKind, SttProvider, SttSession};
use tokio::sync::mpsc;

const SECRET: &str = "the merger closes friday";

/// Transcribes `SECRET` from every final chunk.
struct SecretProvider;

struct SecretSession {
    tx: mpsc::UnboundedSender<SttEvent>,
    rx: mpsc::UnboundedReceiver<SttEvent>,
}

#[async_trait]
impl SttProvider for SecretProvider {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn kind(&self) -> SttKind {
        SttKind::Local
    }
    async fn start_session(&self, _cfg: &SessionCfg) -> Result<Box<dyn SttSession>> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Box::new(SecretSession { tx, rx }))
    }
}

#[async_trait]
impl SttSession for SecretSession {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()> {
        if chunk.kind == ChunkKind::Final {
            let _ = self.tx.send(SttEvent::Final(Segment {
                channel: chunk.channel,
                t_start_ms: chunk.t_start_ms,
                t_end_ms: chunk.t_end_ms,
                text: format!("{SECRET} {}", chunk.seq),
            }));
        }
        Ok(())
    }
    async fn next_event(&mut self) -> Option<SttEvent> {
        self.rx.recv().await
    }
    async fn finish(&mut self) -> Result<Vec<Segment>> {
        self.rx.close();
        Ok(Vec::new())
    }
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("auricle-mcp-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn(name: &str) -> (String, Arc<Engine>) {
    spawn_with(name, None).await
}

async fn spawn_with(
    name: &str,
    provider_override: Option<Arc<dyn SttProvider>>,
) -> (String, Arc<Engine>) {
    let engine = Engine::new(EngineOptions {
        cfg: Config::default(),
        data_root: temp_root(name),
        provider_override,
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
async fn list_sessions_pages_and_says_how_many_meetings_match() {
    let (base, engine) = spawn("list-pages").await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;
    let store = engine.store();
    // One more than a default page holds, the last few being a different title.
    for i in 0..51 {
        let title = if i < 3 { "Retro" } else { "Standup" };
        store
            .create_session(
                &format!("s{i:02}"),
                title,
                1_700_000_000 + i,
                "whisper-local",
                &serde_json::json!({}),
            )
            .unwrap();
    }
    let session = handshake(&client, &base).await;

    let list = |arguments: serde_json::Value| {
        let (client, base, session) = (client.clone(), base.clone(), session.clone());
        async move {
            let resp = post(&client, &base)
                .header("mcp-session-id", &session)
                .json(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": { "name": "auricle.list_sessions", "arguments": arguments }
                }))
                .send()
                .await
                .unwrap();
            let payload = rpc_payload(&resp.text().await.unwrap());
            let result = payload["result"]["structuredContent"].clone();
            let ids: Vec<String> = result["sessions"]
                .as_array()
                .unwrap_or_else(|| panic!("no sessions in {payload}"))
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_string())
                .collect();
            (ids, result["total"].as_i64().unwrap())
        }
    };

    // A bare call cannot drown the agent, and says that it was cut.
    let (ids, total) = list(serde_json::json!({})).await;
    assert_eq!((ids.len(), total), (50, 51));
    assert_eq!(ids[0], "s50", "newest first");

    let (ids, total) = list(serde_json::json!({"limit": 2, "offset": 49})).await;
    assert_eq!(ids, ["s01", "s00"]);
    assert_eq!(total, 51);

    let (ids, total) = list(serde_json::json!({"query": "retro", "limit": 2})).await;
    assert_eq!(ids, ["s02", "s01"]);
    assert_eq!(total, 3, "counts the matches, not every meeting");

    // Out-of-range values land in range rather than erroring.
    let (ids, _) = list(serde_json::json!({"limit": 0, "offset": -5})).await;
    assert_eq!(ids, ["s50"]);
    let (ids, _) = list(serde_json::json!({"limit": 5000})).await;
    assert_eq!(ids.len(), 51);
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
    // Size in characters of everything disclosed — the matched line *and* the
    // meeting title that came with it — in the unit the rest of the ledger uses.
    assert_eq!(
        row["items"],
        ("budget approved".len() + "Weekly sync".len()) as i64
    );
    assert!(row["host"].is_null(), "an agent read has no remote host");
}

/// The ledger used to record the *active* session for every read, and count
/// only transcript characters while also returning the title and summaries.
#[tokio::test]
async fn a_read_is_attributed_to_the_meeting_actually_read() {
    let (base, engine) = spawn("attribution").await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;

    let store = engine.store();
    store
        .create_session(
            "past",
            "Pricing review",
            1_700_000_000,
            "whisper-local",
            &serde_json::json!({}),
        )
        .unwrap();
    store
        .insert_segment(
            "past",
            ChannelId::Mic,
            "You",
            0,
            900,
            "ship on thursday",
            "whisper-local",
        )
        .unwrap();
    store
        .insert_summary(
            "past",
            "minutes",
            "llama3",
            "Agreed to ship Thursday.",
            1_700_000_100,
        )
        .unwrap();
    store.end_session("past", 1_700_000_180).unwrap();

    let session = handshake(&client, &base).await;
    let resp = post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "auricle.get_session", "arguments": { "id": "past" } }
        }))
        .send()
        .await
        .unwrap();
    let payload = rpc_payload(&resp.text().await.unwrap());
    assert!(
        payload["result"]["isError"] != true,
        "{}",
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
        .find(|r| r["kind"] == "session_read")
        .expect("the read must be on the ledger");

    // The session that was read — not whichever one happens to be recording.
    assert_eq!(row["session_id"], "past");
    // Transcript + title + summary, because all three were disclosed.
    let expected =
        "ship on thursday".len() + "Pricing review".len() + "Agreed to ship Thursday.".len();
    assert_eq!(
        row["items"], expected as i64,
        "the ledger must size everything the response disclosed"
    );
}

/// rmcp synthesizes peer info from its own build identity on stateless calls,
/// so reading `peer.peer_info()` filed rows against "rmcp". The client's own
/// name must survive into the ledger.
#[tokio::test]
async fn the_ledger_records_the_calling_client_not_the_mcp_library() {
    let (base, _engine) = spawn("client-name").await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;

    let session = handshake(&client, &base).await;
    post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "auricle.live_transcript", "arguments": {} }
        }))
        .send()
        .await
        .unwrap()
        // Streamable HTTP sends the headers before the tool runs, so only the
        // finished body proves the read reached the ledger.
        .text()
        .await
        .unwrap();

    let ledger = client
        .get(format!("{base}/api/v1/egress"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let row = ledger["entries"].as_array().unwrap().first().unwrap();
    assert_eq!(
        row["provider"], "integration-test",
        "a row naming rmcp (or anything but the caller) misattributes the read"
    );
}

/// What `auricle.live_transcript` answers right now, as raw response text.
async fn live_transcript(client: &reqwest::Client, base: &str, session: &str) -> String {
    post(client, base)
        .header("mcp-session-id", session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "auricle.live_transcript", "arguments": {} }
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// A stopped session whose words the live transcript is serving.
async fn recorded_meeting(
    client: &reqwest::Client,
    base: &str,
    engine: &Arc<Engine>,
    mcp_session: &str,
) -> String {
    let id = engine
        .start_session(StartParams {
            title: Some("board meeting".into()),
            audio: Some(AudioSource::WavFile {
                path: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/tts_loopback_16k.wav"),
                channel: ChannelId::Loopback,
            }),
            ..StartParams::default()
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    engine.stop_session(&id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.active_session().is_some() {
        assert!(std::time::Instant::now() < deadline, "stop did not finish");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let live = live_transcript(client, base, mcp_session).await;
    assert!(live.contains(SECRET), "nothing to lose: {live}");
    id
}

#[tokio::test]
async fn a_dictation_never_reaches_an_agent() {
    let (base, engine) = spawn_with("dictation", Some(Arc::new(SecretProvider))).await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;
    let session = handshake(&client, &base).await;

    engine
        .start_dictation(DictationParams {
            audio: Some(AudioSource::WavFile {
                path: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/tts_loopback_16k.wav"),
                channel: ChannelId::Mic,
            }),
            ..DictationParams::default()
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let dictated = engine.stop_dictation().await.unwrap();
    assert!(
        dictated.text.contains(SECRET),
        "nothing to leak: {dictated:?}"
    );

    let live = live_transcript(&client, &base, &session).await;
    assert!(!live.contains(SECRET), "{live}");
    let found = post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 11, "method": "tools/call",
            "params": { "name": "auricle.search", "arguments": { "query": "merger" } }
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!found.contains(SECRET), "{found}");

    // A meeting's words do reach the live transcript, so the look above is real.
    recorded_meeting(&client, &base, &engine, &session).await;
}

#[tokio::test]
async fn a_deleted_meeting_leaves_the_live_transcript() {
    let (base, engine) = spawn_with("ring-deleted", Some(Arc::new(SecretProvider))).await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;
    let session = handshake(&client, &base).await;
    let id = recorded_meeting(&client, &base, &engine, &session).await;

    let deleted = client
        .delete(format!("{base}/api/v1/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);

    let live = live_transcript(&client, &base, &session).await;
    assert!(!live.contains(SECRET), "{live}");
}

#[tokio::test]
async fn a_meeting_retention_took_leaves_the_live_transcript() {
    let (base, engine) = spawn_with("ring-expired", Some(Arc::new(SecretProvider))).await;
    let client = reqwest::Client::new();
    enable_mcp(&base, &client).await;
    let session = handshake(&client, &base).await;
    let id = recorded_meeting(&client, &base, &engine, &session).await;

    let store = engine.store();
    store
        .set_setting("retention_days", &serde_json::json!(1))
        .unwrap();
    let now = store.get_session(&id).unwrap().unwrap().ended_at.unwrap();
    assert_eq!(engine.sweep_retention_at(now).unwrap().deleted, 0);
    let sweep = engine.sweep_retention_at(now + 2 * 86_400).unwrap();
    assert_eq!(sweep.deleted, 1, "{:?}", sweep.notice);

    let live = live_transcript(&client, &base, &session).await;
    assert!(!live.contains(SECRET), "{live}");
}

/// `egress::record` used to be best-effort — it logged the failure and let the
/// request through. That made the ledger quietly incomplete, which is worse
/// than a refused call, because the whole argument for letting agents read
/// private meetings is that every read is on the record.
#[tokio::test]
async fn a_read_is_refused_when_the_ledger_cannot_be_written() {
    let (base, engine) = spawn("fail-closed").await;
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

    // Take the database's single writer slot and keep it. WAL still lets the
    // daemon read the transcript, so this isolates the ledger write.
    let blocker = rusqlite::Connection::open(engine.data_root().join("auricle.db")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let resp = post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": "auricle.get_session", "arguments": { "id": "s1" } }
        }))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();

    assert!(
        !body.contains("budget approved"),
        "transcript content was disclosed without a ledger row: {body}"
    );
    let payload = rpc_payload(&body);
    let reported_error = payload["result"]["isError"] == true || payload.get("error").is_some();
    assert!(
        reported_error,
        "the call should fail, not silently succeed: {payload}"
    );

    blocker.execute_batch("ROLLBACK").unwrap();

    // With the ledger writable again, the same call works and is recorded.
    let resp = post(&client, &base)
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": { "name": "auricle.get_session", "arguments": { "id": "s1" } }
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.text().await.unwrap().contains("budget approved"));
}
