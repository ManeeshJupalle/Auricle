# LLM streaming fixtures

Raw SSE bodies captured verbatim (`curl.exe -sS --no-buffer -o <file>`)
from live `stream: true` chat-completion endpoints on 2026-07-15. Every
byte of the response body is preserved: `data:` lines, blank separators,
usage frames, `data: [DONE]` sentinels. The `*_headers.txt` files carry
the matching HTTP response headers. The streaming parser in
`auricle-llm/src/stream.rs` is derived from these files, not from
documentation (see docs/PHASE8_ASSISTANT_REPORT.md for the
doc-vs-reality findings).

| File | Provider / model | Request |
|---|---|---|
| `ollama_qwen3_stream.txt` | Ollama 0.32.0, qwen3:8b (reasoning model) | `stream: true` |
| `ollama_qwen3_usage_stream.txt` | Ollama 0.32.0, qwen3:8b | + `stream_options: {include_usage: true}` |
| `ollama_qwen25_usage_stream.txt` | Ollama 0.32.0, qwen2.5:14b | + `stream_options: {include_usage: true}` |
| `groq_usage_stream.txt` | Groq, llama-3.3-70b-versatile | + `stream_options: {include_usage: true}` |
| `groq_plain_stream.txt` | Groq, llama-3.3-70b-versatile | `stream: true` |
| `ollama_badmodel_error.txt` | Ollama, unknown model | `stream: true` → HTTP 404, plain JSON |
| `groq_badmodel_error.txt` | Groq, unknown model | `stream: true` → HTTP 404, plain JSON |

Endpoints: `http://localhost:11434/v1/chat/completions` and
`https://api.groq.com/openai/v1/chat/completions`. All bodies use bare
LF line endings as received. API keys were sent via header only and do
not appear in any fixture.
