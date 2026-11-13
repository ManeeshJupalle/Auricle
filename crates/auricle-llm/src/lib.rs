//! LLM summarization: one OpenAI-compatible chat client covers Ollama,
//! Groq, and any compatible endpoint (configurable base_url). Response
//! types are derived from real captured payloads in `fixtures/llm/`
//! (see docs/PHASE6A_REPORT.md).

mod chat;
mod mapreduce;
mod stream;
mod templates;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use auricle_core::{Config, Error, Result};

pub use chat::{OpenAiChatProvider, TRUNCATION_NOTICE};
pub use mapreduce::{chunk_by_lines, estimate_tokens, summarize, MAP_REDUCE_TOKEN_THRESHOLD};
pub use stream::{parse_stream_frame, FrameOut, SseParser, TokenUsage, DONE_SENTINEL};
pub use templates::{
    list_templates, load_template, TemplateInfo, DEFAULT_TEMPLATES, DICTATION_POLISH,
};

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

/// How long the Ollama check may take. A running Ollama answers in a few
/// milliseconds. Windows takes about two seconds to refuse a connection to a
/// closed local port, so an Ollama that isn't there shows up as this running
/// out, not as a refusal.
const OLLAMA_CHECK_TIMEOUT: Duration = Duration::from_millis(800);

/// What asking Ollama for its models came to.
enum OllamaModels {
    NotAnswering,
    /// Something answered, but not with a list this can read.
    Unreadable,
    Listed(Vec<String>),
}

/// The models behind an OpenAI-compatible `GET {base}/models`. The shape is
/// from Ollama's API documentation, not a captured payload, so anything that
/// doesn't parse is `Unreadable` rather than an empty list.
async fn ollama_models(base_url: &str) -> OllamaModels {
    #[derive(serde::Deserialize)]
    struct List {
        data: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        id: String,
    }
    let Ok(resp) = reqwest::Client::new()
        .get(format!("{base_url}/models"))
        .send()
        .await
    else {
        return OllamaModels::NotAnswering;
    };
    match resp.json::<List>().await {
        Ok(list) => OllamaModels::Listed(list.data.into_iter().map(|m| m.id).collect()),
        Err(_) => OllamaModels::Unreadable,
    }
}

/// Ollama lists every model with its tag, and reads a name without one as
/// `:latest`.
fn ollama_has_model(listed: &[String], model: &str) -> bool {
    listed.iter().any(|id| {
        id == model || (!model.contains(':') && id.strip_suffix(":latest") == Some(model))
    })
}

/// Whether Ollama is up and has the configured model. It has no key to look
/// for, so asking it is the only way to know. Takes up to
/// [`OLLAMA_CHECK_TIMEOUT`]. An answer that can't be read counts as ready:
/// this must not hide a setup that works.
pub async fn ollama_status(cfg: &Config) -> LlmProviderStatus {
    let base_url = cfg.llm.ollama.base_url.trim_end_matches('/');
    let model = &cfg.llm.ollama.model;
    let models = tokio::time::timeout(OLLAMA_CHECK_TIMEOUT, ollama_models(base_url))
        .await
        .unwrap_or(OllamaModels::NotAnswering);
    let (ready, detail) = match models {
        OllamaModels::NotAnswering => (false, chat::ollama_not_answering(base_url, model)),
        OllamaModels::Listed(listed) if !ollama_has_model(&listed, model) => {
            (false, chat::ollama_model_missing(model))
        }
        OllamaModels::Listed(_) | OllamaModels::Unreadable => {
            (true, format!("local endpoint {base_url}"))
        }
    };
    LlmProviderStatus {
        id: "ollama",
        model: model.clone(),
        ready,
        detail,
    }
}

/// Readiness without construction or network: key presence only. Ollama has
/// no key, so it is always ready here; [`ollama_status`] asks it.
pub fn llm_provider_statuses(cfg: &Config) -> Vec<LlmProviderStatus> {
    let key_status = |var: &str| {
        if auricle_core::secret_present(var) {
            (true, format!("key present ({var})"))
        } else {
            (
                false,
                format!("{var} not set (environment or credential store)"),
            )
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
            let key = auricle_core::secret_lookup(&cfg.llm.openai_compat.api_key_env).ok_or_else(
                || {
                    Error::Config(format!(
                        "openai-compat LLM key {} is not set (environment or credential store)",
                        cfg.llm.openai_compat.api_key_env
                    ))
                },
            )?;
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

    #[test]
    fn a_model_without_a_tag_means_latest() {
        let listed = ["llama3.1:latest".to_string(), "qwen3:8b".to_string()];
        assert!(ollama_has_model(&listed, "llama3.1"));
        assert!(ollama_has_model(&listed, "llama3.1:latest"));
        assert!(ollama_has_model(&listed, "qwen3:8b"));
        assert!(!ollama_has_model(&listed, "qwen3"), "qwen3 is qwen3:latest");
        assert!(!ollama_has_model(&listed, "llama3.1:70b"));
    }
}
