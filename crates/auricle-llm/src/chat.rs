//! OpenAI-compatible `/chat/completions` client.
//!
//! Wire types derived from real captured payloads (`fixtures/llm/`):
//! - Groq attaches `usage` timing extras, `x_groq{id,seed}`, `service_tier`
//!   — all ignored by serde's unknown-field tolerance.
//! - Ollama (qwen3) returns a non-standard `message.reasoning` field next
//!   to `content` (the model's chain of thought). Only `content` is read;
//!   a strict deserializer would reject the frame.

use std::time::Duration;

use async_trait::async_trait;
use auricle_core::{Error, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::stream::{parse_stream_frame, SseParser, TokenUsage, DONE_SENTINEL};
use crate::LlmProvider;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per read, never total, so a stream may run as long as bytes keep coming.
/// Sized for the worst single silence: `complete` sends nothing until the
/// whole answer is done.
const READ_TIMEOUT: Duration = Duration::from_secs(600);
/// The same for a model the user runs themselves, where long silence is
/// normal: on a CPU a full prompt alone can take well over 10 minutes before
/// the first byte, streaming or not. Still bounded, so a hung server ends in
/// an error rather than a summary that never returns.
const LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(3_600);
const MAX_ATTEMPTS: u32 = 4;
const BASE_BACKOFF: Duration = Duration::from_secs(1);
/// Ceiling on one 429 wait, even when the server advises longer.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// Appended to output cut off by `finish_reason: length`. A cut-off answer
/// is still worth keeping, but must never read as complete.
pub const TRUNCATION_NOTICE: &str =
    "[Truncated: the model reached its output limit before finishing.]";

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    temperature: f32,
    stream: bool,
    /// Ollama sends no usage frame without this; Groq tolerates it (and
    /// reports usage on the finish chunk regardless) — fixture-verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatResponse {
    pub choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatChoice {
    pub message: ChatResponseMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatResponseMessage {
    pub content: String,
}

pub struct OpenAiChatProvider {
    id: String,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiChatProvider {
    pub fn new(id: &str, base_url: &str, model: &str, api_key: Option<String>) -> Self {
        OpenAiChatProvider {
            id: id.to_string(),
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
            client: http_client(read_timeout(id, base_url)),
        }
    }

    /// POST the request and surface non-2xx as a clean error, retrying 429s.
    /// Both providers answer `stream: true` errors with a plain JSON body
    /// and a real HTTP status (fixtures `*_badmodel_error.txt`), so this
    /// check covers streaming and non-streaming alike.
    async fn send_checked(&self, request: &ChatRequest<'_>) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self.client.post(&self.endpoint).json(request);
            if let Some(key) = &self.api_key {
                req = req.bearer_auth(key);
            }
            let resp = req.send().await.map_err(|e| {
                Error::Config(format!(
                    "llm request to {} failed: {}",
                    self.endpoint,
                    http_error(&e)
                ))
            })?;
            let status = resp.status();
            if status.as_u16() == 429 && attempt < MAX_ATTEMPTS {
                let wait = retry_after(resp.headers())
                    .unwrap_or(BASE_BACKOFF * 2u32.pow(attempt - 1))
                    .min(MAX_RATE_LIMIT_WAIT);
                tokio::time::sleep(wait).await;
                continue;
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let head: String = body.chars().take(300).collect();
                return Err(Error::Config(format!(
                    "llm {} returned HTTP {status}: {head}",
                    self.id
                )));
            }
            return Ok(resp);
        }
    }
}

/// A request error in words: reqwest's own text for a timeout is only "error
/// sending request".
fn http_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out waiting for the server".to_string()
    } else {
        e.to_string()
    }
}

/// Ollama is self-hosted wherever it lives; anything else is local only if it
/// is on this machine.
fn read_timeout(id: &str, base_url: &str) -> Duration {
    if id == "ollama" || auricle_core::is_loopback_url(base_url) {
        LOCAL_READ_TIMEOUT
    } else {
        READ_TIMEOUT
    }
}

fn http_client(read_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
        .build()
        .expect("building http client")
}

/// The `Retry-After` wait, in its seconds form (the only one these providers
/// send; an HTTP date falls back to the backoff).
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: f64 = value.trim().parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

