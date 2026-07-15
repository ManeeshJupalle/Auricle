//! REST surface (architecture §4.8) + server bootstrap.

use std::path::PathBuf;
use std::sync::Arc;

use auricle_core::{Config, Error, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::engine::{Engine, EngineOptions, StartError, StartParams, StopError};
use crate::export::render_markdown_full;
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    /// Bearer token required on every request (None on localhost binds).
    pub token: Option<Arc<String>>,
    /// On-demand screen capture + OCR for /peek and /ask (mocked in tests).
    pub screen_reader: Arc<dyn auricle_vision::ScreenReader>,
    /// Rolling in-memory window of final segments feeding /ask.
    pub(crate) ring: Arc<crate::ring::TranscriptRing>,
    /// In-memory follow-up memory for /ask.
    pub(crate) ask_history: Arc<crate::ask::AskHistory>,
}

/// Build the router. Exposed for the integration tests, which bind an
/// ephemeral port and inject a fake provider through the engine options.
pub fn build_router(engine: Arc<Engine>, token: Option<String>) -> Router {
    build_router_with_reader(
        engine,
        token,
        Arc::new(auricle_vision::WindowsOcrReader::new()),
    )
}

/// `build_router` with an injected `ScreenReader` (peek/ask endpoint
/// tests). Must run inside a tokio runtime: it spawns the transcript-ring
/// feeder on the engine's broadcast channel.
pub fn build_router_with_reader(
    engine: Arc<Engine>,
    token: Option<String>,
    screen_reader: Arc<dyn auricle_vision::ScreenReader>,
) -> Router {
    let ring = crate::ring::TranscriptRing::new(engine.config().copilot.transcript_window_min);
    ring.spawn_feeder(engine.subscribe_important());
    let state = AppState {
        engine,
        token: token.map(Arc::new),
        screen_reader,
        ring,
        ask_history: Arc::new(crate::ask::AskHistory::default()),
    };
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/providers", get(providers))
        .route("/api/v1/sessions", post(start_session).get(list_sessions))
        .route(
            "/api/v1/sessions/{id}",
            get(get_session)
                .patch(rename_session)
                .delete(delete_session),
        )
        .route("/api/v1/sessions/{id}/audio/{channel}", get(session_audio))
        .route("/api/v1/sessions/{id}/stop", post(stop_session))
        .route("/api/v1/sessions/{id}/export", get(export_session))
        .route("/api/v1/sessions/{id}/summarize", post(summarize_session))
        .route("/api/v1/settings", get(get_settings).put(put_settings))
        .route("/api/v1/peek", post(peek))
        .route("/api/v1/ask", post(crate::ask::ask))
        .route("/ws/live", get(crate::ws::ws_handler))
        // Embedded web UI (ui/dist via rust-embed): everything that isn't
        // an API route serves static files, unknown paths fall back to
        // index.html (SPA).
        .fallback(static_handler)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

#[derive(rust_embed::Embed)]
#[folder = "$CARGO_MANIFEST_DIR/ui-dist"]
struct UiAssets;

async fn static_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let file = UiAssets::get(path).or_else(|| {
        if path.starts_with("api/") {
            None
        } else {
            UiAssets::get("index.html") // SPA fallback
        }
    });
    match file {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([("content-type", mime.as_ref().to_string())], f.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
    }
}

/// Run the daemon: open the store, bind, serve until the process ends.
pub async fn serve(cfg: Config, data_root: PathBuf, bind_override: Option<String>) -> Result<()> {
    let bind = bind_override.unwrap_or_else(|| cfg.server.bind.clone());
    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| Error::Config(format!("invalid bind address \"{bind}\": {e}")))?;

    // Bearer token: only demanded when exposed beyond localhost.
    let token = if addr.ip().is_loopback() {
        None
    } else {
        let var = cfg.server.token_env.clone();
        Some(auricle_core::env_lookup(&var).ok_or_else(|| {
            Error::Config(format!(
                "binding to non-localhost {addr} requires a bearer token in ${var}"
            ))
        })?)
    };

    let engine = Engine::new(EngineOptions {
        cfg,
        data_root: data_root.clone(),
        provider_override: None,
    })?;
    let app = build_router(engine.clone(), token);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(Error::Io)?;
    eprintln!(
        "auricle listening on http://{addr} (data: {})",
        data_root.display()
    );
    // Run until Ctrl+C, then stop any active session cleanly before exiting
    // — an intentional shutdown must not be recorded as an interrupted
    // session. (No graceful connection drain: open WebSockets would hold
    // the process forever; clients auto-reconnect.)
    tokio::select! {
        result = axum::serve(listener, app) => result.map_err(Error::Io),
        _ = shutdown_on_ctrl_c(engine) => Ok(()),
    }
}

