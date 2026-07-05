//! `/ws/live`: pushes partial/final/lifecycle/error events as JSON.
//!
//! Two broadcast lanes implement the slow-consumer policy: finals and
//! lifecycle events ride a deep channel (a laggard would have to fall a
//! whole meeting behind to lose one), partials ride a shallow lossy
//! channel. When a consumer lags the partial lane, the dropped count is
//! reported to it as a `lag` event — partials are dropped, finals never.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use tokio::sync::broadcast::error::RecvError;

use crate::api::AppState;
use crate::events::WsEvent;

pub async fn ws_handler(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    upgrade.on_upgrade(move |socket| connection(socket, state))
}

async fn connection(mut socket: WebSocket, state: AppState) {
    let mut important = state.engine.subscribe_important();
    let mut partials = state.engine.subscribe_partials();
    let mut dropped_partials: u64 = 0;

    loop {
        tokio::select! {
            event = important.recv() => match event {
                Ok(ev) => {
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
                    let ev = WsEvent::Lag { dropped_partials };
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
