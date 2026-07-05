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
use serde::{Deserialize, Serialize};

use crate::LlmProvider;

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    temperature: f32,
    stream: bool,
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
        };
        let mut req = self.client.post(&self.endpoint).json(&request);
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
