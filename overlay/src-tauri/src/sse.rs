//! Minimal SSE line parser for the engine's /ask stream. The engine
//! emits one single-line `data: <json>` per event with blank-line
//! separators and SSE keep-alive comments (`:`) during silent stretches
//! (fixture-verified in Phase 8); this parser handles exactly that shape,
//! byte-split-safe across network chunks.

#[derive(Default)]
pub struct SseLineParser {
    buf: Vec<u8>,
}

impl SseLineParser {
    /// Feed raw bytes; returns the `data:` payloads of every line completed
    /// by them. Comments, blank lines, and other fields are skipped. A
    /// chunk can end inside a multibyte character, so nothing is decoded
    /// until its line is whole: `\n` never occurs inside a UTF-8 sequence.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
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
        let out =
            p.push(b"data: {\"type\":\"ask_started\"}\n\n: keep-alive\n\ndata: {\"a\":1}\n\n");
        assert_eq!(out, vec!["{\"type\":\"ask_started\"}", "{\"a\":1}"]);
    }

    #[test]
    fn split_across_chunks_reassembles() {
        let mut p = SseLineParser::default();
        assert!(p.push(b"data: {\"te").is_empty());
        assert!(p.push(b"xt\":\"he").is_empty());
        let out = p.push(b"llo\"}\n");
        assert_eq!(out, vec!["{\"text\":\"hello\"}"]);
    }

    #[test]
    fn crlf_is_tolerated() {
        let mut p = SseLineParser::default();
        let out = p.push(b"data: {\"x\":1}\r\n\r\n");
        assert_eq!(out, vec!["{\"x\":1}"]);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        // Every split point, including inside the 2- and 4-byte characters.
        let line = "data: {\"text\":\"caf\u{e9} \u{1f3a7}\"}\n".as_bytes();
        for split in 1..line.len() {
            let mut p = SseLineParser::default();
            let mut out = p.push(&line[..split]);
            out.extend(p.push(&line[split..]));
            assert_eq!(
                out,
                vec!["{\"text\":\"caf\u{e9} \u{1f3a7}\"}"],
                "split {split}"
            );
        }
    }
}
