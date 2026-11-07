//! STT provider abstraction (architecture §4.5) and implementations:
//! `whisper-local` (always), plus `deepgram`, `groq-whisper`, and
//! `openai-compat` behind the default `cloud` feature.
//!
//! Inference/uploads never block capture: every session runs a worker with
//! a bounded chunk queue (drop-oldest-interim + counter).

mod hallucination;
mod language;
mod model;
mod queue;
mod registry;
mod whisper_local;

#[cfg(feature = "cloud")]
mod deepgram;
#[cfg(feature = "cloud")]
mod openai_batch;

use async_trait::async_trait;
use auricle_core::{AudioChunk, Result, Segment, SttEvent};

pub use language::{check_language, english_only_warning, validate_language};
pub use model::{available_models, default_data_dir, ensure_model, file_sha256, ModelInfo};
pub use registry::{create_provider, provider_ids, provider_statuses, ProviderStatus};
pub use whisper_local::WhisperLocalProvider;

#[cfg(feature = "cloud")]
pub use deepgram::DeepgramProvider;
#[cfg(feature = "cloud")]
pub use openai_batch::OpenAiBatchProvider;

/// A request error in words: reqwest's own text for a timeout is only "error
/// sending request".
fn http_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out waiting for the server".to_string()
    } else {
        e.to_string()
    }
}

/// How a provider processes audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttKind {
    Local,
    CloudStreaming,
    CloudBatch,
}

impl std::fmt::Display for SttKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SttKind::Local => write!(f, "local"),
            SttKind::CloudStreaming => write!(f, "cloud-streaming"),
            SttKind::CloudBatch => write!(f, "cloud-batch"),
        }
    }
}

/// Per-session configuration handed to a provider.
#[derive(Debug, Clone)]
pub struct SessionCfg {
    pub session_id: String,
    /// "auto" or a language tag ("en", "pt-BR"): see [`validate_language`].
    pub language: String,
}

/// What a session has shed so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SttDrops {
    /// Every chunk shed, interim or final.
    pub chunks: u64,
    /// The finals among `chunks`: transcript lost, not a skipped partial.
    pub finals: u64,
}

#[async_trait]
pub trait SttProvider: Send + Sync {
    fn id(&self) -> &'static str; // "whisper-local", "deepgram", ...
    fn kind(&self) -> SttKind;
    async fn start_session(&self, cfg: &SessionCfg) -> Result<Box<dyn SttSession>>;
}

#[async_trait]
pub trait SttSession: Send {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()>;
    async fn next_event(&mut self) -> Option<SttEvent>;
    async fn finish(&mut self) -> Result<Vec<Segment>>;
    /// Running totals of what this session shed; still valid after `finish`.
    fn drops(&self) -> SttDrops {
        SttDrops::default()
    }
}
