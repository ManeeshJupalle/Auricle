//! Who may reach the daemon: bearer-token mode, the browser-facing response
//! headers, and the same-origin rule on REST and /ws/live.

use std::sync::Arc;

use auricle_core::Config;
use auricle_server::{build_router, Engine, EngineOptions};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Error as WsError;

const TOKEN: &str = "right-token";

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("auricle-security-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn(name: &str, token: Option<&str>) -> (String, Arc<Engine>) {
    let engine = Engine::new(EngineOptions {
        cfg: Config::default(),
        data_root: temp_root(name),
        provider_override: None,
    })
    .unwrap();
    let app = build_router(engine.clone(), token.map(str::to_string));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), engine)
}

/// Status of a /ws/live handshake carrying the given headers. 101 on success.
async fn ws_handshake(base: &str, headers: &[(&'static str, String)]) -> u16 {
    ws_handshake_at(base, "/ws/live", headers).await
}

async fn ws_handshake_at(base: &str, path: &str, headers: &[(&'static str, String)]) -> u16 {
    let url = base.replace("http://", "ws://") + path;
    let mut req = url.into_client_request().unwrap();
    for (name, value) in headers {
        req.headers_mut().insert(*name, value.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((_, resp)) => resp.status().as_u16(),
        Err(WsError::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("handshake failed before any status: {e}"),
    }
}

// ---------- token mode ----------

#[tokio::test]
async fn token_mode_guards_rest_by_the_exact_token() {
    let (base, _engine) = spawn("token-rest", Some(TOKEN)).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/v1/settings");

    let missing = client.get(&url).send().await.unwrap();
    assert_eq!(missing.status(), 401);

    for wrong in ["Bearer wrong-token", "Bearer ", "Basic right-token", TOKEN] {
        let resp = client
            .get(&url)
            .header("authorization", wrong)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{wrong:?} must not pass");
    }

    let right = client
        .get(&url)
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(right.status(), 200);
}

#[tokio::test]
async fn token_mode_guards_the_websocket() {
    let (base, _engine) = spawn("token-ws", Some(TOKEN)).await;

    assert_eq!(ws_handshake(&base, &[]).await, 401);
    assert_eq!(
        ws_handshake(&base, &[("authorization", "Bearer wrong-token".into())]).await,
        401
    );
    assert_eq!(
        ws_handshake(&base, &[("authorization", format!("Bearer {TOKEN}"))]).await,
        101
    );
}

#[tokio::test]
async fn token_mode_guards_every_route_that_holds_data() {
    let (base, engine) = spawn("token-routes", Some(TOKEN)).await;
    // Open the gate: /mcp must still want the token with the server enabled.
    engine
        .store()
        .set_setting("mcp_enabled", &serde_json::json!(true))
        .unwrap();
    let client = reqwest::Client::new();

    for path in [
        "/api/v1/health",
        "/api/v1/devices",
        "/api/v1/providers",
        "/api/v1/sessions",
        "/api/v1/sessions/s1",
        "/api/v1/sessions/s1/export",
        "/api/v1/sessions/s1/audio/mic",
        "/api/v1/settings",
        "/api/v1/egress",
        "/api/v1/diagnostics",
        "/api/v1/asks",
    ] {
        let resp = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(resp.status(), 401, "GET {path}");
    }
    for path in [
        "/api/v1/sessions",
        "/api/v1/sessions/s1/stop",
        "/api/v1/sessions/s1/summarize",
        "/api/v1/peek",
        "/api/v1/ask",
        "/api/v1/shutdown",
        "/api/v1/backup",
        "/mcp",
    ] {
        let resp = client.post(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(resp.status(), 401, "POST {path}");
    }
    let resp = client
        .delete(format!("{base}/api/v1/asks/1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "DELETE /api/v1/asks/1");
}

/// A session whose retained mic audio is a real file on disk.
fn seed_retained_audio(engine: &Engine) {
    let dir = engine.data_root().join("sessions").join("s1");
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("mic_16k.wav");
    std::fs::write(&wav, b"RIFF").unwrap();
    engine
        .store()
        .create_session(
            "s1",
            "t",
            1_700_000_000,
            "fake",
            &serde_json::json!({"audio": {"mic": wav.to_string_lossy()}}),
        )
        .unwrap();
}

#[tokio::test]
async fn the_socket_and_retained_audio_take_a_query_token() {
    // A browser cannot set a header on a WebSocket or an <audio> element.
    let (base, engine) = spawn("token-query", Some(TOKEN)).await;
    seed_retained_audio(&engine);
    let client = reqwest::Client::new();

    assert_eq!(
        ws_handshake_at(&base, &format!("/ws/live?token={TOKEN}"), &[]).await,
        101
    );
    assert_eq!(
        ws_handshake_at(&base, "/ws/live?token=wrong-token", &[]).await,
        401
    );
    assert_eq!(ws_handshake_at(&base, "/ws/live?token=", &[]).await, 401);

    let audio = format!("{base}/api/v1/sessions/s1/audio/mic");
    let status = |url: String| {
        let client = client.clone();
        async move { client.get(url).send().await.unwrap().status() }
    };
    assert_eq!(status(format!("{audio}?token={TOKEN}")).await, 200);
    // Percent-encoded the way encodeURIComponent would leave it.
    assert_eq!(status(format!("{audio}?token=right%2Dtoken")).await, 200);
    assert_eq!(status(format!("{audio}?token=wrong-token")).await, 401);
    assert_eq!(status(audio.clone()).await, 401);

    // The header keeps working on both.
    let resp = client
        .get(&audio)
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn no_other_route_takes_a_query_token() {
    let (base, engine) = spawn("token-query-elsewhere", Some(TOKEN)).await;
    seed_retained_audio(&engine);
    engine
        .store()
        .set_setting("mcp_enabled", &serde_json::json!(true))
        .unwrap();
    let client = reqwest::Client::new();

    for path in [
        "/api/v1/health",
        "/api/v1/sessions",
        "/api/v1/sessions/s1",
        "/api/v1/sessions/s1/export",
        "/api/v1/settings",
        "/api/v1/egress",
        "/api/v1/asks",
    ] {
        let resp = client
            .get(format!("{base}{path}?token={TOKEN}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "GET {path}");
    }
    for path in ["/api/v1/shutdown", "/api/v1/backup", "/mcp"] {
        let resp = client
            .post(format!("{base}{path}?token={TOKEN}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "POST {path}");
    }
    assert!(!engine.data_root().join("backups").exists());
}

#[tokio::test]
async fn token_mode_still_serves_the_dashboard_shell() {
    let (base, _engine) = spawn("token-static", Some(TOKEN)).await;
    let client = reqwest::Client::new();

    // No browser can attach a token to a page load, so the shell has to come
    // down without one. It holds no user data.
    let index = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(index.status(), 200);
    let html = index.text().await.unwrap();
    assert!(html.contains("<div id=\"root\">"), "{html}");

    let asset = html
        .split('"')
        .find(|s| s.starts_with("/assets/"))
        .expect("the shell references its bundle");
    let bundle = client.get(format!("{base}{asset}")).send().await.unwrap();
    assert_eq!(bundle.status(), 200, "{asset}");

    let deep_link = client
        .get(format!("{base}/sessions/s1"))
        .send()
        .await
        .unwrap();
    assert_eq!(deep_link.status(), 200, "SPA fallback");

    // The API behind that shell is what stays closed: a path that is not a
    // route gets the plain 404 and no content.
    let unknown = client
        .get(format!("{base}/api/v1/no-such-route"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    assert!(!unknown.text().await.unwrap().contains("<div id=\"root\">"));
}

#[tokio::test]
async fn token_mode_keeps_the_origin_rule_on_the_shell() {
    let (base, _engine) = spawn("token-origin", Some(TOKEN)).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn shutdown_refuses_a_foreign_origin() {
    // A cross-site POST with no body needs no preflight, so the origin check
    // is what keeps a web page from ending the daemon.
    let (base, _engine) = spawn("shutdown-origin", None).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/shutdown"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

// ---------- browser-facing headers ----------

/// `stt_language` ends up in a provider URL, so a bad value has to be refused
/// when it is saved, not discovered at the next Start.
#[tokio::test]
async fn a_malformed_language_is_refused_when_saved() {
    let (base, _engine) = spawn("language-setting", None).await;
    let client = reqwest::Client::new();
    let put = |value: serde_json::Value| {
        client
            .put(format!("{base}/api/v1/settings"))
            .json(&serde_json::json!({ "stt_language": value }))
            .send()
    };

    for bad in ["en&model=x", "", "../etc", "en us"] {
        let status = put(serde_json::json!(bad)).await.unwrap().status();
        assert_eq!(status, 400, "{bad:?}");
    }
    assert_eq!(put(serde_json::json!(7)).await.unwrap().status(), 400);

    for good in ["auto", "en", "pt-BR"] {
        let status = put(serde_json::json!(good)).await.unwrap().status();
        assert_eq!(status, 200, "{good:?}");
    }
}

#[tokio::test]
async fn responses_carry_nosniff_and_a_framing_ban() {
    let (base, _engine) = spawn("headers", None).await;
    let client = reqwest::Client::new();

    for path in ["/", "/api/v1/health", "/api/v1/sessions/none"] {
        let resp = client.get(format!("{base}{path}")).send().await.unwrap();
        let headers = resp.headers();
        assert_eq!(headers["x-content-type-options"], "nosniff", "{path}");
        assert_eq!(
            headers["cross-origin-resource-policy"], "same-origin",
            "{path}"
        );
        let csp = headers["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
        assert!(csp.contains("default-src 'self'"), "{path}: {csp}");
        assert!(!csp.contains("unsafe"), "{path}: {csp}");
    }

    // Refusals too: they are responses a browser can be pointed at.
    let refused = client
        .get(format!("{base}/api/v1/health"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 403);
    assert_eq!(refused.headers()["x-content-type-options"], "nosniff");
}

// ---------- /ws/live origin ----------

#[tokio::test]
async fn ws_accepts_its_own_origin_and_the_dev_proxy_only() {
    let (base, _engine) = spawn("ws-origin", None).await;
    let addr = base.strip_prefix("http://").unwrap().to_string();

    // Not a browser: nothing to vouch for, same as any local process.
    assert_eq!(ws_handshake(&base, &[]).await, 101);

    // The bundled dashboard.
    assert_eq!(
        ws_handshake(&base, &[("origin", format!("http://{addr}"))]).await,
        101
    );

    // Another page on this machine. CORS never applies to a WebSocket
    // handshake, so the origin check is all that stands between it and the
    // live transcript.
    assert_eq!(
        ws_handshake(&base, &[("origin", "http://localhost:3000".into())]).await,
        403
    );
    assert_eq!(
        ws_handshake(&base, &[("origin", "http://127.0.0.1:8080".into())]).await,
        403
    );

    // `npm run dev`: Vite's /ws proxy keeps the browser's Host, so the dev
    // server arrives same-origin and needs no exception...
    assert_eq!(
        ws_handshake(
            &base,
            &[
                ("origin", "http://localhost:5173".into()),
                ("host", "localhost:5173".into()),
            ],
        )
        .await,
        101
    );
    // ...which is why a page on that port dialing the daemon directly gets
    // no special treatment.
    assert_eq!(
        ws_handshake(&base, &[("origin", "http://localhost:5173".into())]).await,
        403
    );
}

#[tokio::test]
async fn rest_refuses_another_page_on_this_machine() {
    // These POSTs carry no body, so a browser sends them without a preflight:
    // only the origin check keeps a page on another localhost port from
    // stopping a recording or the daemon.
    let (base, engine) = spawn("rest-origin", None).await;
    let client = reqwest::Client::new();
    engine
        .store()
        .create_session("s1", "t", 1_700_000_000, "fake", &serde_json::json!({}))
        .unwrap();

    for origin in [
        "http://localhost:3000",
        "http://127.0.0.1:8080",
        "http://[::1]:5173",
    ] {
        for path in ["/api/v1/shutdown", "/api/v1/sessions/s1/stop"] {
            let resp = client
                .post(format!("{base}{path}"))
                .header("origin", origin)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403, "POST {path} from {origin}");
        }
        let resp = client
            .delete(format!("{base}/api/v1/sessions/s1"))
            .header("origin", origin)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "DELETE from {origin}");
    }
    assert!(engine.store().get_session("s1").unwrap().is_some());
}

#[tokio::test]
async fn rest_answers_its_own_origin() {
    let (base, engine) = spawn("rest-same-origin", None).await;
    let client = reqwest::Client::new();
    engine
        .store()
        .create_session("s1", "t", 1_700_000_000, "fake", &serde_json::json!({}))
        .unwrap();

    // The bundled dashboard: its fetches carry the daemon's own origin.
    let resp = client
        .delete(format!("{base}/api/v1/sessions/s1"))
        .header("origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(engine.store().get_session("s1").unwrap().is_none());

    // Not a browser: nothing to vouch for, same as any local process.
    let resp = client
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn rest_still_answers_through_the_dev_proxy() {
    // `npm run dev`: Vite's /api proxy keeps the browser's Host, as its /ws
    // proxy does, so the dev server arrives same-origin.
    let (base, _engine) = spawn("dev-rest", None).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/sessions/none/stop"))
        .header("host", "localhost:5173")
        .header("origin", "http://localhost:5173")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "reached the route: nothing is active");

    // ...which is why a page on that port dialing the daemon directly gets
    // no special treatment.
    let resp = client
        .post(format!("{base}/api/v1/sessions/none/stop"))
        .header("origin", "http://localhost:5173")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}
