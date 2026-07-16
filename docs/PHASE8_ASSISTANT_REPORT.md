# Phase 8 Assistant Report — Context Assembly + Streaming Answers

Payload-first results and live verification for the assistant service:
`POST /api/v1/ask` streams an LLM answer assembled from question +
on-demand screen OCR + a rolling transcript window. Everything below was
measured on the dev machine on 2026-07-15; raw SSE captures live in
`fixtures/llm-stream/` (see its README for the file map).

**Machine:** Windows 11 Home 10.0.26200 · i7-9750H · Ollama 0.32.0
(qwen3:8b, qwen2.5:14b, gemma4 pulled) · Groq llama-3.3-70b-versatile ·
Deepgram nova-3 for the live-session STT.

---

## Streaming payload capture (payload-first)

`stream: true` chat completions captured verbatim (every SSE frame, the
usage frame, the `[DONE]` sentinel) from Ollama's OpenAI-compat endpoint
and Groq, with and without `stream_options: {"include_usage": true}`,
plus error-shape probes. The SSE parser (`auricle-llm/src/stream.rs`)
was derived from these captures, and its unit tests replay them —
including byte-at-a-time chunking to prove split-frame handling.

## Doc-vs-reality discrepancies

1. **Ollama repeats `delta.role: "assistant"` in EVERY chunk.** The
   OpenAI spec (and Groq) sends `role` only in the first chunk. Harmless
   for a tolerant parser; fatal for one that treats "has role" as
   "stream start".
2. **Ollama reasoning models stream their entire thinking phase as
   `delta.reasoning` with `delta.content: ""`.** On qwen3:8b, 185 of the
   209 delta frames in the capture were pure chain of thought; content
   stayed empty for the whole thinking phase and there is no marker
   frame at the transition. Two failure modes for naive consumers: emit
   every delta → hundreds of empty fragments; concatenate any non-null
   delta field → chain-of-thought leaks into the answer. The parser
   never reads `reasoning` and filters empty `content` (fixture-locked).
3. **Ollama sends no usage frame unless `stream_options:
   {"include_usage": true}` is requested.** With it, a final
   `"choices": []` frame carries `usage` before `[DONE]`. The client
   always sends `include_usage` (Groq tolerates it).
