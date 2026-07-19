//! Provider registry: which providers exist, whether each is ready (model
//! present / key present), and construction from config.

use std::path::Path;
use std::sync::Arc;

use auricle_core::{Config, Error, Result};

use crate::{SttKind, SttProvider};

/// Provider ids in the order they cycle in the CLI.
pub fn provider_ids() -> &'static [&'static str] {
    #[cfg(feature = "cloud")]
    {
        &["whisper-local", "deepgram", "groq-whisper", "openai-compat"]
    }
    #[cfg(not(feature = "cloud"))]
    {
        &["whisper-local"]
    }
}

/// Readiness of one provider for `auricle providers`.
#[derive(Debug, Clone)]
pub struct ProviderStatus {
    pub id: &'static str,
    pub kind: SttKind,
    pub ready: bool,
    pub detail: String,
}

/// Check readiness without constructing anything (no model loads, no
/// network). Key values are never read here — only presence.
pub fn provider_statuses(cfg: &Config, data_dir: &Path) -> Vec<ProviderStatus> {
    let mut out = Vec::new();

    let model_status = crate::model::available_models()
        .iter()
        .find(|m| m.name == cfg.stt.whisper_local.model)
        .map(|m| {
            let path = data_dir.join(m.file_name);
            if path.exists() {
                (true, format!("model {} present", m.name))
            } else {
                (
                    false,
                    format!("model {} not downloaded (runs on first use)", m.name),
                )
            }
        })
        .unwrap_or((
            false,
            format!("unknown model \"{}\"", cfg.stt.whisper_local.model),
        ));
    out.push(ProviderStatus {
        id: "whisper-local",
        kind: SttKind::Local,
        ready: model_status.0,
        detail: model_status.1,
    });

    #[cfg(feature = "cloud")]
    {
        let key_status = |var: &str| {
            if auricle_core::secret_present(var) {
                (true, format!("key present ({var})"))
            } else {
                (false, format!("{var} not set (environment or credential store)"))
            }
        };
        let (ready, detail) = key_status(&cfg.stt.deepgram.api_key_env);
        out.push(ProviderStatus {
            id: "deepgram",
            kind: SttKind::CloudStreaming,
            ready,
            detail,
        });
        let (ready, detail) = key_status(&cfg.stt.groq_whisper.api_key_env);
        out.push(ProviderStatus {
            id: "groq-whisper",
            kind: SttKind::CloudBatch,
            ready,
            detail,
        });
        let (ready, detail) = key_status(&cfg.stt.openai_compat.api_key_env);
        out.push(ProviderStatus {
            id: "openai-compat",
            kind: SttKind::CloudBatch,
            ready,
            detail,
        });
    }

    out
}

/// Construct a provider by id. whisper-local downloads/verifies its model
/// on first use (may take a while); cloud providers only read their key.
pub async fn create_provider(
    id: &str,
    cfg: &Config,
    data_dir: &Path,
) -> Result<Arc<dyn SttProvider>> {
    match id {
        "whisper-local" => {
            let model = crate::model::available_models()
                .iter()
                .find(|m| m.name == cfg.stt.whisper_local.model)
                .ok_or_else(|| {
                    let names: Vec<_> = crate::model::available_models()
                        .iter()
                        .map(|m| m.name)
                        .collect();
                    Error::Model(format!(
                        "unknown model \"{}\" (available: {})",
                        cfg.stt.whisper_local.model,
                        names.join(", ")
                    ))
                })?;
            let path = crate::model::ensure_model(model, data_dir).await?;
            let provider = tokio::task::spawn_blocking(move || {
                crate::whisper_local::WhisperLocalProvider::load(&path)
            })
            .await
            .map_err(|e| Error::Stt(format!("model load task failed: {e}")))??;
            Ok(Arc::new(provider))
        }
        #[cfg(feature = "cloud")]
        "deepgram" => Ok(Arc::new(crate::deepgram::DeepgramProvider::from_env(
            &cfg.stt.deepgram,
        )?)),
        #[cfg(feature = "cloud")]
        "groq-whisper" => Ok(Arc::new(
            crate::openai_batch::OpenAiBatchProvider::from_env(
                "groq-whisper",
                &cfg.stt.groq_whisper.api_key_env,
                &cfg.stt.groq_whisper.base_url,
                &cfg.stt.groq_whisper.model,
            )?,
        )),
        #[cfg(feature = "cloud")]
        "openai-compat" => Ok(Arc::new(
            crate::openai_batch::OpenAiBatchProvider::from_env(
                "openai-compat",
                &cfg.stt.openai_compat.api_key_env,
                &cfg.stt.openai_compat.base_url,
                &cfg.stt.openai_compat.model,
            )?,
        )),
        other => Err(Error::Stt(format!(
            "unknown stt provider \"{other}\" (available: {})",
            provider_ids().join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_cover_all_provider_ids() {
        let cfg = Config::default();
        let dir = std::env::temp_dir().join("auricle-registry-test");
        let statuses = provider_statuses(&cfg, &dir);
        let ids: Vec<_> = statuses.iter().map(|s| s.id).collect();
        assert_eq!(ids, provider_ids());
    }

    async fn expect_err(id: &str, cfg: &Config) -> String {
        match create_provider(id, cfg, &std::env::temp_dir()).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected provider creation to fail for {id}"),
        }
    }

    #[tokio::test]
    async fn unknown_provider_is_a_clean_error() {
        let msg = expect_err("nope", &Config::default()).await;
        assert!(msg.contains("unknown stt provider"), "{msg}");
        assert!(msg.contains("whisper-local"), "{msg}");
    }

    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn missing_key_error_names_the_env_var() {
        let mut cfg = Config::default();
        cfg.stt.deepgram.api_key_env = "AURICLE_TEST_NO_SUCH_KEY_DG".to_string();
        cfg.stt.groq_whisper.api_key_env = "AURICLE_TEST_NO_SUCH_KEY_GROQ".to_string();

        let msg = expect_err("deepgram", &cfg).await;
        assert!(msg.contains("AURICLE_TEST_NO_SUCH_KEY_DG"), "{msg}");
        let msg = expect_err("groq-whisper", &cfg).await;
        assert!(msg.contains("AURICLE_TEST_NO_SUCH_KEY_GROQ"), "{msg}");
    }
}