async fn shutdown_on_ctrl_c(engine: Arc<Engine>) {
    if tokio::signal::ctrl_c().await.is_err() {
        // Can't install a handler: never resolve, run until killed.
        std::future::pending::<()>().await;
    }
    if let Some(id) = engine.active_session() {
        eprintln!("\nstopping active session {id} (Ctrl+C again to force quit) ...");
        let _ = engine.stop_session(&id);
        let stopped = async {
            while engine.active_session().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        };
        tokio::select! {
            _ = stopped => {}
            _ = tokio::signal::ctrl_c() => eprintln!("force quit; session left for crash recovery"),
        }
    }
    eprintln!("auricle stopped");
}

async fn auth_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Browser-attached Origins must be same-origin (or another local page).
    // CORS does not protect WebSocket handshakes: without this, any website
    // could open ws://127.0.0.1:4820/ws/live and read meetings live.
    let host = request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(origin) = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
    {
        if !origin_allowed(origin, host.as_deref()) {
            return forbidden("cross-origin requests are not allowed");
        }
    }
    // Loopback binds run tokenless, so the request must actually address
    // localhost — a DNS-rebound hostname resolving to 127.0.0.1 does not.
    if state.token.is_none() && !host.as_deref().is_some_and(authority_is_loopback) {
        return forbidden("requests must address the daemon as localhost");
    }
    if let Some(expected) = &state.token {
        let ok = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| t == expected.as_str());
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// An Origin is acceptable when it is same-origin with the request's Host,
/// or itself a local page (the Vite dev server on :5173 proxies here with
/// its own origin; a local page is as trusted as any local process).
fn origin_allowed(origin: &str, host: Option<&str>) -> bool {
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false; // "null", file://, or malformed: no way to vouch
    };
    host.is_some_and(|h| authority.eq_ignore_ascii_case(h)) || authority_is_loopback(authority)
}

/// True when a `host[:port]` authority is localhost or a loopback IP
/// (handles `127.0.0.1:4820`, `localhost`, `[::1]:4820`, raw `::1`).
fn authority_is_loopback(authority: &str) -> bool {
    let a = authority.trim();
    let bare = if let Some(rest) = a.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if a.matches(':').count() > 1 {
        a // raw IPv6 literal: colons are part of the address, not a port
    } else {
        a.split(':').next().unwrap_or(a)
    };
    bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"error": msg}))).into_response()
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "state": state.engine.lifecycle_state(),
        "active_session": state.engine.active_session(),
    }))
}

async fn devices() -> Response {
    // COM-touching enumeration runs off the async threads.
    let result = tokio::task::spawn_blocking(auricle_capture::enumerate).await;
    match result {
        Ok(Ok(devices)) => {
            let list: Vec<_> = devices
                .iter()
                .map(|d| {
                    json!({
                        "name": d.name,
                        "kind": match d.kind {
                            auricle_capture::DeviceKind::Input => "input",
                            auricle_capture::DeviceKind::Loopback => "loopback",
                        },
                        "is_default": d.is_default,
                        "sample_rate_hz": d.sample_rate_hz,
                        "channels": d.channels,
                        "sample_format": d.sample_format,
                    })
                })
                .collect();
            Json(json!({"devices": list})).into_response()
        }
        Ok(Err(e)) => internal(e.to_string()),
        Err(e) => internal(format!("device enumeration task failed: {e}")),
    }
}

