//! Minimal SSE line parser for the engine's /ask stream. The engine
//! emits one single-line `data: <json>` per event with blank-line
//! separators and SSE keep-alive comments (`:`) during silent stretches
//! (fixture-verified in Phase 8); this parser handles exactly that shape,
//! byte-split-safe across network chunks.

#[derive(Default)]
pub struct SseLineParser {
    buf: String,
}

impl SseLineParser {
    /// Feed a chunk; returns the `data:` payloads of every line completed
    /// by it. Comments, blank lines, and other fields are skipped.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=nl).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(payload) = line.strip_prefix("data:") {
                out.push(payload.strip_prefix(' ').unwrap_or(payload).to_string());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_and_skips_keepalive_comments() {
        let mut p = SseLineParser::default();
        let out = p.push("data: {\"type\":\"ask_started\"}\n\n: keep-alive\n\ndata: {\"a\":1}\n\n");
        assert_eq!(out, vec!["{\"type\":\"ask_started\"}", "{\"a\":1}"]);
    }

    #[test]
    fn split_across_chunks_reassembles() {
        let mut p = SseLineParser::default();
        assert!(p.push("data: {\"te").is_empty());
        assert!(p.push("xt\":\"he").is_empty());
        let out = p.push("llo\"}\n");
        assert_eq!(out, vec!["{\"text\":\"hello\"}"]);
    }

    #[test]
    fn crlf_is_tolerated() {
        let mut p = SseLineParser::default();
        let out = p.push("data: {\"x\":1}\r\n\r\n");
        assert_eq!(out, vec!["{\"x\":1}"]);
    }
}
