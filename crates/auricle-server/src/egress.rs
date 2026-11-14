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
/// Bracket-aware so it agrees with [`authority_is_loopback`]: a port-less
/// `[::1]` must not be split on a colon that is part of the address.
pub(crate) fn host_of(url: &str) -> Option<String> {
    let after = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after.split(['/', '?', '#']).next().unwrap_or("").trim();
    // `user:password@` comes before the host, and the request goes to what
    // follows the last `@`: `http://localhost:1@api.groq.com` is Groq.
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // [IPv6] or [IPv6]:port
    } else if host_port.matches(':').count() > 1 {
        host_port // raw IPv6 literal: colons are the address, not a port
    } else {
        host_port.split(':').next().unwrap_or(host_port) // host or host:port
    };
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

/// Ledger write that surfaces its error.
///
/// Every caller records just before the data leaves, so a send that then fails
/// leaves a row for something that never went. The ledger can over-report, and
/// that is the safe direction for it: a row too many misleads nobody about what
/// might have left. A row too few would, so an unwritable ledger stops the
/// send; [`record`] does that for a cloud destination only, and the MCP tools,
/// which disclose even a local read, use this directly.
pub(crate) fn record_checked(
    store: &Store,
    session_id: Option<&str>,
    kind: &str,
    dest: (&str, Option<String>),
    provider: &str,
    items: Option<i64>,
    detail: Option<&str>,
) -> auricle_core::Result<()> {
    let (destination, host) = dest;
    store.record_egress(&EgressEntry {
        ts: crate::engine::unix_secs(),
        session_id,
        destination,
        provider,
        host: host.as_deref(),
        kind,
        items,
        detail,
    })
}

/// Ledger write for data about to leave. A row that can't be written stops a
/// send to the cloud, as nothing goes there without one; for data that stays
/// on this machine the failure is only logged, and the request goes ahead.
pub(crate) fn record(
    store: &Store,
    session_id: Option<&str>,
    kind: &str,
    dest: (&str, Option<String>),
    provider: &str,
    items: Option<i64>,
    detail: Option<&str>,
) -> auricle_core::Result<()> {
    let cloud = dest.0 == "cloud";
    match record_checked(store, session_id, kind, dest, provider, items, detail) {
        Err(e) if cloud => Err(auricle_core::Error::Config(format!(
            "the egress ledger couldn't be written, so nothing was sent: {e}"
        ))),
        Err(e) => {
            eprintln!("recording egress: {e}");
            Ok(())
        }
        Ok(()) => Ok(()),
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
        // Bracketed IPv6 loopback, with and without a port, must yield the
        // bare address so classify() sees it as local (not "[:").
        assert_eq!(host_of("http://[::1]:11434/v1").as_deref(), Some("::1"));
        assert_eq!(host_of("http://[::1]/v1").as_deref(), Some("::1"));
        assert_eq!(host_of("").as_deref(), None);
    }

    #[test]
    fn credentials_in_a_url_are_not_its_host() {
        assert_eq!(
            host_of("https://user:pw@llm-proxy.example/v1").as_deref(),
            Some("llm-proxy.example")
        );
        // Looks local, goes to Groq.
        let groq = host_of("http://localhost:1@api.groq.com/openai/v1");
        assert_eq!(groq.as_deref(), Some("api.groq.com"));
        assert_eq!(classify(groq).0, "cloud");
    }

    #[test]
    fn bracketed_ipv6_loopback_classifies_as_local() {
        assert_eq!(classify(host_of("http://[::1]/v1")).0, "local");
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
