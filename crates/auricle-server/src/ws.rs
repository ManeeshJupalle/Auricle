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
                    // Likewise a partial being withdrawn, or made final: the
                    // partial arriving after it would leave a row nothing
                    // else clears.
                    if matches!(ev, WsEvent::PartialCleared { .. } | WsEvent::Final { .. }) {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message as Frame;

    use crate::engine::{Engine, EngineOptions};
    use crate::events::WsEvent;

    fn line(final_: bool, text: &str) -> WsEvent {
        let (session, channel, speaker) = ("s1".into(), "mic".into(), "You".into());
        if final_ {
            WsEvent::Final {
                session,
                channel,
                speaker,
                t_start_ms: 0,
                t_end_ms: 900,
                text: text.into(),
                latency_ms: 0,
            }
        } else {
            WsEvent::Partial {
                session,
                channel,
                speaker,
                t_start_ms: 0,
                t_end_ms: 900,
                text: text.into(),
            }
        }
    }

    /// A partial sent after its final makes a new row that nothing clears, so
    /// what was queued on the partial lane goes out first. Current-thread
    /// runtime: the burst below is all queued before the socket task runs,
    /// so its unbiased select is free to reach the final first.
    #[tokio::test]
    async fn no_partial_is_sent_after_the_final_that_ends_it() {
        let root = std::env::temp_dir().join(format!("auricle-ws-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let engine = Engine::new(EngineOptions {
            cfg: auricle_core::Config::default(),
            data_root: root,
            provider_override: None,
        })
        .unwrap();
        let app = crate::api::build_router(engine.clone(), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/live"))
            .await
            .unwrap();
        let (_, mut rx) = ws.split();
        let mut next_type = async move || loop {
            if let Some(Ok(Frame::Text(t))) = rx.next().await {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                return v["type"].as_str().unwrap().to_string();
            }
        };

        // The socket subscribes some time after the handshake: until it
        // hears the lane, what is sent there is lost.
        loop {
            engine.publish_important(WsEvent::Lag {
                dropped_partials: 0,
                dropped_answer_deltas: 0,
            });
            let heard = tokio::time::timeout(Duration::from_millis(50), next_type()).await;
            if heard.is_ok_and(|t| t == "lag") {
                break;
            }
        }
        // A second marker may still be on its way.
        tokio::time::sleep(Duration::from_millis(100)).await;
        while tokio::time::timeout(Duration::from_millis(20), next_type())
            .await
            .is_ok()
        {}

        publish_burst(&engine);
        let mut seen = Vec::new();
        for _ in 0..11 {
            seen.push(next_type().await);
        }
        assert_eq!(seen.last().map(String::as_str), Some("final"), "{seen:?}");
    }

    fn publish_burst(engine: &Arc<Engine>) {
        for i in 0..10 {
            engine.publish_partial(line(false, &format!("hello wor{i}")));
        }
        engine.publish_important(line(true, "hello world"));
    }
}
