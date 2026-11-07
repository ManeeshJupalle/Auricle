//! `/ws/live`: pushes partial/final/lifecycle/error events as JSON.
//!
//! Three broadcast lanes implement the slow-consumer policy: finals and
//! lifecycle events ride a deep channel (a laggard would have to fall a
//! whole meeting behind to lose one), partials ride a shallow lossy
//! channel, and streamed answer fragments get a lane of their own so a long
//! answer can't use up the slots finals wait in. When a consumer lags a lossy
//! lane, the dropped counts are reported to it as a `lag` event — partials
//! and fragments are shed first, so finals are the last thing to go.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use tokio::sync::broadcast::error::RecvError;

use crate::api::AppState;
use crate::events::WsEvent;

/// A long answer runs to thousands of fragments, so this is deep enough that
/// only a stalled consumer loses any.
pub(crate) const ANSWER_CAPACITY: usize = 1024;

pub async fn ws_handler(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    upgrade.on_upgrade(move |socket| connection(socket, state))
}

async fn connection(mut socket: WebSocket, state: AppState) {
    let mut important = state.engine.subscribe_important();
    let mut partials = state.engine.subscribe_partials();
    let mut answers = state.answers.subscribe();
    let mut dropped_partials: u64 = 0;
    let mut dropped_answer_deltas: u64 = 0;

    loop {
        tokio::select! {
            event = important.recv() => match event {
                Ok(ev) => {
                    // An ask's end rides this lane so that it can't be shed,
                    // but the fragments it ends ride another: send what is
                    // queued there first, or it could overtake them.
                    if matches!(ev, WsEvent::AnswerDone { .. } | WsEvent::AskError { .. }) {
                        while let Ok(delta) = answers.try_recv() {
                            if socket.send(Message::text(delta.to_json())).await.is_err() {
                                return;
                            }
                        }
                    }
                    // Likewise a partial being withdrawn: arriving ahead of it
                    // would leave a row nothing else clears.
                    if matches!(ev, WsEvent::PartialCleared { .. }) {
                        while let Ok(partial) = partials.try_recv() {
                            if socket.send(Message::text(partial.to_json())).await.is_err() {
                                return;
                            }
                        }
                    }
                    if socket.send(Message::text(ev.to_json())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Deep channel: reaching this means the consumer fell
                    // >1024 events behind. Surface it honestly.
                    let ev = WsEvent::Error {
                        session: String::new(),
                        message: format!("event stream lagged; {n} events missed"),
                    };
                    if socket.send(Message::text(ev.to_json())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
            event = partials.recv() => match event {
                Ok(ev) => {
                    if socket.send(Message::text(ev.to_json())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // By design: slow consumers shed partials, never finals.
                    dropped_partials += n;
                    let ev = WsEvent::Lag { dropped_partials, dropped_answer_deltas };
                    if socket.send(Message::text(ev.to_json())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
            event = answers.recv() => match event {
                Ok(ev) => {
                    if socket.send(Message::text(ev.to_json())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // The answer on this socket now has holes in it; the
                    // ask's own SSE response is still complete.
                    dropped_answer_deltas += n;
                    let ev = WsEvent::Lag { dropped_partials, dropped_answer_deltas };
                    if socket.send(Message::text(ev.to_json())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                // Client messages are ignored (the socket is push-only),
                // but recv() is how disconnects surface.
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
        }
    }
}