/// Overlay the DB settings that steer LLM use onto the TOML config
/// (mirrors the engine's STT overlay; precedence: settings > config file).
/// Shared by the providers/summarize endpoints and the post-stop
/// auto-titler.
pub(crate) fn overlay_llm_settings(cfg: &mut Config, store: &Store) {
    if let Ok(settings) = store.all_settings() {
        if let Some(m) = settings.get("ollama_model").and_then(|v| v.as_str()) {
            cfg.llm.ollama.model = m.to_string();
        }
        if let Some(p) = settings.get("llm_provider").and_then(|v| v.as_str()) {
            cfg.llm.provider = p.to_string();
        }
    }
}

async fn providers(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut cfg = state.engine.config().clone();
    overlay_llm_settings(&mut cfg, &state.engine.store());
    let cfg = &cfg;
    let data_root = crate::default_data_root().unwrap_or_default();
    let statuses: Vec<_> = auricle_stt::provider_statuses(cfg, &data_root.join("models"))
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "kind": s.kind.to_string(),
                "ready": s.ready,
                "detail": s.detail,
                "default": s.id == cfg.stt.provider,
            })
        })
        .collect();
    let llm: Vec<_> = auricle_llm::llm_provider_statuses(cfg)
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "model": s.model,
                "ready": s.ready,
                "detail": s.detail,
                "default": s.id == cfg.llm.provider,
            })
        })
        .collect();
    let templates = auricle_llm::list_templates(&data_root.join("templates"));
    Json(json!({"providers": statuses, "llm": llm, "templates": templates}))
}

#[derive(serde::Deserialize)]
struct SummarizeBody {
    template: Option<String>,
    provider: Option<String>,
}

async fn summarize_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SummarizeBody>,
) -> Response {
    let mut cfg = state.engine.config().clone();
    let template_name = body.template.unwrap_or_else(|| "minutes".to_string());
    let store = state.engine.store();
    overlay_llm_settings(&mut cfg, &store);
    // Precedence: request body > settings store > config file.
    let provider_id = body.provider.unwrap_or_else(|| cfg.llm.provider.clone());
    let session = match store.get_session(&id) {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(&id),
        Err(e) => return internal(e.to_string()),
    };
    let segments = match store.get_segments(&id) {
        Ok(s) => s,
        Err(e) => return internal(e.to_string()),
    };
    if segments.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "session has no transcript to summarize"})),
        )
            .into_response();
    }

    let templates_dir = crate::default_data_root()
        .unwrap_or_default()
        .join("templates");
    let template = match auricle_llm::load_template(&templates_dir, &template_name) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let provider = match auricle_llm::create_llm_provider(&provider_id, &cfg) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    let transcript = crate::export::render_transcript_text(&segments);
    let content = match auricle_llm::summarize(provider.as_ref(), &template, &transcript).await {
        Ok(c) => c,
        Err(e) => {
            // Upstream LLM failure (endpoint down, model missing, quota).
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let summary_id = match store.insert_summary(
        &session.id,
        &template_name,
        provider.model(),
        &content,
        created_at,
    ) {
        Ok(rowid) => rowid,
        Err(e) => return internal(e.to_string()),
    };

    (
        StatusCode::CREATED,
        Json(json!({
            "id": summary_id,
            "session": session.id,
            "template": template_name,
            "provider": provider.id(),
            "model": provider.model(),
            "created_at": created_at,
            "content": content,
        })),
    )
        .into_response()
}

async fn start_session(State(state): State<AppState>, Json(params): Json<StartParams>) -> Response {
    match state.engine.start_session(params).await {
        Ok(id) => (StatusCode::CREATED, Json(json!({"id": id}))).into_response(),
        Err(StartError::Busy(active)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "a session is already active",
                "active_session": active,
            })),
        )
            .into_response(),
        Err(StartError::Invalid(msg)) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
        }
        Err(StartError::Internal(e)) => internal(e.to_string()),
    }
}

