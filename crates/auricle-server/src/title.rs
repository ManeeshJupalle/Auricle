//! Auto-titling: once a session stops, sessions still carrying their
//! placeholder title get a 3–6 word title generated from the transcript.
//! LLM first (the configured default provider), first-words-of-transcript
//! as the offline fallback. User-set titles are never touched (renaming
//! clears the `auto_title` meta flag).

use std::sync::Arc;
use std::time::Duration;

use auricle_core::Config;
use tokio::sync::broadcast;

use crate::events::WsEvent;
use crate::store::{SegmentRow, Store};

/// Placeholder given to sessions started without an explicit title.
pub const UNTITLED: &str = "Untitled session";

const LLM_TIMEOUT: Duration = Duration::from_secs(20);
/// Titles derive from how a meeting opens; cap the prompt cost.
const MAX_PROMPT_CHARS: usize = 4_000;

const TITLE_SYSTEM: &str = "You name meeting transcripts. Reply with ONLY a \
     concise, specific title of 3 to 6 words for this transcript — no \
     quotes, no trailing punctuation, no explanation.";

/// Clean an LLM-produced title into something list-worthy. None = unusable.
pub fn sanitize_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().find(|l| !l.trim().is_empty())?;
    let cleaned = first_line
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '“' || c == '”')
        .trim_end_matches(['.', '!', '?', ',', ';', ':'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() || cleaned.len() < 3 {
        return None;
    }
    let mut out = cleaned;
    if out.len() > 64 {
        let cut = out
            .char_indices()
            .take_while(|(i, _)| *i <= 61)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(61);
        out.truncate(cut);
        out.push('…');
    }
    Some(out)
}

/// Offline fallback: the first six words anyone said.
pub fn title_from_words(segments: &[SegmentRow]) -> Option<String> {
    let first = segments.iter().find(|s| !s.text.trim().is_empty())?;
    let words: Vec<&str> = first.text.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let mut title = words.iter().take(6).cloned().collect::<Vec<_>>().join(" ");
    title = title
        .trim_end_matches(['.', '!', '?', ',', ';', ':'])
        .to_string();
    if words.len() > 6 {
        title.push('…');
    }
    Some(title)
}

/// Generate and persist an auto-title for a just-stopped session, then
/// announce it. No-ops for user-titled sessions and empty transcripts.
pub async fn autotitle_session(
    store: Arc<Store>,
    cfg: Config,
    session_id: String,
    events: broadcast::Sender<WsEvent>,
) {
    let Ok(Some(session)) = store.get_session(&session_id) else {
        return;
    };
    if session.meta["auto_title"] != serde_json::Value::Bool(true) {
        return; // user named it (or a pre-feature session)
    }
    let segments = {
        let (store, id) = (store.clone(), session_id.clone());
        crate::api::off_runtime(move || store.get_segments(&id)).await
    };
    let Ok(segments) = segments else {
        return;
    };
    if segments.is_empty() {
        return; // nothing was said; the placeholder is honest
    }

    let title = llm_title(&cfg, &store, &session_id, &segments)
        .await
        .or_else(|| title_from_words(&segments));
    let Some(title) = title else { return };

    // The LLM call can run for LLM_TIMEOUT: only a session that is still
    // auto-titled gets this name, or it would replace the user's own.
    if store
        .rename_auto_titled(&session_id, &title)
        .unwrap_or(false)
    {
        let _ = events.send(WsEvent::SessionUpdated {
            session: session_id,
            title,
        });
    }
}

/// Cut `s` to at most `max` characters. `String::truncate` takes a byte index
/// and panics inside a multibyte character.
fn truncate_chars(s: &mut String, max: usize) {
    if let Some((end, _)) = s.char_indices().nth(max) {
        s.truncate(end);
    }
}

async fn llm_title(
    cfg: &Config,
    store: &Store,
    session_id: &str,
    segments: &[SegmentRow],
) -> Option<String> {
    let mut cfg = cfg.clone();
    crate::api::overlay_llm_settings(&mut cfg, store);
    let provider_id = cfg.llm.provider.clone();
    let provider = auricle_llm::create_llm_provider(&provider_id, &cfg).ok()?;
    let mut transcript = crate::export::render_transcript_text(segments);
    truncate_chars(&mut transcript, MAX_PROMPT_CHARS);
    // Egress ledger: the opening of the transcript goes to the titling LLM
    // (or stays local). Metadata only, as for summaries.
    crate::egress::record(
        store,
        Some(session_id),
        "title",
        crate::egress::llm_egress(&provider_id, &cfg),
        &provider_id,
        Some(transcript.chars().count() as i64),
        Some("auto-title"),
    );
    let raw = tokio::time::timeout(LLM_TIMEOUT, provider.complete(TITLE_SYSTEM, &transcript))
        .await
        .ok()?
        .ok()?;
    sanitize_title(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> SegmentRow {
        SegmentRow {
            channel: 1,
            speaker: "Them".into(),
            t_start_ms: 0,
            t_end_ms: 1000,
            text: text.into(),
            provider: "x".into(),
        }
    }

    #[test]
    fn prompt_cap_counts_characters_and_survives_multibyte_text() {
        // Byte 4000 falls inside a 'é': a byte-indexed cut panics here.
        let mut text = format!("a{}", "é".repeat(5_000));
        truncate_chars(&mut text, MAX_PROMPT_CHARS);
        assert_eq!(text.chars().count(), MAX_PROMPT_CHARS);
        assert!(text.ends_with('é'));

        let mut short = "日本語".to_string();
        truncate_chars(&mut short, MAX_PROMPT_CHARS);
        assert_eq!(short, "日本語");
    }

    #[test]
    fn sanitize_strips_quotes_punctuation_and_length() {
        assert_eq!(
            sanitize_title("\"Weekly Pipeline Latency Review.\"\n"),
            Some("Weekly Pipeline Latency Review".to_string())
        );
        assert_eq!(
            sanitize_title("  Q3   budget    sync!  "),
            Some("Q3 budget sync".to_string())
        );
        // Multi-line LLM chatter: first non-empty line wins.
        assert_eq!(
            sanitize_title("\nSprint Retro Highlights\nHere is why…"),
            Some("Sprint Retro Highlights".to_string())
        );
        assert_eq!(sanitize_title("   "), None);
        let long = "word ".repeat(40);
        let t = sanitize_title(&long).unwrap();
        assert!(t.chars().count() <= 63);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn word_fallback_takes_first_six_words() {
        let segs = vec![
            seg(""),
            seg("Welcome to the weekly engineering sync everyone."),
        ];
        assert_eq!(
            title_from_words(&segs),
            Some("Welcome to the weekly engineering sync…".to_string())
        );
        let segs = vec![seg("Ship it Friday.")];
        assert_eq!(title_from_words(&segs), Some("Ship it Friday".to_string()));
        assert_eq!(title_from_words(&[]), None);
    }
}
