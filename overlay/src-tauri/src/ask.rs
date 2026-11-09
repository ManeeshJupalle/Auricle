//! One /ask stream at a time. Dismissing the overlay or asking again
//! abandons the running stream: dropping it here closes the connection, so
//! nothing stale can reach the webview. The engine finishes the ask anyway
//! (a dropped SSE client never breaks its /ws/live mirror).

use std::sync::Mutex;

use futures_util::future::{AbortHandle, AbortRegistration, Abortable};
use futures_util::StreamExt;

use crate::sse;

#[derive(Default)]
pub struct AskSlot {
    current: Mutex<Option<AbortHandle>>,
}

impl AskSlot {
    /// Claim the slot for a new ask, cancelling whichever ask held it.
    pub fn begin(&self) -> AbortRegistration {
        let (handle, registration) = AbortHandle::new_pair();
        if let Some(previous) = self.current.lock().unwrap().replace(handle) {
            previous.abort();
        }
        registration
    }

    pub fn cancel(&self) {
        if let Some(current) = self.current.lock().unwrap().take() {
            current.abort();
        }
    }
}

/// POST `body` to /ask and pass each SSE event to `on_event`. A cancelled
/// run is not an error: whoever cancelled it has moved on.
pub async fn stream(
    http: &reqwest::Client,
    base_url: &str,
    body: &serde_json::Value,
    registration: AbortRegistration,
    on_event: impl FnMut(serde_json::Value),
) -> Result<(), String> {
    let run = forward(http, base_url, body, on_event);
    Abortable::new(run, registration).await.unwrap_or(Ok(()))
}

async fn forward(
    http: &reqwest::Client,
    base_url: &str,
    body: &serde_json::Value,
    mut on_event: impl FnMut(serde_json::Value),
) -> Result<(), String> {
    let resp = http
        .post(format!("{base_url}/api/v1/ask"))
        .json(body)
        .send()
        .await
        .map_err(|e| format!("engine unreachable: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or(text);
        return Err(format!("{detail} (HTTP {status})"));
    }

    let mut parser = sse::SseLineParser::default();
    let mut stream = resp.bytes_stream();
    let mut finished = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("answer stream broke: {e}"))?;
        for payload in parser.push(&chunk) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                finished |= v["type"] == "answer_done" || v["type"] == "ask_error";
                on_event(v);
            }
        }
    }
    // The webview stays busy until it hears an ending, so a stream that
    // closes cleanly without one still has to end the ask.
    if finished {
        Ok(())
    } else {
        Err("the engine closed the answer stream before it finished".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    /// An engine that streams one event, then keeps the connection busy
    /// until the client goes away, and says when that happened.
    fn endless_stream() -> (String, mpsc::Receiver<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (closed, closed_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 2048]);
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
                .unwrap();
            s.write_all(b"data: {\"type\":\"answer_delta\",\"text\":\"old\"}\n\n")
                .unwrap();
            while s.write_all(b": keep-alive\n\n").is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = closed.send(());
        });
        (base, closed_rx)
    }

    async fn first_event_then<F: FnOnce(&AskSlot)>(end_it: F) {
        let (base, closed) = endless_stream();
        let slot = Arc::new(AskSlot::default());
        let registration = slot.begin();
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            let http = reqwest::Client::new();
            let body = serde_json::json!({});
            stream(&http, &base, &body, registration, |v| {
                let _ = seen_tx.send(v);
            })
            .await
        });

        assert_eq!(seen.recv().await.unwrap()["text"], "old");
        end_it(&slot);

        let finished = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("a cancelled ask ends at once");
        assert_eq!(finished.unwrap(), Ok(()));
        closed
            .recv_timeout(Duration::from_secs(5))
            .expect("cancelling closes the connection to the engine");
        assert!(seen.recv().await.is_none(), "nothing arrives after cancel");
    }

    #[tokio::test]
    async fn dismissing_cancels_the_stream_and_closes_the_connection() {
        first_event_then(|slot| slot.cancel()).await;
    }

    #[tokio::test]
    async fn a_new_ask_cancels_the_one_before_it() {
        first_event_then(|slot| drop(slot.begin())).await;
    }

    #[tokio::test]
    async fn a_new_ask_is_not_cancelled_by_claiming_the_slot() {
        let slot = AskSlot::default();
        drop(slot.begin());
        let registration = slot.begin();
        let done = Abortable::new(async { 7 }, registration).await;
        assert_eq!(done, Ok(7));
    }

    /// An engine that sends `events`, then ends the response cleanly.
    fn finite_stream(events: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 2048]);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                events.len()
            );
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(events.as_bytes()).unwrap();
        });
        base
    }

    async fn run_to_the_end(base: &str) -> (Result<(), String>, usize) {
        let slot = AskSlot::default();
        let http = reqwest::Client::new();
        let body = serde_json::json!({});
        let mut seen = 0;
        let result = stream(&http, base, &body, slot.begin(), |_| seen += 1).await;
        (result, seen)
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_terminal_event_is_an_error() {
        let base = finite_stream("data: {\"type\":\"answer_delta\",\"text\":\"half\"}\n\n");
        let (result, seen) = run_to_the_end(&base).await;
        assert_eq!(seen, 1, "what did arrive still reaches the webview");
        let err = result.unwrap_err();
        assert!(err.contains("before"), "{err}");
    }

    #[tokio::test]
    async fn a_stream_that_ends_with_nothing_at_all_is_an_error() {
        let (result, _) = run_to_the_end(&finite_stream("")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn a_finished_answer_ends_the_stream_cleanly() {
        let base = finite_stream("data: {\"type\":\"answer_done\",\"usage\":null}\n\n");
        assert_eq!(run_to_the_end(&base).await, (Ok(()), 1));
    }

    #[tokio::test]
    async fn an_ask_error_event_needs_no_second_error() {
        let base = finite_stream("data: {\"type\":\"ask_error\",\"message\":\"no key\"}\n\n");
        assert_eq!(run_to_the_end(&base).await, (Ok(()), 1));
    }

    #[tokio::test]
    async fn an_error_status_reports_the_engine_message() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 2048]);
            let body = r#"{"error":"llm provider is not ready"}"#;
            let head = format!(
                "HTTP/1.1 409 Conflict\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(body.as_bytes()).unwrap();
        });
        let slot = AskSlot::default();
        let http = reqwest::Client::new();
        let body = serde_json::json!({});
        let err = stream(&http, &base, &body, slot.begin(), |_| {})
            .await
            .unwrap_err();
        assert!(err.contains("llm provider is not ready"), "{err}");
        assert!(err.contains("409"), "{err}");
    }
}
