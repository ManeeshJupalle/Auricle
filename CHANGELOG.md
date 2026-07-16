# Changelog

## 0.3.0 — 2026-07-15 (first public release)

One launch, three layers: the local-first transcription engine, the
embedded dashboard, and a screen-aware desktop copilot built on the
same public API.

### Engine

- **Capture (Windows):** simultaneous microphone + system-audio (WASAPI
  loopback) capture via cpal; lock-free ring buffers; 16 kHz resampling;
  gap-aware timestamps for event-driven loopback silence.
- **Pipeline:** Silero VAD gating (300 ms pre-roll / 500 ms hangover),
  rolling-window chunker with overlap dedup, two-channel speaker labeling
  (You/Them), tokio broadcast fan-out.
- **STT providers behind one trait:** whisper-local (whisper.cpp, model
  auto-download with SHA-256 verification), Deepgram nova-3 streaming
  (KeepAlive gap bridging, reconnect with backoff), Groq Whisper and
  OpenAI-compatible batch (retry with jitter); runtime provider cycling in
  the CLI.
- **Daemon:** axum REST + WebSocket API, SQLite (WAL) persistence with
  crash recovery, session lifecycle with 409 on concurrent starts,
  localhost-default bind with bearer-token middleware for remote binds,
  markdown export.
- **Security:** same-origin enforcement for browser requests — foreign
  pages cannot open `/ws/live` and read transcripts (WebSocket handshakes
  bypass CORS); tokenless loopback binds reject non-localhost `Host`
  headers (DNS rebinding).
- **Screen peek (`auricle-vision`):** on-demand single-frame capture of
  the active window (Windows.Graphics.Capture) + local OCR
  (Windows.Media.Ocr) behind a `ScreenReader` trait; reading-order
  flattening; typed errors, never panics; warm capture→text 175–298 ms at
  1080p. `POST /api/v1/peek` and `auricle peek [--json]`. Strictly on
  demand: no capture loops, nothing screen-derived persisted, no
  concealment APIs anywhere.
- **Performance:** whisper models load once and are cached across
  sessions; select-based channel driver (up to 50 ms less capture→partial
  latency); retained audio is 16-bit PCM served with HTTP Range support;
  bounded session stop; first-run model downloads and queue shedding are
  announced over the WebSocket.
- **Benchmarks:** real-time-paced latency harness; honest numbers and
  budget misses in benches/RESULTS.md.

### Dashboard (embedded web UI)

- Virtualized live transcript with single-row partial updates, VU meters,
  latency readout, provider pickers.
- Sessions browser with full-text search (titles + transcript content),
  inline rename, delete, markdown export.
- LLM auto-titles after stop (offline fallback: first words of the
  transcript).
- Synchronized audio playback with click-timestamp-to-seek for sessions
  recorded with raw-audio retention (off by default).
- Dark/light themes; one self-contained binary, no Node at runtime.

### Summaries

- OpenAI-compatible LLM client (Ollama / Groq / any base_url), keys via
  environment only.
- Four overridable templates (minutes, action-items, standup, 1on1) plus
  user-added `.md` templates without recompiling.
- Map-reduce for long transcripts; summaries persisted and appended to
  exports.

### Copilot (assistant service + overlay)

- **`POST /api/v1/ask`:** streams an LLM answer assembled from the
  question, an on-demand screen capture, and a rolling in-memory
  transcript window (last 10 min, configurable) — SSE on the response,
  mirrored as `answer_delta` / `answer_done` / `ask_error` on `/ws/live`.
  Follow-up questions see in-memory ask history. Streaming SSE parser
  derived from captured Ollama/Groq frames (reasoning-model chain of
  thought is never surfaced).
- **Privacy default:** nothing question- or screen-derived is persisted
  unless `copilot.retain_context = true`; with it off the database schema
  never even grows the table.
- **Overlay (`overlay/`, Tauri v2):** Ctrl+Shift+Space summons an
  always-on-top ask card in ~100 ms; Ctrl+Shift+A is one-keystroke
  quick assist (screen + transcript); Esc dismisses. Context chips name
  exactly what was captured. Streaming markdown answers, copy button,
  follow-ups, provider + elapsed-time readout. Tray icon starts/stops
  recording, opens the dashboard, and spawns the engine if it isn't
  running. The overlay is a normal, visible window — no concealment
  APIs — and talks only to the public API, passing its own window
  handle so captures never include the overlay itself.
- **MSI installer:** per-user by default (no elevation), engine bundled
  as a sidecar; per-machine via documented msiexec flags.

Measured on the reference laptop (i7-9750H): capture→partial 0.14 s
(Deepgram p50), hotkey→overlay visible 91–122 ms, ask time-to-first-token
0.7 s (Groq) — local-model copilot numbers and their misses are
documented in docs/PHASE8_ASSISTANT_REPORT.md.
