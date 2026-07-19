//! Egress ledger: classify where a provider sends data, and record it.
//!
//! The ledger records only metadata — provider, host, rough size — never
//! the audio, prompt, or answer. A provider counts as `"local"` when its
//! endpoint is loopback (on-device Whisper, or an Ollama / OpenAI-compatible
//! server on localhost); anything reachable off the machine is `"cloud"`.

use auricle_core::Config;

use crate::api::authority_is_loopback;
use crate::store::{EgressEntry, Store};

/// Host portion of an http(s) base URL — no scheme, port, or path.
pub(crate) fn host_of(url: &str) -> Option<String> {
    let after = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host_port = after.split(['/', '?']).next().unwrap_or("");
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port)
        .trim();
    (!host.is_empty()).then(|| host.to_string())
}

fn classify(host: Option<String>) -> (&'static str, Option<String>) {
    match host {
        Some(h) if authority_is_loopback(&h) => ("local", Some(h)),
        Some(h) => ("cloud", Some(h)),
        None => ("local", None),
    }
}

/// Destination + host for an STT provider given the active config.
pub(crate) fn stt_egress(id: &str, cfg: &Config) -> (&'static str, Option<String>) {
    match id {
        "deepgram" => ("cloud", Some("api.deepgram.com".to_string())),
        "groq-whisper" => classify(host_of(&cfg.stt.groq_whisper.base_url)),
        "openai-compat" => classify(host_of(&cfg.stt.openai_compat.base_url)),
        _ => ("local", None), // whisper-local and any local/unknown provider
    }
}

/// Destination + host for an LLM provider given the active config.
pub(crate) fn llm_egress(id: &str, cfg: &Config) -> (&'static str, Option<String>) {
    match id {
        "ollama" => classify(host_of(&cfg.llm.ollama.base_url)),
        "groq" => classify(host_of(&cfg.llm.groq.base_url)),
        "openai-compat" => classify(host_of(&cfg.llm.openai_compat.base_url)),
        _ => ("local", None),
    }
}

/// Best-effort ledger write: a failure here must never break the request
/// that triggered the egress, so the error is logged and dropped.
pub(crate) fn record(
    store: &Store,
    session_id: Option<&str>,
    kind: &str,
    dest: (&str, Option<String>),
    provider: &str,
    items: Option<i64>,
    detail: Option<&str>,
) {
    let (destination, host) = dest;
    if let Err(e) = store.record_egress(&EgressEntry {
        ts: crate::engine::unix_secs(),
        session_id,
        destination,
        provider,
        host: host.as_deref(),
        kind,
        items,
        detail,
    }) {
        eprintln!("recording egress: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_strips_scheme_port_and_path() {
        assert_eq!(
            host_of("https://api.groq.com/openai/v1").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(
            host_of("http://localhost:11434/v1").as_deref(),
            Some("localhost")
        );
        assert_eq!(host_of("").as_deref(), None);
    }

    #[test]
    fn local_endpoints_classify_as_local() {
        let cfg = Config::default();
        // Ollama defaults to localhost.
        assert_eq!(llm_egress("ollama", &cfg).0, "local");
        // Groq/OpenAI default to real hosts.
        assert_eq!(llm_egress("groq", &cfg).0, "cloud");
        assert_eq!(stt_egress("deepgram", &cfg).0, "cloud");
        // whisper-local never touches the network.
        assert_eq!(stt_egress("whisper-local", &cfg), ("local", None));
    }
}
