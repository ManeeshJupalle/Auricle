//! POST /api/v1/ask — the assistant service (COPILOT_ARCHITECTURE §2.2).
//!
//! Builds a prompt from question + on-demand screen OCR + the rolling
//! transcript window, streams the LLM answer as SSE on the response, and
//! mirrors every fragment as `answer_delta` / `answer_done` / `ask_error`
//! events on /ws/live. Ask history is in-memory only; a row reaches the
//! database only when `copilot.retain_context = true` (default off).

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::api::{internal, overlay_llm_settings, AppState};
use crate::assemble::{assemble_user_prompt, AskPromptParts, QaPair, TranscriptWindow};
use crate::engine::{unix_ms, unix_secs};
use crate::events::WsEvent;

/// Follow-up memory per history key (the ask's session id, or one global
/// bucket for session-less asks). In-memory only, bounded, process-lifetime.
#[derive(Default)]
pub struct AskHistory {
    inner: Mutex<HashMap<String, Vec<QaPair>>>,
}

/// Prior Q/A pairs kept per key; the assembler further narrows to the
/// newest few that fit the prompt budget.
const HISTORY_KEEP: usize = 8;
const GLOBAL_HISTORY_KEY: &str = "";

impl AskHistory {
    fn get(&self, key: &str) -> Vec<QaPair> {
        self.inner
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    fn push(&self, key: &str, pair: QaPair) {
        let mut inner = self.inner.lock().unwrap();
        let entries = inner.entry(key.to_string()).or_default();
        entries.push(pair);
        if entries.len() > HISTORY_KEEP {
            let excess = entries.len() - HISTORY_KEEP;
            entries.drain(..excess);
        }
    }
}

#[derive(serde::Deserialize)]
pub struct AskBody {
    question: String,
    #[serde(default)]
    include_screen: bool,
    #[serde(default)]
    include_transcript: bool,
    provider: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    follow_up: bool,
    /// PHASE-9: the overlay passes its own HWND so the capture never OCRs
    /// the copilot's previous answer. Plumbed through to auricle-vision;
    /// nothing sets it until the overlay exists.
    exclude_hwnd: Option<isize>,
}

pub async fn ask(State(state): State<AppState>, Json(body): Json<AskBody>) -> Response {
    let question = body.question.trim().to_string();
    if question.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "question must not be empty"})),
        )
            .into_response();
    }

    let mut cfg = state.engine.config().clone();
    let store = state.engine.store();
    overlay_llm_settings(&mut cfg, &store);
    // Provider precedence: request > [copilot].provider > settings/[llm].
    let provider_id = body
        .provider
        .clone()
        .or_else(|| cfg.copilot.provider.clone())
        .unwrap_or_else(|| cfg.llm.provider.clone());

    // No capability, no stream: a provider that cannot work right now
    // (missing key) is 409; a name that can never work is 400.
    if let Some(status) = auricle_llm::llm_provider_statuses(&cfg)
        .iter()
        .find(|s| s.id == provider_id)
    {
        if !status.ready {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("llm provider \"{provider_id}\" is not ready: {}", status.detail)
                })),
            )
                .into_response();
        }
    }
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

    let ask_id = format!("a{:x}", unix_ms());

    // Screen context: one frame, on demand, only because this request
    // asked for it. Failures are surfaced, not papered over — the /peek
    // status mapping applies (409 transient desktop state, 500 machine).
    let screen = if body.include_screen {
        let reader = state.screen_reader.clone();
        let exclude = body.exclude_hwnd;
        let result =
            tokio::task::spawn_blocking(move || reader.capture_active_window(exclude)).await;
        match result {
            Ok(Ok(ctx)) => Some(ctx),
            Ok(Err(e)) => {
                use auricle_vision::VisionError;
                let status = match e {
                    VisionError::NoWindow | VisionError::Minimized | VisionError::WindowGone => {
                        StatusCode::CONFLICT
                    }
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                let message = format!("screen capture failed: {e}");
                state.engine.publish_important(WsEvent::AskError {
                    ask_id,
                    message: message.clone(),
                });
                return (status, Json(json!({"error": message}))).into_response();
            }
            Err(join_err) => return internal(format!("screen capture task failed: {join_err}")),
        }
    } else {
        None
    };

    // Session metadata: explicit session id (404 when unknown), otherwise
    // whatever session is live right now, otherwise none.
    let resolved_session = match &body.session_id {
        Some(id) => match store.get_session(id) {
            Ok(Some(s)) => Some(s),
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("no session \"{id}\"")})),
                )
                    .into_response()
            }
            Err(e) => return internal(e.to_string()),
        },
        None => match state.engine.active_session() {
            Some(id) => store.get_session(&id).ok().flatten(),
            None => None,
        },
    };

    let history_key = body
        .session_id
        .clone()
        .unwrap_or_else(|| GLOBAL_HISTORY_KEY.to_string());
    let history = if body.follow_up {
        state.ask_history.get(&history_key)
    } else {
        Vec::new()
    };

    let templates_dir = crate::default_data_root()
        .unwrap_or_default()
        .join("templates");
    let system = match auricle_llm::load_template(&templates_dir, "copilot") {
        Ok(t) => t,
        Err(e) => return internal(format!("loading copilot template: {e}")),
    };

    let transcript_segments = body.include_transcript.then(|| state.ring.snapshot());
    let user_prompt = assemble_user_prompt(
        &AskPromptParts {
            question: &question,
            history: &history,
            screen: screen.as_ref(),
            transcript: transcript_segments
                .as_ref()
                .map(|segments| TranscriptWindow {
                    segments,
                    window_min: cfg.copilot.transcript_window_min,
                }),
            session_title: resolved_session.as_ref().map(|s| s.title.as_str()),
        },
        cfg.copilot.max_screen_chars,
    );

    // The SSE response streams from `event_rx`; the orchestrator below is
    // the single consumer of the provider's delta channel and fans out to
    // both surfaces, so a dropped SSE client never breaks the WS mirror.
    let (delta_tx, mut delta_rx) = tokio::sync::mpsc::channel::<String>(64);
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Event>(64);

    let started = json!({
        "type": "ask_started",
        "ask_id": ask_id,
        "provider": provider_id.as_str(),
        "model": provider.model(),
        "screen": screen.as_ref().map(|s| json!({
            "window_title": s.window_title,
            "app_name": s.app_name,
            "ocr_ms": s.ocr_ms,
        })),
        "transcript_segments": transcript_segments.as_ref().map(|s| s.len()),
    });
    let _ = event_tx.try_send(Event::default().data(started.to_string()));

    let llm_task = {
        let provider = provider.clone();
        let system = system.clone();
        let user_prompt = user_prompt.clone();
        tokio::spawn(async move { provider.chat_stream(&system, &user_prompt, delta_tx).await })
    };

    let engine = state.engine.clone();
    let ask_history = state.ask_history.clone();
    let retain = cfg.copilot.retain_context;
    let screen_json = screen.as_ref().and_then(|s| serde_json::to_string(s).ok());
    let row_session = resolved_session.map(|s| s.id);
    tokio::spawn(async move {
        let mut answer = String::new();
        while let Some(delta) = delta_rx.recv().await {
            answer.push_str(&delta);
            engine.publish_important(WsEvent::AnswerDelta {
                ask_id: ask_id.clone(),
                text: delta.clone(),
            });
            let frame = json!({"type": "answer_delta", "ask_id": ask_id, "text": delta});
            let _ = event_tx
                .send(Event::default().data(frame.to_string()))
                .await;
        }
        match llm_task.await {
            Ok(Ok(usage)) => {
                engine.publish_important(WsEvent::AnswerDone {
                    ask_id: ask_id.clone(),
                    usage: usage.clone(),
                });
                let frame = json!({"type": "answer_done", "ask_id": ask_id, "usage": usage});
                let _ = event_tx
                    .send(Event::default().data(frame.to_string()))
                    .await;
                ask_history.push(
                    &history_key,
                    QaPair {
                        question: question.clone(),
                        answer: answer.clone(),
                    },
                );
                if retain {
                    // The ONLY path that persists anything question- or
                    // screen-derived, and it is config-gated off by default.
                    if let Err(e) = engine.store().insert_ask(
                        row_session.as_deref(),
                        &question,
                        screen_json.as_deref(),
                        &answer,
                        &provider_id,
                        unix_secs(),
                    ) {
                        eprintln!("persisting ask: {e}");
                    }
                }
            }
            Ok(Err(e)) => {
                let message = e.to_string();
                engine.publish_important(WsEvent::AskError {
                    ask_id: ask_id.clone(),
                    message: message.clone(),
                });
                let frame = json!({"type": "ask_error", "ask_id": ask_id, "message": message});
                let _ = event_tx
                    .send(Event::default().data(frame.to_string()))
                    .await;
            }
            Err(join_err) => {
                let message = format!("ask stream task failed: {join_err}");
                engine.publish_important(WsEvent::AskError {
                    ask_id: ask_id.clone(),
                    message: message.clone(),
                });
                let frame = json!({"type": "ask_error", "ask_id": ask_id, "message": message});
                let _ = event_tx
                    .send(Event::default().data(frame.to_string()))
                    .await;
            }
        }
    });

    let stream = futures_util::stream::unfold(event_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|ev| (Ok::<_, std::convert::Infallible>(ev), rx))
    });
    // Keep-alive comments bridge long silent stretches (a local reasoning
    // model can think for tens of seconds before its first answer token).
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
