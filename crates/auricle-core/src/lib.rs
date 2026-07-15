//! Core types and configuration for Auricle.

mod config;
mod env;
mod error;
mod types;

pub use config::{
    AudioConfig, Config, CopilotConfig, DeepgramConfig, GroqLlmConfig, GroqWhisperConfig,
    LlmConfig, OllamaConfig, OpenAiCompatConfig, OpenAiLlmConfig, ServerConfig, SttConfig,
    WhisperLocalConfig,
};
pub use env::env_lookup;
pub use error::{Error, Result};
pub use types::{AudioChunk, ChannelId, ChunkKind, Segment, SttEvent, TimestampMs};
