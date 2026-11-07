//! REST surface (architecture §4.8) + server bootstrap.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use auricle_core::{Config, Error, Result};
use axum::extract::{MatchedPath, Path, Query, State};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::{broadcast, Notify};

use crate::engine::{
    DictationError, DictationParams, Engine, EngineOptions, StartError, StartParams, StopError,
};
use crate::events::WsEvent;
use crate::export::render_markdown_full;
use crate::store::{SegmentRow, SessionPage, SessionRow, Store, SummaryRow};

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    /// Bearer token required on every route (None on localhost binds).
    pub token: Option<Arc<String>>,
    /// On-demand screen capture + OCR for /peek and /ask (mocked in tests).
    pub screen_reader: Arc<dyn auricle_vision::ScreenReader>,
    /// Rolling in-memory window of final segments feeding /ask.
    pub(crate) ring: Arc<crate::ring::TranscriptRing>,
    /// In-memory follow-up memory for /ask.
    pub(crate) ask_history: Arc<crate::ask::AskHistory>,
    /// Streamed answer fragments for /ws/live. Kept off the engine's
    /// important lane so a long answer can't use up the slots a slow
    /// consumer's finals wait in.
    pub(crate) answers: broadcast::Sender<WsEvent>,
    /// Fired once `POST /shutdown` has stopped the active session. `serve`
    /// exits on it; a router built without `serve` has nothing listening.
    shutdown: Arc<Notify>,
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
    router(engine, token, screen_reader, Arc::new(Notify::new()))
}

fn router(
    engine: Arc<Engine>,
    token: Option<String>,
    screen_reader: Arc<dyn auricle_vision::ScreenReader>,
    shutdown: Arc<Notify>,
) -> Router {
    let ring = engine.ring();
    ring.spawn_feeder(engine.subscribe_important());
    let (answers, _) = broadcast::channel(crate::ws::ANSWER_CAPACITY);
    let state = AppState {
        ask_history: engine.ask_history(),
        engine,
        token: token.map(Arc::new),
        screen_reader,
        ring,
        answers,
        shutdown,
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
        .route("/api/v1/dictation/start", post(start_dictation))
        .route("/api/v1/dictation/stop", post(stop_dictation))
        .route("/api/v1/dictation/cancel", post(cancel_dictation))
        .route("/api/v1/settings", get(get_settings).put(put_settings))
        .route(
            "/api/v1/secrets/{provider}",
            put(put_secret).delete(delete_secret),
        )
        .route("/api/v1/search", get(search_lines))
        .route("/api/v1/egress", get(egress))
        .route("/api/v1/diagnostics", get(diagnostics))
        .route("/api/v1/peek", post(peek))
        .route("/api/v1/ask", post(crate::ask::ask))
        .route("/api/v1/asks", get(list_asks))
        .route("/api/v1/asks/{id}", delete(delete_ask))
        .route("/api/v1/backup", post(backup))
        .route("/api/v1/shutdown", post(request_shutdown))
        .route("/ws/live", get(crate::ws::ws_handler))
        // MCP (Streamable HTTP) for local agents. Merged as its own router so
        // the enable gate wraps only this path; the global auth layer below
        // still applies, so /mcp gets the same origin and loopback checks as
        // everything else.
        .merge(
            Router::new()
                .nest_service("/mcp", crate::mcp::service(state.clone()))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    mcp_gate,
                )),
        )
        // Covers every route above by construction, so a route added later
        // cannot be left outside it. Only the fallback below escapes.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            token_middleware,
        ))
        // Embedded web UI (ui/dist via rust-embed): everything that isn't
        // an API route serves static files, unknown paths fall back to
        // index.html (SPA).
        .fallback(static_handler)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(axum::middleware::map_response(security_headers))
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

