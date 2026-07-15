//! OpenAI-compatible `/chat/completions` client.
//!
//! Wire types derived from real captured payloads (`fixtures/llm/`):
//! - Groq attaches `usage` timing extras, `x_groq{id,seed}`, `service_tier`
//!   — all ignored by serde's unknown-field tolerance.
//! - Ollama (qwen3) returns a non-standard `message.reasoning` field next
//!   to `content` (the model's chain of thought). Only `content` is read;
//!   a strict deserializer would reject the frame.

use async_trait::async_trait;
use auricle_core::{Error, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::stream::{parse_stream_frame, SseParser, TokenUsage, DONE_SENTINEL};
use crate::LlmProvider;

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
            client: reqwest::Client::new(),
        }
    }

    /// POST the request and surface non-2xx as a clean error. Both
    /// providers answer `stream: true` errors with a plain JSON body and a
    /// real HTTP status (fixtures `*_badmodel_error.txt`), so this check
    /// covers streaming and non-streaming alike.
    async fn send_checked(&self, request: &ChatRequest<'_>) -> Result<reqwest::Response> {
        let mut req = self.client.post(&self.endpoint).json(request);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Config(format!("llm request to {} failed: {e}", self.endpoint)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let head: String = body.chars().take(300).collect();
            return Err(Error::Config(format!(
                "llm {} returned HTTP {status}: {head}",
                self.id
            )));
        }
        Ok(resp)
    }
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
            .map_err(|e| Error::Config(format!("parsing llm response: {e}")))?;
        let content = body
            .choices
            .first()
            .map(|c| c.message.content.trim().to_string())
            .unwrap_or_default();
        if content.is_empty() {
            return Err(Error::Config(format!(
                "llm {} returned an empty completion",
                self.id
            )));
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
        let mut sent_any = false;
        'stream: while let Some(chunk) = body.next().await {
            let chunk =
                chunk.map_err(|e| Error::Config(format!("llm {} stream failed: {e}", self.id)))?;
            for payload in parser.push(&chunk) {
                if payload == DONE_SENTINEL {
                    break 'stream;
                }
                let frame = parse_stream_frame(&payload).map_err(|e| {
                    Error::Config(format!("llm {} sent an unparseable frame: {e}", self.id))
                })?;
                if frame.usage.is_some() {
                    usage = frame.usage;
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
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
