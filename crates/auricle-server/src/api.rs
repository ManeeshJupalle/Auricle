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

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    /// Bearer token required on every request (None on localhost binds).
    pub token: Option<Arc<String>>,
}

/// Build the router. Exposed for the integration tests, which bind an
/// ephemeral port and inject a fake provider through the engine options.
pub fn build_router(engine: Arc<Engine>, token: Option<String>) -> Router {
    let state = AppState {
        engine,
        token: token.map(Arc::new),
    };
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/providers", get(providers))
        .route("/api/v1/sessions", post(start_session).get(list_sessions))
        .route("/api/v1/sessions/{id}", get(get_session))
        .route("/api/v1/sessions/{id}/stop", post(stop_session))
        .route("/api/v1/sessions/{id}/export", get(export_session))
        .route("/api/v1/sessions/{id}/summarize", post(summarize_session))
        .route("/api/v1/settings", get(get_settings).put(put_settings))
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
    let app = build_router(engine, token);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(Error::Io)?;
    eprintln!(
        "auricle listening on http://{addr} (data: {})",
        data_root.display()
    );
    axum::serve(listener, app).await.map_err(Error::Io)
}

async fn auth_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
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

async fn providers(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut cfg = state.engine.config().clone();
    if let Ok(settings) = state.engine.store().all_settings() {
        if let Some(m) = settings.get("ollama_model").and_then(|v| v.as_str()) {
            cfg.llm.ollama.model = m.to_string();
        }
    }
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
    let provider_id = body.provider.unwrap_or_else(|| cfg.llm.provider.clone());

    let store = state.engine.store();
    // Settings overlay (mirrors the STT model overlay): lets the UI point
    // Ollama at a model the user actually has pulled.
    if let Ok(settings) = store.all_settings() {
        if let Some(m) = settings.get("ollama_model").and_then(|v| v.as_str()) {
            cfg.llm.ollama.model = m.to_string();
        }
    }
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

async fn list_sessions(State(state): State<AppState>) -> Response {
    match state.engine.store().list_sessions() {
        Ok(rows) => Json(json!({"sessions": rows})).into_response(),
        Err(e) => internal(e.to_string()),
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

fn not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": format!("no session \"{id}\"")})),
    )
        .into_response()
}

fn internal(msg: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": msg})),
    )
        .into_response()
}