async fn stop_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.engine.stop_session(&id) {
        Ok(()) => Json(json!({"id": id, "status": "stopping"})).into_response(),
        Err(StopError::NotActive) => (
            StatusCode::CONFLICT,
            Json(json!({"error": format!("session {id} is not the active session")})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ListQuery {
    q: Option<String>,
}

async fn list_sessions(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    match state.engine.store().list_sessions(q.q.as_deref()) {
        Ok(rows) => Json(json!({"sessions": rows})).into_response(),
        Err(e) => internal(e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct RenameBody {
    title: String,
}

async fn rename_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Response {
    let title = body.title.trim();
    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "title must not be empty"})),
        )
            .into_response();
    }
    let store = state.engine.store();
    match store.rename_session(&id, title) {
        Ok(true) => {
            // A user-chosen name is permanent: clear the auto-title flag so
            // the post-stop generator never overwrites it.
            if let Ok(Some(session)) = store.get_session(&id) {
                if session.meta["auto_title"] == serde_json::Value::Bool(true) {
                    let mut meta = session.meta.clone();
                    meta["auto_title"] = serde_json::json!(false);
                    let _ = store.update_session_meta(&id, &meta);
                }
            }
            Json(json!({"id": id, "title": title})).into_response()
        }
        Ok(false) => not_found(&id),
        Err(e) => internal(e.to_string()),
    }
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.engine.active_session().as_deref() == Some(id.as_str()) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "session is currently recording; stop it first"})),
        )
            .into_response();
    }
    // Retained audio (if any) lives outside the DB; remove it with the rows.
    let audio_dir = crate::default_data_root()
        .map(|r| r.join("sessions").join(&id))
        .ok();
    match state.engine.store().delete_session(&id) {
        Ok(true) => {
            if let Some(dir) = audio_dir {
                let _ = std::fs::remove_dir_all(dir); // best-effort
            }
            Json(json!({"id": id, "deleted": true})).into_response()
        }
        Ok(false) => not_found(&id),
        Err(e) => internal(e.to_string()),
    }
}

/// Serve a retained per-channel WAV (`mic` | `loopback`). Sessions recorded
/// without `retain_raw_audio` have no audio and return 404. Delegates to
/// tower-http's ServeFile: retained WAVs can be hours long, so they are
/// streamed from disk with Range/206 support — which is also what lets the
/// UI's click-timestamp-to-seek work without re-downloading the file.
// FUTURE: re-transcribe a stored session with a different provider from
// these WAVs.
async fn session_audio(
    State(state): State<AppState>,
    Path((id, channel)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    if channel != "mic" && channel != "loopback" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "channel must be \"mic\" or \"loopback\""})),
        )
            .into_response();
    }
    let session = match state.engine.store().get_session(&id) {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(&id),
        Err(e) => return internal(e.to_string()),
    };
    let Some(path) = session.meta["audio"][&channel].as_str() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no retained audio for this session/channel"})),
        )
            .into_response();
    };
    if tokio::fs::metadata(path).await.is_err() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "retained audio file is missing on disk"})),
        )
            .into_response();
    }
    match tower::ServiceExt::oneshot(tower_http::services::ServeFile::new(path), request).await {
        Ok(resp) => resp.into_response(),
        Err(infallible) => match infallible {},
    }
}

async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.engine.store();
    match (
        store.get_session(&id),
        store.get_segments(&id),
        store.get_summaries(&id),
    ) {
        (Ok(Some(session)), Ok(segments), Ok(summaries)) => Json(json!({
            "id": session.id,
            "title": session.title,
            "started_at": session.started_at,
            "ended_at": session.ended_at,
            "stt_provider": session.stt_provider,
            "meta": session.meta,
            "transcript": segments,
            "summaries": summaries,
        }))
        .into_response(),
        (Ok(None), _, _) => not_found(&id),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => internal(e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

async fn export_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Response {
    let format = q.format.unwrap_or_else(|| "md".to_string());
    if format != "md" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("unsupported format \"{format}\" (supported: md)")})),
        )
            .into_response();
    }
    let store = state.engine.store();
    match (
        store.get_session(&id),
        store.get_segments(&id),
        store.get_summaries(&id),
    ) {
        (Ok(Some(session)), Ok(segments), Ok(summaries)) => {
            let md = render_markdown_full(&session, &segments, &summaries);
            ([("content-type", "text/markdown; charset=utf-8")], md).into_response()
        }
        (Ok(None), _, _) => not_found(&id),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => internal(e.to_string()),
    }
}

