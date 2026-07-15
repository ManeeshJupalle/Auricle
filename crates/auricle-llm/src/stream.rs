//! SSE parsing for `stream: true` chat completions.
//!
//! Derived from raw frames captured verbatim from live endpoints into
//! `fixtures/llm-stream/` (Ollama 0.32.0: qwen3:8b, qwen2.5:14b; Groq:
//! llama-3.3-70b-versatile — see the fixtures README and
//! docs/PHASE8_ASSISTANT_REPORT.md). Reality the parser is built on:
//!
//! - Frames are `data: <json>` lines separated by blank lines, bare LF
//!   endings, closed by a literal `data: [DONE]` sentinel.
//! - Ollama repeats `delta.role` in EVERY chunk and its reasoning models
//!   (qwen3) stream the entire thinking phase as `delta.reasoning` with
//!   `delta.content: ""` — reasoning must never be surfaced, and empty
//!   content strings must not become deltas.
//! - Groq's `finish_reason: "stop"` chunk has an EMPTY `delta: {}` (no
//!   content key at all) and carries `usage` twice (top level and under
//!   `x_groq`) even when `stream_options` was never sent; with
//!   `include_usage` both providers add a final `choices: []` frame.
//!
//! The parser is intentionally tolerant: unknown fields are ignored,
//! `choices` may be empty, and CRLF line endings are accepted (not
//! observed in captures, but allowed by the SSE spec).

use serde::{Deserialize, Serialize};

/// Token usage reported by a provider at stream end. Groq attaches timing
/// extras (queue_time, prompt_time, …) — ignored, as in the batch client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// The `data: [DONE]` sentinel payload.
pub const DONE_SENTINEL: &str = "[DONE]";

/// Incremental SSE event splitter: feed raw response bytes in any
/// chunking, get back complete `data:` payloads in order. Handles events
/// split across network chunks (byte-at-a-time verified in tests).
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// data lines of the event currently being accumulated.
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns each completed event's `data` payload
    /// (multi-line data joined with `\n` per the SSE spec).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        // Consume complete lines; the remainder stays buffered until the
        // next push.
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // the \n
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if line.is_empty() {
                // Blank line: event boundary.
                if !self.data.is_empty() {
                    out.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(payload) = line.strip_prefix("data:") {
                self.data
                    .push(payload.strip_prefix(' ').unwrap_or(payload).to_string());
            }
            // Other field lines (`event:`, `id:`, comments) are ignored —
            // none were observed in the captures.
        }
        out
    }
}

