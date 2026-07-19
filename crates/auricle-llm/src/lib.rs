//! LLM summarization: one OpenAI-compatible chat client covers Ollama,
//! Groq, and any compatible endpoint (configurable base_url). Response
//! types are derived from real captured payloads in `fixtures/llm/`
//! (see docs/PHASE6A_REPORT.md).

mod chat;
mod mapreduce;
mod stream;
mod templates;

use std::sync::Arc;

use async_trait::async_trait;
use auricle_core::{Config, Error, Result};

pub use chat::OpenAiChatProvider;
pub use mapreduce::{chunk_by_lines, estimate_tokens, summarize, MAP_REDUCE_TOKEN_THRESHOLD};
pub use stream::{parse_stream_frame, FrameOut, SseParser, TokenUsage, DONE_SENTINEL};
pub use templates::{list_templates, load_template, TemplateInfo, DEFAULT_TEMPLATES};

// PHASE-NEXT: native Anthropic variant — skipped in this build because no
// ANTHROPIC_API_KEY was available to capture a real payload against
// (payload-first rule); the trait below is where it plugs in.

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn model(&self) -> &str;
    /// One chat completion: system prompt + user content -> assistant text.
    async fn complete(&self, system: &str, user: &str) -> Result<String>;

    /// Streaming chat completion: answer fragments are delivered through
    /// `tx` as they arrive; returns the provider-reported token usage (if
    /// any) once the stream ends. A closed receiver aborts the stream
    /// without error — the consumer walked away.
    ///
    /// Default: wraps [`complete`](Self::complete), delivering the whole
    /// answer as one fragment. Providers with native streaming override it.
    async fn chat_stream(
        &self,
        system: &str,
        user: &str,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<Option<TokenUsage>> {
        let text = self.complete(system, user).await?;
        let _ = tx.send(text).await;
        Ok(None)
    }
}

/// Readiness of one LLM provider for the API/UI pickers.
#[derive(Debug, Clone)]
pub struct LlmProviderStatus {
    pub id: &'static str,
    pub model: String,
    pub ready: bool,
    pub detail: String,
}

pub fn llm_provider_ids() -> &'static [&'static str] {
    &["ollama", "groq", "openai-compat"]
}

/// Readiness without construction: key presence only (the Ollama endpoint
/// is only probed at use — a summarize call reports a clear error if the
/// daemon is down).
pub fn llm_provider_statuses(cfg: &Config) -> Vec<LlmProviderStatus> {
    let key_status = |var: &str| {
        if auricle_core::secret_present(var) {
            (true, format!("key present ({var})"))
        } else {
            (false, format!("{var} not set (environment or credential store)"))
        }
    };
    let groq = key_status(&cfg.llm.groq.api_key_env);
    let openai = key_status(&cfg.llm.openai_compat.api_key_env);
    vec![
        LlmProviderStatus {
            id: "ollama",
            model: cfg.llm.ollama.model.clone(),
            ready: true,
            detail: format!("local endpoint {}", cfg.llm.ollama.base_url),
        },
        LlmProviderStatus {
            id: "groq",
            model: cfg.llm.groq.model.clone(),
            ready: groq.0,
            detail: groq.1,
        },
        LlmProviderStatus {
            id: "openai-compat",
            model: cfg.llm.openai_compat.model.clone(),
            ready: openai.0,
            detail: openai.1,
        },
    ]
}

/// Construct a provider by id. Keys are read from the env var named in
/// config, held in memory only.
pub fn create_llm_provider(id: &str, cfg: &Config) -> Result<Arc<dyn LlmProvider>> {
    match id {
        "ollama" => Ok(Arc::new(OpenAiChatProvider::new(
            "ollama",
            &cfg.llm.ollama.base_url,
            &cfg.llm.ollama.model,
            None,
        ))),
        "groq" => {
            let key = auricle_core::secret_lookup(&cfg.llm.groq.api_key_env).ok_or_else(|| {
                Error::Config(format!(
                    "groq LLM key {} is not set (environment or credential store)",
                    cfg.llm.groq.api_key_env
                ))
            })?;
            Ok(Arc::new(OpenAiChatProvider::new(
                "groq",
                &cfg.llm.groq.base_url,
                &cfg.llm.groq.model,
                Some(key),
            )))
        }
        "openai-compat" => {
            let key =
                auricle_core::secret_lookup(&cfg.llm.openai_compat.api_key_env).ok_or_else(|| {
                    Error::Config(format!(
                        "openai-compat LLM key {} is not set (environment or credential store)",
                        cfg.llm.openai_compat.api_key_env
                    ))
                })?;
            Ok(Arc::new(OpenAiChatProvider::new(
                "openai-compat",
                &cfg.llm.openai_compat.base_url,
                &cfg.llm.openai_compat.model,
                Some(key),
            )))
        }
        other => Err(Error::Config(format!(
            "unknown llm provider \"{other}\" (available: {})",
            llm_provider_ids().join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_cover_all_ids() {
        let statuses = llm_provider_statuses(&Config::default());
        let ids: Vec<_> = statuses.iter().map(|s| s.id).collect();
        assert_eq!(ids, llm_provider_ids());
        // Ollama never gates on a key.
        assert!(statuses[0].ready);
    }

    #[test]
    fn unknown_provider_and_missing_key_are_clean_errors() {
        let mut cfg = Config::default();
        cfg.llm.groq.api_key_env = "AURICLE_TEST_NO_KEY_LLM".to_string();

        let err = create_llm_provider("nope", &cfg).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("unknown llm provider"), "{err}");

        let err = create_llm_provider("groq", &cfg).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("AURICLE_TEST_NO_KEY_LLM"), "{err}");
    }
}
