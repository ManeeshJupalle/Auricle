//! Auricle daemon: session engine + SQLite persistence + axum REST/WS API.
//!
//! The engine wires the existing crates (capture → pipeline → stt →
//! assembler) exactly like `auricle record`, but driven by the REST
//! lifecycle instead of Ctrl+C. The assembler's broadcast feeds both the
//! WebSocket fan-out and the persistence path (finals only; partials are
//! never persisted).

mod api;
mod ask;
mod assemble;
mod diagnostics;
mod egress;
mod engine;
mod events;
mod export;
mod lifecycle;
mod redact;
mod ring;
mod store;
mod title;
mod ws;

pub use api::{build_router, build_router_with_reader, serve};
pub use engine::{AudioSource, Engine, EngineOptions, StartParams};
pub use events::WsEvent;
pub use export::render_markdown_full;
pub use store::{SegmentRow, SessionRow, Store, SummaryRow};

use std::path::PathBuf;

use auricle_core::{Error, Result};

/// Root data directory: `%LOCALAPPDATA%\auricle` (DB, models/, sessions/).
pub fn default_data_root() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| Error::Config("LOCALAPPDATA is not set".to_string()))?;
    Ok(PathBuf::from(base).join("auricle"))
}