/// Everything the dashboard loads is same-origin: its script, stylesheet and
/// fonts, `/api` fetches, retained audio, and the `/ws/live` socket (`'self'`
/// covers ws: on the same host and port). React applies `style` props through
/// the CSSOM, which `style-src` does not govern, so no `'unsafe-inline'`.
/// `data:` is for whatever Vite decides is small enough to inline.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data:; \
     font-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'self'; \
     frame-ancestors 'none'";

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    // An <audio> or <img> on a foreign page sends no Origin, so the origin
    // check can't see it; without this, such a page could play retained audio
    // from a tokenless daemon.
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    response
}

/// How long `POST /shutdown` waits on the active session. The engine bounds
/// its own stop, so this only matters if that stop wedges, and an HTTP caller
/// has no second Ctrl+C to force the issue.
const SHUTDOWN_STOP_TIMEOUT: Duration = Duration::from_secs(150);
/// How long `POST /shutdown` lets a post-stop auto-title run. Under the LLM's
/// own 20 s because the overlay gives the whole shutdown 30 s; a title that
/// misses this leaves the placeholder, which the user can still rename.
const SHUTDOWN_TITLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Head start the 202 gets before `serve` returns and the process ends.
const SHUTDOWN_FLUSH: Duration = Duration::from_millis(100);
/// Time between retention sweeps, after the one at startup.
const RETENTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Run the daemon: bind, open the store, serve until asked to stop.
pub async fn serve(cfg: Config, data_root: PathBuf, bind_override: Option<String>) -> Result<()> {
    // Capture panics (this thread and spawned tasks) to a local file for
    // diagnostics. Local only — nothing is sent anywhere.
    crate::diagnostics::install_panic_hook(data_root.clone());

    let bind = bind_override.unwrap_or_else(|| cfg.server.bind.clone());
    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| Error::Config(format!("invalid bind address \"{bind}\": {e}")))?;

    // Bearer token: only demanded when exposed beyond localhost.
    let token = if addr.ip().is_loopback() {
        None
    } else {
        let var = cfg.server.token_env.clone();
        Some(auricle_core::secret_lookup(&var).ok_or_else(|| {
            Error::Config(format!(
                "binding to non-localhost {addr} requires a bearer token in {var} \
                 (environment or credential store)"
            ))
        })?)
    };

    // Bind before touching the database, so a port clash fails with nothing
    // disturbed. A second daemon on another port gets past this, and is
    // refused by the data root's lock in Engine::new.
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(Error::Io)?;

    let engine = Engine::new(EngineOptions {
        cfg,
        data_root,
        provider_override: None,
    })?;
    serve_engine(engine, listener, token).await
}

/// Everything `serve` does once it has an engine and a port: answer requests
/// and sweep retention until asked to stop. Exposed for the integration tests,
/// which inject a fake provider through the engine options.
pub async fn serve_engine(
    engine: Arc<Engine>,
    listener: tokio::net::TcpListener,
    token: Option<String>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    let shutdown = Arc::new(Notify::new());
    let app = router(
        engine.clone(),
        token,
        Arc::new(auricle_vision::WindowsOcrReader::new()),
        shutdown.clone(),
    );

    eprintln!(
        "auricle listening on http://{addr} (data: {})",
        engine.data_root().display()
    );
    // Run until asked to stop, then stop any active session cleanly before
    // exiting — an intentional shutdown must not be recorded as an
    // interrupted session. (No graceful connection drain: open WebSockets
    // would hold the process forever; clients auto-reconnect.)
    tokio::select! {
        result = axum::serve(listener, app) => result.map_err(Error::Io),
        _ = retention_loop(engine.clone()) => Ok(()),
        _ = shutdown_on_signal(engine) => Ok(()),
        _ = shutdown.notified() => Ok(()),
    }
}

/// Apply the retention setting now and then daily. It is read on every sweep,
/// so a change takes effect from the next one. Logs counts and nothing else:
/// which meetings went is the user's business, not a diagnostic.
async fn retention_loop(engine: Arc<Engine>) {
    let mut daily = tokio::time::interval(RETENTION_INTERVAL);
    loop {
        daily.tick().await; // the first tick is immediate: the startup sweep
        let engine = engine.clone();
        match off_runtime(move || engine.sweep_retention()).await {
            Ok(sweep) => {
                if sweep.deleted > 0 {
                    eprintln!("retention: deleted {} expired session(s)", sweep.deleted);
                }
                if let Some(notice) = sweep.notice {
                    eprintln!("{notice}");
                }
            }
            Err(e) => eprintln!("retention sweep failed: {e}"),
        }
    }
}

