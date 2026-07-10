# Changelog

## 0.1.0 (unreleased)

Hardening pass (post-UI-redesign):

- **Security:** same-origin enforcement for browser requests — foreign
  pages can no longer open `/ws/live` and read transcripts (WebSocket
  handshakes bypass CORS); tokenless loopback binds now also reject
  non-localhost `Host` headers (DNS rebinding).
- **Correctness:** crash recovery moved out of `Store::open` into the
  engine — `auricle export` during a live recording no longer marks the
  active session interrupted; `summarize` now honors the settings-store
  default LLM provider.
- **Robustness:** session stop is bounded (a wedged provider can't stick
  the lifecycle in `stopping` forever); `auricle serve` stops the active
  session cleanly on Ctrl+C instead of leaving it for crash recovery.
- **Performance:** whisper-local models load once and are cached across
  sessions (starts were re-reading 148–500 MB); the channel driver is
  select-based, removing up to 50 ms of polling latency from the
  capture→partial path; retained audio is 16-bit PCM (half the disk) and
  served with HTTP Range support (streamed, seekable playback).
- **Visibility:** first-run model downloads announce themselves over the
  WebSocket (the UI shows progress instead of a silent hang); queue
  shedding is reported live (throttled), not only at session end.

Initial release: the engine, end to end.

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
- **Web UI (embedded):** virtualized live transcript with single-row
  partial updates, VU meters, provider pickers, sessions browser, settings
  with config overlay, auto-reconnect with state resync — one self-contained
  binary, no Node at runtime.
- **Summaries:** OpenAI-compatible LLM client (Ollama/Groq/any base_url),
  four overridable templates (minutes, action-items, standup, 1on1),
  map-reduce for long transcripts, summaries persisted and included in
  exports.
- **Benchmarks:** real-time-paced latency harness; honest numbers and
  budget misses in benches/RESULTS.md.