#[async_trait]
impl LlmProvider for OpenAiChatProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let request = ChatRequest {
            model: &self.model,
            messages: [
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            temperature: 0.3,
            stream: false,
            stream_options: None,
        };
        let resp = self.send_checked(&request).await?;
        let body: ChatResponse = resp
            .json()
            .await
            .map_err(|e| Error::Config(format!("parsing llm response: {}", http_error(&e))))?;
        let choice = body.choices.first();
        let content = choice
            .map(|c| c.message.content.trim().to_string())
            .unwrap_or_default();
        if content.is_empty() {
            return Err(Error::Config(format!(
                "llm {} returned an empty completion",
                self.id
            )));
        }
        if choice.and_then(|c| c.finish_reason.as_deref()) == Some("length") {
            return Ok(format!("{content}\n\n{TRUNCATION_NOTICE}"));
        }
        Ok(content)
    }

    async fn chat_stream(
        &self,
        system: &str,
        user: &str,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<Option<TokenUsage>> {
        let request = ChatRequest {
            model: &self.model,
            messages: [
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            temperature: 0.3,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        };
        let resp = self.send_checked(&request).await?;
        let mut body = resp.bytes_stream();
        let mut parser = SseParser::new();
        let mut usage = None;
        let mut finish = None;
        let mut saw_done = false;
        let mut sent_any = false;
        'stream: while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| {
                Error::Config(format!("llm {} stream failed: {}", self.id, http_error(&e)))
            })?;
            for payload in parser.push(&chunk) {
                if payload == DONE_SENTINEL {
                    saw_done = true;
                    break 'stream;
                }
                let frame = parse_stream_frame(&payload).map_err(|e| {
                    Error::Config(format!("llm {} sent an unparseable frame: {e}", self.id))
                })?;
                if let Some(message) = frame.error {
                    return Err(Error::Config(format!(
                        "llm {} failed mid-stream: {message}",
                        self.id
                    )));
                }
                if frame.usage.is_some() {
                    usage = frame.usage;
                }
                if frame.finish.is_some() {
                    finish = frame.finish;
                }
                if let Some(delta) = frame.delta {
                    if tx.send(delta).await.is_err() {
                        return Ok(usage); // consumer gone: abandon quietly
                    }
                    sent_any = true;
                }
            }
        }
        if !sent_any {
            // A reasoning model can think its way to a stop with no
            // answer content at all; treat it like the empty completion
            // in `complete`.
            return Err(Error::Config(format!(
                "llm {} streamed an empty completion",
                self.id
            )));
        }
        // A finish_reason already proves the model ended; only a stream with
        // neither that nor [DONE] was cut off partway.
        if !saw_done && finish.is_none() {
            return Err(Error::Config(format!(
                "llm {} stream ended before the answer finished",
                self.id
            )));
        }
        if finish.as_deref() == Some("length") {
            let _ = tx.send(format!("\n\n{TRUNCATION_NOTICE}")).await;
        }
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn stalled_endpoint_fails_instead_of_hanging() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        let mut provider = OpenAiChatProvider::new("ollama", &server.uri(), "m", None);
        provider.client = http_client(Duration::from_millis(200));

        let result =
            tokio::time::timeout(Duration::from_secs(10), provider.complete("sys", "user"))
                .await
                .expect("a stalled server must not hang the caller");

        let err = result.unwrap_err().to_string();
        assert!(err.contains("failed"), "{err}");
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn a_stream_may_outlast_the_read_timeout_while_bytes_keep_coming() {
        use std::io::{Read as _, Write as _};

        // Raw socket: wiremock can't pace a body. 1.5 s of stream against a
        // 1 s timeout only passes if the timeout is per read, not total.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0u8; 4096]);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n")
                .unwrap();
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(300));
                socket
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
                    .unwrap();
            }
            socket.write_all(b"data: [DONE]\n\n").unwrap();
            std::thread::sleep(Duration::from_millis(200)); // let the client read it
        });
        let mut provider = OpenAiChatProvider::new("ollama", &url, "m", None);
        provider.client = http_client(Duration::from_secs(1));

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        provider.chat_stream("sys", "user", tx).await.unwrap();

        let mut deltas = 0;
        while rx.recv().await.is_some() {
            deltas += 1;
        }
        assert_eq!(deltas, 5);
    }

    #[test]
    fn a_self_hosted_model_gets_longer_to_answer_than_a_cloud_one() {
        for (id, url) in [
            ("ollama", "http://localhost:11434/v1"),
            ("ollama", "http://192.168.1.20:11434/v1"),
            ("openai-compat", "http://127.0.0.1:8080/v1"),
        ] {
            assert_eq!(read_timeout(id, url), LOCAL_READ_TIMEOUT, "{id} {url}");
        }
        for (id, url) in [
            ("groq", "https://api.groq.com/openai/v1"),
            ("openai-compat", "https://api.openai.com/v1"),
            ("openai-compat", "http://192.168.1.20:8080/v1"),
        ] {
            assert_eq!(read_timeout(id, url), READ_TIMEOUT, "{id} {url}");
        }
    }

    #[test]
    fn retry_after_reads_seconds_and_ignores_dates() {
        let wait = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            retry_after(&headers)
        };
        assert_eq!(wait("7"), Some(Duration::from_secs(7)));
        assert_eq!(wait("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(wait("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(wait("-1"), None);
    }

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/llm")
            .join(name);
        std::fs::read_to_string(path).expect("fixture present")
    }

    #[test]
    fn groq_fixture_deserializes() {
        let body: ChatResponse = serde_json::from_str(&fixture("groq_chat.json")).unwrap();
        let content = &body.choices[0].message.content;
        assert!(content.contains("Meeting Minutes"));
        assert!(content.contains("Friday"));
    }

    #[test]
    fn ollama_fixture_with_reasoning_field_deserializes() {
        // qwen3 via Ollama adds message.reasoning — must not break parsing,
        // and content must come out clean (no chain of thought).
        let body: ChatResponse = serde_json::from_str(&fixture("ollama_chat.json")).unwrap();
        let content = &body.choices[0].message.content;
        assert!(content.contains("Friday"));
        assert!(!content.contains("Okay, let me start"), "reasoning leaked");
    }
}