async fn shutdown_on_signal(engine: Arc<Engine>) {
    stop_signal().await;
    engine.begin_shutdown();
    if let Some(id) = engine.active_session() {
        eprintln!("\nstopping active session {id} (Ctrl+C again to force quit) ...");
        tokio::select! {
            _ = stop_and_wait(&engine, &id) => {}
            _ = stop_signal() => eprintln!("force quit; session left for crash recovery"),
        }
    }
    eprintln!("auricle stopped");
}

/// Begin stopping `id`, then wait for the engine to finish persisting it.
pub(crate) async fn stop_and_wait(engine: &Engine, id: &str) {
    let _ = engine.stop_session(id);
    while engine.active_session().as_deref() == Some(id) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stop `id` on the way out, within `SHUTDOWN_STOP_TIMEOUT`. A stop cannot
/// interrupt a start, and a start can be minutes into a model download. Until
/// it has opened capture it has recorded nothing, so nothing is worth the wait.
async fn stop_for_exit(engine: &Engine, id: &str) {
    let _ = engine.stop_session(id);
    if !engine.capture_began(id) {
        return;
    }
    let stopped = stop_and_wait(engine, id);
    if tokio::time::timeout(SHUTDOWN_STOP_TIMEOUT, stopped)
        .await
        .is_err()
    {
        eprintln!("session did not stop in time; left for crash recovery");
    }
}

/// Resolves when the user or Windows asks the daemon to stop. A source whose
/// handler can't be installed never resolves: run until killed.
async fn stop_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(windows)]
    let session_ending = async {
        if windows_session_ending().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(windows))]
    let session_ending = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = session_ending => {}
    }
}

/// The console closing, the user logging off, or the machine shutting down.
/// Tokio parks Windows' handler thread for these, so the process lives until
/// it exits by itself or Windows' deadline (about 5 s) passes. A stop that
/// misses the deadline is left for crash recovery, like any other kill.
///
/// Windows sends logoff and shutdown only to services: a daemon started from
/// a console or by the overlay is terminated at logoff without hearing either.
/// Those two matter only when running as a service.
#[cfg(windows)]
async fn windows_session_ending() -> std::io::Result<()> {
    use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
    let (mut close, mut logoff, mut shutdown) = (ctrl_close()?, ctrl_logoff()?, ctrl_shutdown()?);
    tokio::select! {
        _ = close.recv() => {}
        _ = logoff.recv() => {}
        _ = shutdown.recv() => {}
    }
    Ok(())
}

/// POST /api/v1/shutdown — stop any active session cleanly, then exit, for
/// callers that need the daemon gone (the overlay, before an update). Answers
/// at once because the stop lasts as long as the final transcription does;
/// the caller sees completion as the port closing.
async fn request_shutdown(State(state): State<AppState>) -> Response {
    // Before the reply: whoever hears 202 must not be able to start a session.
    state.engine.begin_shutdown();
    let engine = state.engine.clone();
    let exit = state.shutdown.clone();
    tokio::spawn(async move {
        if let Some(id) = engine.active_session() {
            eprintln!("stopping active session {id} ...");
            stop_for_exit(&engine, &id).await;
        }
        // A title is generated once and never retried, so exiting mid-call
        // would leave the placeholder on this session for good.
        let titled = engine.titles_settled();
        if tokio::time::timeout(SHUTDOWN_TITLE_TIMEOUT, titled)
            .await
            .is_err()
        {
            eprintln!("auto-title did not finish in time; keeping the placeholder");
        }
        tokio::time::sleep(SHUTDOWN_FLUSH).await;
        eprintln!("auricle stopped");
        exit.notify_one();
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "shutting_down"})),
    )
        .into_response()
}

