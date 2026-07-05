# Changelog

## 0.1.0 (unreleased)

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