/// What one parsed frame contributes to the answer.
#[derive(Debug, Default, PartialEq)]
pub struct FrameOut {
    /// Non-empty content fragment to append to the answer.
    pub delta: Option<String>,
    /// `finish_reason` when the provider sent one.
    pub finish: Option<String>,
    /// Usage when this frame carried it (Groq: on the finish chunk even
    /// without `stream_options`; both: on the `choices: []` usage frame).
    pub usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct StreamFrame {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    finish_reason: Option<String>,
}

/// `role` (Ollama: every chunk) and `reasoning` (Ollama qwen3: the whole
/// thinking phase) exist on the wire but are deliberately not read —
/// chain of thought must never reach the answer.
#[derive(Deserialize, Default)]
struct StreamDelta {
    content: Option<String>,
}

/// Interpret one `data:` JSON payload. The caller filters the
/// [`DONE_SENTINEL`] before calling this.
pub fn parse_stream_frame(payload: &str) -> Result<FrameOut, serde_json::Error> {
    let frame: StreamFrame = serde_json::from_str(payload)?;
    let mut out = FrameOut {
        usage: frame.usage,
        ..FrameOut::default()
    };
    if let Some(choice) = frame.choices.into_iter().next() {
        out.finish = choice.finish_reason;
        // Empty strings are structural (Ollama reasoning chunks, both
        // providers' first/last chunks), not answer text.
        if let Some(content) = choice.delta.content.filter(|c| !c.is_empty()) {
            out.delta = Some(content);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_bytes(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/llm-stream")
            .join(name);
        std::fs::read(path).expect("fixture present")
    }

    /// Run a fixture through the parser and interpret every frame,
    /// returning (concatenated answer, usage, finish_reason, saw_done).
    fn replay(
        bytes: &[u8],
        chunk_size: usize,
    ) -> (String, Option<TokenUsage>, Option<String>, bool) {
        let mut parser = SseParser::new();
        let mut answer = String::new();
        let mut usage = None;
        let mut finish = None;
        let mut done = false;
        for chunk in bytes.chunks(chunk_size) {
            for payload in parser.push(chunk) {
                assert!(!done, "no payloads may follow [DONE]");
                if payload == DONE_SENTINEL {
                    done = true;
                    continue;
                }
                let out = parse_stream_frame(&payload).expect("captured frame parses");
                if let Some(d) = out.delta {
                    assert!(!d.is_empty(), "empty deltas must be filtered");
                    answer.push_str(&d);
                }
                if out.usage.is_some() {
                    usage = out.usage;
                }
                if out.finish.is_some() {
                    finish = out.finish;
                }
            }
        }
        (answer, usage, finish, done)
    }

    #[test]
    fn qwen25_fixture_replays_to_the_answer_and_usage() {
        let bytes = fixture_bytes("ollama_qwen25_usage_stream.txt");
        let (answer, usage, finish, done) = replay(&bytes, bytes.len());
        assert_eq!(
            answer,
            "A meeting transcription tool converts spoken words in a meeting into written text."
        );
        assert_eq!(
            usage,
            Some(TokenUsage {
                prompt_tokens: 32,
                completion_tokens: 15,
                total_tokens: 47
            })
        );
        assert_eq!(finish.as_deref(), Some("stop"));
        assert!(done);
    }

    #[test]
    fn byte_at_a_time_chunking_changes_nothing() {
        // Network chunk boundaries are arbitrary; the parser must produce
        // identical output fed one byte at a time.
        let bytes = fixture_bytes("groq_usage_stream.txt");
        assert_eq!(replay(&bytes, 1), replay(&bytes, bytes.len()));
        let bytes = fixture_bytes("ollama_qwen25_usage_stream.txt");
        assert_eq!(replay(&bytes, 1), replay(&bytes, 7));
    }

    #[test]
    fn qwen3_reasoning_never_leaks_into_the_answer() {
        // 185 of this capture's 209 delta frames are pure chain of thought
        // (delta.reasoning with content: ""). The answer must contain none
        // of it and no empty fragments.
        let bytes = fixture_bytes("ollama_qwen3_usage_stream.txt");
        let (answer, usage, finish, done) = replay(&bytes, 1024);
        assert!(
            answer.starts_with("A meeting transcription tool"),
            "{answer}"
        );
        assert!(!answer.contains("Okay"), "reasoning leaked: {answer}");
        assert_eq!(usage.as_ref().map(|u| u.completion_tokens), Some(211));
        assert_eq!(finish.as_deref(), Some("stop"));
        assert!(done);
    }

    #[test]
    fn groq_delivers_usage_even_without_stream_options() {
        // Reality over docs: Groq's finish chunk carries top-level usage
        // unconditionally (fixtures groq_plain_stream.txt).
        let bytes = fixture_bytes("groq_plain_stream.txt");
        let (answer, usage, finish, done) = replay(&bytes, 512);
        assert!(!answer.is_empty());
        let usage = usage.expect("usage on the finish chunk");
        assert_eq!(usage.prompt_tokens, 54);
        assert_eq!(usage.total_tokens, 75);
        assert_eq!(finish.as_deref(), Some("stop"));
        assert!(done);
    }

    #[test]
    fn groq_empty_finish_delta_and_first_chunk_produce_no_fragments() {
        // First chunk: delta {role, content: ""}; finish chunk: delta {}.
        let first = r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],"x_groq":{"id":"req","seed":1}}"#;
        let out = parse_stream_frame(first).unwrap();
        assert_eq!(out, FrameOut::default());

        let finish = r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
        let out = parse_stream_frame(finish).unwrap();
        assert_eq!(out.delta, None);
        assert_eq!(out.finish.as_deref(), Some("stop"));
        assert_eq!(out.usage.map(|u| u.total_tokens), Some(3));
    }

    #[test]
    fn crlf_and_multiline_data_are_tolerated() {
        // Not observed in captures, but legal SSE — must not corrupt.
        let mut parser = SseParser::new();
        let events = parser.push(b"data: {\"a\":1}\r\n\r\ndata: line1\ndata: line2\n\n");
        assert_eq!(
            events,
            vec!["{\"a\":1}".to_string(), "line1\nline2".to_string()]
        );
    }

    #[test]
    fn error_bodies_are_not_sse_and_fail_frame_parsing_cleanly() {
        // stream:true with an unknown model returns a plain JSON error body
        // (HTTP 404) on both providers — captured in *_badmodel_error.txt.
        // The client checks HTTP status first; if such a body ever reached
        // the frame parser it must error, not panic or emit deltas.
        for name in ["ollama_badmodel_error.txt", "groq_badmodel_error.txt"] {
            let body = String::from_utf8(fixture_bytes(name)).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(parsed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("model"));
            // No `data:` framing → the SSE splitter yields nothing.
            let mut parser = SseParser::new();
            assert!(parser.push(body.as_bytes()).is_empty());
        }
    }
}