/// Run store work on the blocking pool, the way `POST /peek` runs capture.
/// These reads are unbounded and wait on the store mutex: inline, they would
/// occupy a Tokio worker the capture pipeline needs to persist segments.
pub(crate) async fn off_runtime<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(format!("store task failed: {e}"))))?
}

/// Answer 404 on every `/mcp` request while the MCP server is switched off,
/// so a disabled endpoint is indistinguishable from an absent one.
///
/// The setting is read per request rather than at router build time: the
/// toggle in Settings then takes effect immediately instead of at the next
/// restart. The settings table is a handful of rows, and MCP is not a hot
/// path, so the read is not worth caching, but it does take the store mutex,
/// so it runs off the async workers. A read that fails leaves MCP off.
async fn mcp_gate(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let store = state.engine.store();
    let enabled = off_runtime(move || store.all_settings())
        .await
        .ok()
        .and_then(|s| s.get("mcp_enabled").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    if !enabled {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    next.run(request).await
}

async fn auth_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Browser-attached Origins must be same-origin. CORS does not protect
    // WebSocket handshakes: without this, any website could open
    // ws://127.0.0.1:4820/ws/live and read meetings live. Nor does it stop a
    // POST with no body, which needs no preflight, from ending the daemon.
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
    next.run(request).await
}

/// Routes where the token may ride in `?token=`, because a browser cannot set
/// a header on a WebSocket handshake or an `<audio>` request. A URL turns up in
/// more places than a header does, so nowhere else.
const QUERY_TOKEN_ROUTES: [&str; 2] = ["/ws/live", "/api/v1/sessions/{id}/audio/{channel}"];

#[derive(serde::Deserialize)]
struct TokenQuery {
    token: String,
}

/// The `?token=` on a request to one of `QUERY_TOKEN_ROUTES`. Goes by the
/// route that matched, not the path as sent, so no spelling of a URL can reach
/// the allowance from another route.
fn query_token(request: &axum::extract::Request) -> Option<String> {
    let route = request.extensions().get::<MatchedPath>()?;
    if !QUERY_TOKEN_ROUTES.contains(&route.as_str()) {
        return None;
    }
    let Query(query) = Query::<TokenQuery>::try_from_uri(request.uri()).ok()?;
    Some(query.token)
}

/// Demand the token on every route, as a bearer header or where
/// `QUERY_TOKEN_ROUTES` allows it. The embedded UI's static files are served
/// by the router's fallback, which this layer does not wrap: they hold no user
/// data, and a browser cannot attach a header to a page load, so the dashboard
/// could never start on a token-protected bind otherwise.
async fn token_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(expected) = &state.token {
        let matches = |t: &str| constant_time_eq(t.as_bytes(), expected.as_bytes());
        let ok = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(matches)
            || query_token(&request).is_some_and(|t| matches(&t));
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

/// Constant-time byte comparison for the bearer token. Token-gated binds
/// are remote by definition, and a plain `==` short-circuits at the first
/// mismatching byte — response timing would leak how much of the token a
/// caller has guessed. Delegates to `subtle`, whose `ct_eq` carries an
/// optimization barrier: a hand-rolled fold can be legally rewritten by
/// the optimizer into an early-exit compare, reintroducing the leak.
/// (Length still short-circuits: token length is not treated as secret.)
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

/// An Origin is acceptable only when it is same-origin with the request's
/// Host: a page on another localhost port gets no more than any other site.
/// `npm run dev` needs no exception, because Vite's `/api` and `/ws` proxies
/// keep the browser's Host, so its origin arrives same-origin.
fn origin_allowed(origin: &str, host: Option<&str>) -> bool {
    origin_authority(origin) // None: "null", file://, or malformed: no way to vouch
        .zip(host)
        .is_some_and(|(authority, host)| authority.eq_ignore_ascii_case(host))
}

fn origin_authority(origin: &str) -> Option<&str> {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
}

/// True when a `host[:port]` authority is localhost or a loopback IP
/// (handles `127.0.0.1:4820`, `localhost`, `[::1]:4820`, raw `::1`).
pub(crate) fn authority_is_loopback(authority: &str) -> bool {
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
        "pid": std::process::id(),
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
    let store = state.engine.store();
    overlay_llm_settings(&mut cfg, &store);
    // The provider a start with no choice of its own would use.
    if let Ok(settings) = store.all_settings() {
        if let Some(p) = settings.get("default_provider").and_then(|v| v.as_str()) {
            cfg.stt.provider = p.to_string();
        }
    }
    let cfg = &cfg;
    let data_root = state.engine.data_root();
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

/// Map a public secret id to the config-declared name it is keyed under.
/// Both the environment and the OS credential store use this name, so one
/// entry serves every consumer (e.g. `groq` covers STT and LLM alike).
/// Returns `None` for an unknown id.
fn secret_name_for(cfg: &Config, id: &str) -> Option<String> {
    let name = match id {
        "deepgram" => &cfg.stt.deepgram.api_key_env,
        "groq" => &cfg.llm.groq.api_key_env,
        "openai" => &cfg.llm.openai_compat.api_key_env,
        "token" => &cfg.server.token_env,
        _ => return None,
    };
    Some(name.clone())
}

#[derive(serde::Deserialize)]
struct SecretBody {
    value: String,
}

/// Store an API key / token in the OS credential store. The value is
/// written to the credential store only — never to config, the database,
/// or the response (which reports presence, not the secret).
async fn put_secret(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SecretBody>,
) -> Response {
    let Some(name) = secret_name_for(state.engine.config(), &id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown secret \"{id}\"")})),
        )
            .into_response();
    };
    if body.value.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "value must not be empty"})),
        )
            .into_response();
    }
    match auricle_core::secret_set(&name, &body.value) {
        Ok(()) => Json(json!({"id": id, "stored": true})).into_response(),
        Err(e) => internal(format!("credential store: {e}")),
    }
}