async fn get_settings(State(state): State<AppState>) -> Response {
    match state.engine.store().all_settings() {
        Ok(map) => Json(serde_json::Value::Object(map)).into_response(),
        Err(e) => internal(e.to_string()),
    }
}

async fn put_settings(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(obj) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "settings body must be a JSON object"})),
        )
            .into_response();
    };
    let store = state.engine.store();
    for (k, v) in obj {
        if let Err(e) = store.set_setting(k, v) {
            return internal(e.to_string());
        }
    }
    match store.all_settings() {
        Ok(map) => Json(serde_json::Value::Object(map)).into_response(),
        Err(e) => internal(e.to_string()),
    }
}

/// POST /api/v1/peek — capture + OCR the active window, on demand only.
/// Nothing screen-derived is persisted; the result exists solely in this
/// response.
async fn peek(State(state): State<AppState>) -> Response {
    let reader = state.screen_reader.clone();
    // Capture + OCR block for 100–500 ms; keep them off the async workers.
    let result = tokio::task::spawn_blocking(move || reader.capture_active_window(None)).await;
    match result {
        Ok(Ok(ctx)) => Json(ctx).into_response(),
        Ok(Err(e)) => {
            use auricle_vision::VisionError;
            let status = match e {
                // Transient desktop state: a retry can succeed.
                VisionError::NoWindow | VisionError::Minimized | VisionError::WindowGone => {
                    StatusCode::CONFLICT
                }
                // Machine-level: capture blocked or OCR pack absent.
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({"error": e.to_string()}))).into_response()
        }
        Err(join_err) => internal(format!("peek task failed: {join_err}")),
    }
}

fn not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": format!("no session \"{id}\"")})),
    )
        .into_response()
}

pub(crate) fn internal(msg: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": msg})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_authorities_are_recognized() {
        for ok in [
            "127.0.0.1:4820",
            "127.0.0.1",
            "localhost:4820",
            "LOCALHOST",
            "[::1]:4820",
            "::1",
            "127.0.0.53",
        ] {
            assert!(authority_is_loopback(ok), "{ok} should be loopback");
        }
        for bad in [
            "evil.com",
            "evil.com:4820",
            "192.168.1.10:4820",
            "127.0.0.1.evil.com", // rebinding-style tricks
            "[2001:db8::1]:4820",
            "",
        ] {
            assert!(!authority_is_loopback(bad), "{bad} must not be loopback");
        }
    }

    #[test]
    fn origin_policy_same_origin_or_local() {
        let host = Some("127.0.0.1:4820");
        // Same-origin (the bundled UI) and local dev pages are fine.
        assert!(origin_allowed("http://127.0.0.1:4820", host));
        assert!(origin_allowed("http://localhost:5173", host)); // vite dev
        assert!(origin_allowed("http://[::1]:4820", host));
        // Cross-site pages are exactly what the check exists to stop.
        assert!(!origin_allowed("https://evil.com", host));
        assert!(!origin_allowed("http://evil.com:4820", host));
        assert!(!origin_allowed("null", host));
        assert!(!origin_allowed("file://evil", host));
        // Non-loopback origin is accepted only when truly same-origin
        // (token-protected remote binds serve the UI from their own host).
        assert!(origin_allowed("http://myhost:9000", Some("myhost:9000")));
        assert!(!origin_allowed(
            "http://myhost:9000",
            Some("otherhost:9000")
        ));
    }
}
