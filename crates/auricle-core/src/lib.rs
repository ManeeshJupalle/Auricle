//! Core types and configuration for Auricle.

mod config;
mod error;
mod types;

pub use config::{AudioConfig, Config};
pub use error::{Error, Result};
pub use types::{AudioChunk, ChannelId, TimestampMs};