/// Remove a stored secret from the OS credential store (idempotent). Does
/// not clear an env-var-provided key; readiness reflects both sources.
async fn delete_secret(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(name) = secret_name_for(state.engine.config(), &id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown secret \"{id}\"")})),
        )
            .into_response();
    };
    match auricle_core::secret_delete(&name) {
        Ok(()) => Json(json!({"id": id, "deleted": true})).into_response(),
        Err(e) => internal(format!("credential store: {e}")),
    }
}

/// `limit` and `offset` as a query string gave them, in the range every
/// paged route accepts.
fn page_bounds(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (
        limit.unwrap_or(200).clamp(1, 1000),
        offset.unwrap_or(0).max(0),
    )
}

#[derive(serde::Deserialize)]
struct EgressQuery {
    session: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// GET /api/v1/egress — the egress ledger: what user data left the machine
/// (and what explicitly stayed local), newest first. Metadata only; the
/// audio, prompt, and answer are never recorded, only where they went.
async fn egress(State(state): State<AppState>, Query(q): Query<EgressQuery>) -> Response {
    let (limit, offset) = page_bounds(q.limit, q.offset);
    let store = state.engine.store();
    // `entries` is one page; `totals` covers the whole ledger, so the UI's
    // "nothing has left this machine" line never becomes an artifact of how
    // many rows fit on a page.
    let ledger = off_runtime(move || {
        Ok((
            store.list_egress(q.session.as_deref(), limit, offset)?,
            store.egress_totals()?,
        ))
    })
    .await;
    match ledger {
        Ok((entries, totals)) => {
            Json(json!({ "entries": entries, "totals": totals })).into_response()
        }
        Err(e) => internal(e.to_string()),
    }
}

/// GET /api/v1/diagnostics — build/runtime info plus recent crash records
/// captured locally. Read-only; a user copies this into a bug report. No
/// user content is ever included, and nothing is sent anywhere.
async fn diagnostics(State(state): State<AppState>) -> Response {
    let dir = state.engine.data_root();
    Json(json!({
        "system": crate::diagnostics::system_info(),
        "crashes": crate::diagnostics::recent_crashes(dir, 20),
    }))
    .into_response()
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
    let loaded = {
        let (store, id) = (store.clone(), id.clone());
        off_runtime(move || Ok((store.get_session(&id)?, store.get_segments(&id)?))).await
    };
    let (session, segments) = match loaded {
        Ok((Some(session), segments)) => (session, segments),
        Ok((None, _)) => return not_found(&id),
        Err(e) => return internal(e.to_string()),
    };
    if segments.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "session has no transcript to summarize"})),
        )
            .into_response();
    }

    let templates_dir = state.engine.data_root().join("templates");
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
    // Egress ledger: the transcript text goes to the summarizing LLM (or
    // stays local). Metadata only — the transcript itself is not recorded.
    crate::egress::record(
        &store,
        Some(id.as_str()),
        "summary",
        crate::egress::llm_egress(&provider_id, &cfg),
        &provider_id,
        Some(transcript.chars().count() as i64),
        Some(&format!("summary: {template_name}")),
    );
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

    let created_at = crate::engine::unix_secs();
    let summary_id = match store.insert_summary(
        &session.id,
        &template_name,
        provider.model(),
        &content,
        created_at,
    ) {
        // Deleted during the LLM call: nothing was written, and 201 with
        // id 0 would claim a summary that does not exist.
        Ok(0) => return not_found(&id),
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
        Err(StartError::Dictating) => {
            conflict("a dictation is in progress; finish it before starting a meeting")
        }
        Err(StartError::Invalid(msg)) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
        }
        Err(e @ StartError::Stopped) => {
            (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response()
        }
        Err(e @ StartError::ShuttingDown) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(StartError::Internal(e)) => internal(e.to_string()),
    }
}