4. **Groq's finish chunk has an EMPTY `delta: {}`** — no `content` key
   at all (Ollama's finish chunk has `content: ""` instead) — **and
   carries `usage` twice** (top level and under `x_groq`) **even when
   `stream_options` was never sent.** Usage is therefore available from
   Groq unconditionally; the parser reads the top-level field on any
   frame.
5. **With `include_usage`, Groq adds a third usage location**: an extra
   `"choices": []` frame with top-level `usage` and
   `service_tier: "on_demand"`, after the finish chunk.
6. **`stream: true` errors are not streams.** Unknown-model requests
   return plain JSON error bodies with real HTTP statuses (404) on both
   providers (`*_badmodel_error.txt`). The client checks HTTP status
   before SSE-parsing; a pre-flight failure surfaces as a clean error,
   not a broken stream.
7. **Both providers emit bare-LF SSE** (`\n\n` separators, no CRLF
   anywhere). The parser tolerates CRLF anyway (legal per the SSE spec)
   and joins multi-line `data:` fields, though neither was observed.
8. **Groq attaches `x_groq: {id, seed}` to the first chunk only** —
   ignored, like the batch client ignores its other extensions.

## Live self-verification (real session, both providers)

A real recording session (Deepgram STT, WASAPI loopback) captured SAPI
TTS played through the system default output; all spoken sentences
transcribed correctly. Asks were issued with
`include_screen: true, include_transcript: true` and the question
*"what was just being discussed and what's on my screen?"* while a
window (a text-dense briefing page) was foreground. The
`curl -N` exchange is reproduced in API.md; every answer was verified
grounded in BOTH sources (transcript facts: Aurora project, $320,000
budget, two Rust engineers; screen facts: the briefing's action items).

The WS mirror was verified with a raw `ClientWebSocket` listener on
`/ws/live`: `answer_delta` frames and `answer_done` (with usage
`{prompt: 607, completion: 69, total: 676}`) arrived interleaved with
the session's `vu`/`final` events, same `ask_id` as the SSE stream.

A follow-up ask (`follow_up: true`, no context flags) answered *"what
exact dollar figure did you cite for the budget in your previous
answer"* correctly ("$320,000 … from the earlier discussion") in 498 ms
TTFT — the prior Q/A was recalled from in-memory history alone.

### Time-to-first-token (request sent → first `answer_delta`)

| Provider / model | TTFT | Budget | Verdict |
|---|---|---|---|
| Groq llama-3.3-70b-versatile (warm) | **677 ms** (total answer 844 ms) | < 1.5 s | **PASS** |
| Groq, first ask in a fresh daemon | ~6.2 s after `ask_started` | < 1.5 s | miss, once: cold DNS+TLS to api.groq.com and free-tier queueing; subsequent asks 0.5–0.7 s |
| Ollama qwen2.5:14b (model resident) | **10.1 s** (total 41.5 s) | < 4 s | **MISS** — prompt evaluation: the ~600-token assembled prompt must be ingested before the first token; this CPU/GPU class evaluates it in ~9 s and generates at ~2.5 tok/s |
| Ollama qwen3:8b (reasoning model) | **104 s** (total 155 s) | < 4 s | **MISS** — the model *thought* for ~100 s; every thinking token streams as `delta.reasoning`, which must never be surfaced, so the first *visible* token arrives only when thinking ends |

Honest reading: the < 4 s local budget is unreachable with the models
pulled on this machine. Root causes are structural, not code: (a)
reasoning models put their entire thinking time in front of the first
answer token — a reasoning model is the wrong default for a latency-
sensitive copilot; (b) prompt-eval throughput on laptop hardware
dominates TTFT for 14B-class models. A small non-reasoning model
(3B-class) would likely pass, but none is pulled here and the
payload-first rule forbids claiming unmeasured numbers. Groq passes
with 2× headroom, streaming its whole answer in under a second.
Model-residency cost (Ollama loading qwen2.5:14b: 26 s) is excluded —
that is Ollama keep-alive policy, paid once per idle window, and is
reported here instead of hidden in the TTFT number.

Ask overhead outside the LLM: TTFB (request → SSE headers, which
includes the on-demand screen capture + OCR + prompt assembly) was
520–590 ms warm, with OCR itself 15–93 ms — the hotkey→context budget
from Phase 7 continues to hold inside the ask path.

### Measurement pitfall worth recording

.NET `HttpWebRequest` (the PowerShell measurement client) silently
spends ~11 s on WPAD proxy auto-discovery before its FIRST request to a
new host — the initial "TTFB" read 11.6 s while the server was idle.
`Proxy = $null` removed it. Any future timing harness on Windows must
disable proxy resolution or the numbers measure the client, not Auricle.

### retain_context = false guarantee (default)

After five real asks in the live daemon (several with screen capture),
the production database schema was inspected directly:

```
tables: ['segments', 'sessions', 'settings', 'summaries']
asks table present: False
```

The `asks` table is deliberately NOT part of the versioned migrations —
`insert_ask` creates it on first use, and the only call site is gated on
`copilot.retain_context`. With retention off (default), the schema
itself carries no trace of the copilot. Integration tests lock in both
directions (`retain_context_off_by_default_persists_nothing`,
`retain_context_on_persists_question_screen_and_answer`).

## Design decisions locked

- **`chat_stream` is a defaulted trait method**: fragments through an
  `mpsc::Sender`, usage returned at completion. The default wraps the
  existing non-streaming `complete()` (one fragment), so every provider
  streams; the OpenAI-compat client overrides with real SSE. The
  summarize path is untouched — its tests pass unmodified.
- **A stream that ends with no content is an error**, mirroring
  `complete()`'s empty-completion error — a reasoning model can think
  its way to `finish_reason: "stop"` without ever answering.
- **Prompt budget**: 24 000 chars (~6k tokens at the ~4 chars/token
  heuristic used across auricle-llm), allocated in priority order
  question → follow-up history (newest 3 pairs, answers capped at
  1 500 chars) → screen (capped at `copilot.max_screen_chars`) →
  transcript (newest segments kept, rendered chronologically) → session
  metadata. Rendering order is the reverse (context first, question
  last). An empty transcript window is stated in the prompt, not
  omitted, so the model doesn't invent one.
- **Ask history** is in-memory per `session_id` (one global bucket for
  session-less asks), capped at 8 pairs, process-lifetime.
- **Transcript ring** windows on wall-clock arrival time (`last N
  minutes of real time`), so an ask right after a session stops still
  has context; it subscribes to the same broadcast lane as the WS.
- **`answer_delta` mirrors ride the deep (finals) WS lane** — a dropped
  answer fragment would corrupt the visible answer, so deltas get the
  never-dropped policy, not the lossy partial lane.
- **The SSE response opens with an `ask_started` event** echoing what
  context was actually captured (window title, app, OCR ms, transcript
  segment count). This is the honesty affordance at API level — the
  Phase 9 context chips derive from it — and it made the acceptance
  verification ("does the answer use both sources?") checkable from a
  bare curl.
- **`exclude_hwnd` is plumbed** from the ask body through to
  `auricle-vision` (integration-tested); nothing sets it until the
  Phase 9 overlay passes its own HWND.
- Vision failures fail the ask loudly with the /peek status mapping
  (409 transient / 500 machine-level) and an `ask_error` WS mirror —
  a copilot that silently answers without the screen you asked it to
  look at would be lying about its context.

## Deviations from the daemon defaults observed in verification

- The settings DB on this machine pins `loopback_device` to a headset
  that was not the actual default output; TTS played through the
  current default (an Echo Dot). The verification session passed
  `loopback_device: "default"` explicitly. Not a Phase 8 defect —
  recorded because the first session captured silence until diagnosed.
- `[llm.ollama]` defaults to `llama3.1`, which is not pulled here; the
  settings overlay selects `qwen3:8b`. Verification pinned models
  explicitly per run.
