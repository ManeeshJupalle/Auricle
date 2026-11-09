//! Core types and configuration for Auricle.

mod config;
mod env;
mod error;
mod secret;
mod types;

pub use config::{
    is_loopback_url, AudioConfig, Config, CopilotConfig, DeepgramConfig, GroqLlmConfig,
    GroqWhisperConfig, LlmConfig, LoadedConfig, OllamaConfig, OpenAiCompatConfig, OpenAiLlmConfig,
    ServerConfig, SttConfig, WhisperLocalConfig, CONFIG_FILE_NAME, LLM_PROVIDER_IDS,
    STT_PROVIDER_IDS,
};
pub use env::env_lookup;
pub use error::{Error, Result};
pub use secret::{secret_delete, secret_lookup, secret_present, secret_set};
pub use types::{AudioChunk, ChannelId, ChunkKind, Segment, SttEvent, TimestampMs};