/// POST /api/v1/dictation/start — the body (`{"language": "en"}`) is optional.
async fn start_dictation(
    State(state): State<AppState>,
    body: Option<Json<DictationParams>>,
) -> Response {
    let params = body.map(|Json(params)| params).unwrap_or_default();
    match state.engine.start_dictation(params).await {
        Ok(started) => (
            StatusCode::CREATED,
            Json(json!({
                "id": started.id,
                "provider": started.provider,
                "local": started.local,
            })),
        )
            .into_response(),
        Err(StartError::Busy(_)) => {
            conflict("a meeting is recording; dictation can start once it stops")
        }
        Err(StartError::Dictating) => conflict("a dictation is already active"),
        Err(StartError::Stopped) => conflict("the dictation was stopped before it began"),
        Err(StartError::Invalid(msg)) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
        }
        Err(e @ StartError::ShuttingDown) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(StartError::Internal(e)) => internal(e.to_string()),
    }
}

/// POST /api/v1/dictation/stop — ends the dictation and answers with its text,
/// once the last chunk has been transcribed.
async fn stop_dictation(State(state): State<AppState>) -> Response {
    match state.engine.stop_dictation().await {
        Ok(dictated) => Json(json!({
            "id": dictated.id,
            "text": dictated.text,
            "duration_ms": dictated.duration_ms,
        }))
        .into_response(),
        Err(e) => dictation_error(e),
    }
}

/// POST /api/v1/dictation/cancel — ends the dictation and discards its text.
async fn cancel_dictation(State(state): State<AppState>) -> Response {
    match state.engine.cancel_dictation().await {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(e) => dictation_error(e),
    }
}

fn dictation_error(e: DictationError) -> Response {
    let (status, message) = match e {
        DictationError::NotActive => (StatusCode::NOT_FOUND, "no dictation is active".to_string()),
        DictationError::Failed(message) => (StatusCode::BAD_GATEWAY, message),
        DictationError::TimedOut => (
            StatusCode::GATEWAY_TIMEOUT,
            "the dictation did not finish in time".to_string(),
        ),
    };
    (status, Json(json!({"error": message}))).into_response()
}

fn conflict(message: &str) -> Response {
    (StatusCode::CONFLICT, Json(json!({"error": message}))).into_response()
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
    limit: Option<i64>,
    offset: Option<i64>,
}

/// GET /api/v1/sessions — newest first, every match unless `limit` or `offset`
/// asks for a page. `total` counts every match either way.
async fn list_sessions(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let store = state.engine.store();
    let page = off_runtime(move || {
        if q.limit.is_none() && q.offset.is_none() {
            let sessions = store.list_sessions(q.q.as_deref())?;
            let total = sessions.len() as i64;
            return Ok(SessionPage { sessions, total });
        }
        let (limit, offset) = page_bounds(q.limit, q.offset);
        store.find_sessions_page(q.q.as_deref(), limit, offset)
    })
    .await;
    match page {
        Ok(page) => Json(page).into_response(),
        Err(e) => internal(e.to_string()),
    }
}

/// Matching lines across every meeting. `?q=` on `/sessions` says which
/// meetings mention something; this says where, so a person can go to the
/// moment rather than reread the meeting.
async fn search_lines(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let (limit, offset) = page_bounds(q.limit, q.offset);
    let query = q.q.unwrap_or_default();
    let store = state.engine.store();
    match off_runtime(move || store.search_segments_page(&query, limit, offset)).await {
        Ok(page) => Json(page).into_response(),
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
    // A user-chosen name is permanent: clear the auto-title flag so the
    // post-stop generator never overwrites it. Before the rename, not after:
    // the generator writes only while the flag is set, so whichever way the
    // two interleave, this name is the one left standing.
    if let Ok(Some(session)) = store.get_session(&id) {
        if session.meta["auto_title"] == serde_json::Value::Bool(true) {
            let _ = store.set_session_meta(&id, "auto_title", &json!(false));
        }
    }
    match store.rename_session(&id, title) {
        Ok(true) => Json(json!({"id": id, "title": title})).into_response(),
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
    let engine = state.engine.clone();
    let target = id.clone();
    let deleted = off_runtime(move || engine.delete_session(&target)).await;
    match deleted {
        Ok(true) => Json(json!({"id": id, "deleted": true})).into_response(),
        Ok(false) => not_found(&id),
        Err(e) => internal(e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct AsksQuery {
    session: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// GET /api/v1/asks — asks kept under `copilot.retain_context`, newest first.
async fn list_asks(State(state): State<AppState>, Query(q): Query<AsksQuery>) -> Response {
    let (limit, offset) = page_bounds(q.limit, q.offset);
    let store = state.engine.store();
    match off_runtime(move || store.list_asks(q.session.as_deref(), limit, offset)).await {
        Ok(asks) => Json(json!({"asks": asks})).into_response(),
        Err(e) => internal(e.to_string()),
    }
}

async fn delete_ask(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let store = state.engine.store();
    match off_runtime(move || store.delete_ask(id)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("no ask {id}")})),
        )
            .into_response(),
        Err(e) => internal(e.to_string()),
    }
}

/// POST /api/v1/backup — write a consistent copy of the database under the
/// data root. Recording and reads carry on while it runs.
async fn backup(State(state): State<AppState>) -> Response {
    let dir = state.engine.data_root().join("backups");
    let path = dir.join(format!("auricle-{}.db", crate::engine::unix_secs()));
    let store = state.engine.store();
    let target = path.clone();
    let written = off_runtime(move || {
        std::fs::create_dir_all(dir)?;
        store.backup_to(&target)
    })
    .await;
    match written {
        Ok(()) => (
            StatusCode::CREATED,
            Json(json!({"path": path.display().to_string()})),
        )
            .into_response(),
        // Names are per second, and a backup never overwrites.
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => (
            StatusCode::CONFLICT,
            Json(json!({"error": "a backup was taken this second; try again"})),
        )
            .into_response(),
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
    // Defense-in-depth: the path was written by this engine, but a
    // tampered database must not turn this endpoint into an arbitrary
    // file server. Canonicalize (resolving `..` and symlinks) and require
    // the file to live under this engine's sessions directory. Serve the
    // *canonicalized* path, not the raw one, so the file that is opened is
    // the file that was checked (a symlink swapped after the check cannot
    // redirect the open).
    let sessions_root = state.engine.data_root().join("sessions");
    let served = match (
        tokio::fs::canonicalize(path).await,
        tokio::fs::canonicalize(&sessions_root).await,
    ) {
        (Ok(file), Ok(root)) if file.starts_with(&root) => file,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "retained audio file is missing on disk"})),
            )
                .into_response();
        }
    };
    match tower::ServiceExt::oneshot(tower_http::services::ServeFile::new(&served), request).await {
        Ok(resp) => resp.into_response(),
        Err(infallible) => match infallible {},
    }
}

type SessionDetail = (SessionRow, Vec<SegmentRow>, Vec<SummaryRow>);

/// A session with its transcript and summaries; None when the id is unknown.
fn load_session(store: &Store, id: &str) -> Result<Option<SessionDetail>> {
    let Some(session) = store.get_session(id)? else {
        return Ok(None);
    };
    Ok(Some((
        session,
        store.get_segments(id)?,
        store.get_summaries(id)?,
    )))
}

async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.engine.store();
    let target = id.clone();
    match off_runtime(move || load_session(&store, &target)).await {
        Ok(Some((session, segments, summaries))) => Json(json!({
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
        Ok(None) => not_found(&id),
        Err(e) => internal(e.to_string()),
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
    let target = id.clone();
    let rendered = off_runtime(move || {
        Ok(
            load_session(&store, &target)?.map(|(session, segments, summaries)| {
                render_markdown_full(&session, &segments, &summaries)
            }),
        )
    })
    .await;
    match rendered {
        Ok(Some(md)) => ([("content-type", "text/markdown; charset=utf-8")], md).into_response(),
        Ok(None) => not_found(&id),
        Err(e) => internal(e.to_string()),
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
    // This one deletes meetings: a value the sweep would ignore must not look
    // like it was accepted.
    if obj
        .get("retention_days")
        .is_some_and(|days| days.as_i64().is_none_or(|days| days < 0))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "retention_days must be a whole number of days, 0 for off"})),
        )
            .into_response();
    }
    // Ends up in a provider's URL, and a bad value would otherwise surface
    // only as a failed Start in the middle of someone's meeting.
    for key in ["stt_language", "dictation_language"] {
        if obj.get(key).is_some_and(|language| {
            language
                .as_str()
                .is_none_or(|tag| auricle_stt::validate_language(tag).is_err())
        }) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("{key} must be \"auto\" or a tag like en or pt-BR")})),
            )
                .into_response();
        }
    }
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
    fn constant_time_eq_matches_semantics_of_eq() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeX"));
        assert!(!constant_time_eq(b"secret-token", b"Xecret-token"));
        assert!(!constant_time_eq(b"short", b"longer-token"));
        assert!(constant_time_eq(b"", b""));
    }

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
    fn origin_policy_is_same_origin_only() {
        let host = Some("127.0.0.1:4820");
        // The bundled UI, and `npm run dev` with the Host Vite's proxy keeps.
        assert!(origin_allowed("http://127.0.0.1:4820", host));
        assert!(origin_allowed(
            "http://localhost:5173",
            Some("localhost:5173")
        ));
        // Another page on this machine is no more trusted than a website.
        assert!(!origin_allowed("http://localhost:5173", host));
        assert!(!origin_allowed("http://[::1]:4820", host));
        // Cross-site pages are exactly what the check exists to stop.
        assert!(!origin_allowed("https://evil.com", host));
        assert!(!origin_allowed("http://evil.com:4820", host));
        assert!(!origin_allowed("null", host));
        assert!(!origin_allowed("file://evil", host));
        assert!(!origin_allowed("http://127.0.0.1:4820", None));
        // Token-protected remote binds serve the UI from their own host.
        assert!(origin_allowed("http://myhost:9000", Some("myhost:9000")));
        assert!(!origin_allowed(
            "http://myhost:9000",
            Some("otherhost:9000")
        ));
    }
}
