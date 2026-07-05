# Auricle — Architecture

**A local-first meeting transcription engine.** One Rust binary that captures your meeting audio (system loopback + microphone), transcribes it in real time through swappable STT backends, and exposes everything over a WebSocket/REST API. The bundled web UI is just a client — any frontend, script, or plugin can attach to the same engine.

> Auricle: the outer ear — the part of you that listens. It sits on your head, not in the cloud.

---

## 1. Positioning & Defensible Gap

### The two incumbents

| | Meetily | Attendee |
|---|---|---|
| Architecture | Monolithic Tauri desktop app (Rust + Next.js) | Server-side bot fleet (Django/Postgres/Redis) |
| Capture model | Local system audio + mic | Bot joins Zoom/Meet/Teams as a participant |
| STT | Local only (Whisper/Parakeet) | Cloud (Deepgram etc.), swappable |
| Interface | The app IS the product | REST API IS the product |
| Deployment | Desktop installer | Docker + platform OAuth credentials |
| Known weaknesses | Laggy pipeline, heavy frontend, install friction, PRO upsell diverting polish | Not personal-scale; requires cloud infra, bot presence in meetings, cloud STT |

### The gap Auricle occupies

Nobody has shipped the **engine**: local-first, bot-free capture (Meetily's virtue) combined with headless, API-first design and swappable STT (Attendee's virtue), in a single lightweight binary. Auricle is neither a desktop app nor a bot fleet — it is a **daemon with an API**, the way FluxFS is a daemon with a CLI.

### Why Meetily lags (root causes we design against)

1. **Batch-oriented transcription** — large audio chunks processed synchronously → multi-second latency spikes.
2. **Heavy frontend** — Tauri + Next.js re-rendering the full transcript on every update.
3. **Single-process coupling** — audio capture, inference, and UI contend for the same resources.
4. **Model/config friction** — GPU setup and model management baked into app startup.

Every architectural decision below answers at least one of these.

---

## 2. Design Pillars

1. **Engine, not app.** Headless daemon. UI is a client. If the web UI dies, transcription doesn't blink.
2. **Streaming everywhere.** Ring buffer → VAD gate → small rolling windows → partial results pushed over WebSocket. No stage waits for a "full" chunk.
3. **STT is a trait.** Local (Whisper via whisper-rs, Parakeet via ONNX) and cloud (Deepgram streaming, Groq/OpenAI batch) behind one interface, swappable per-session via config or API. User picks their privacy/latency/accuracy point.
4. **Two-channel diarization for free.** Mic stream = "You", loopback stream = "Them". Zero ML cost, correct by construction for the two-party case, honestly documented as not separating multiple remote speakers (v1 limitation; pluggable diarizer is the extension point).
5. **Published latency budget.** Performance is a documented, benchmarked feature (see §8), not an adjective.
6. **Privacy by default, cloud by choice.** Default config is 100% local. Cloud STT/LLM providers are opt-in and clearly labeled in UI and config.

---

## 3. System Overview

```
┌─────────────────────────────── auricle (single binary) ───────────────────────────────┐
│                                                                                        │
│  CAPTURE LAYER                 PIPELINE                        SERVICE LAYER           │
│  ┌──────────────┐   f32 PCM   ┌──────────────┐                ┌───────────────────┐    │
│  │ Mic input    ├──────────►  │ Resampler    │                │ axum HTTP server  │    │
│  │ (cpal)       │             │ (rubato→16k) │                │  REST /api/v1/…   │    │
│  └──────────────┘             └─────┬────────┘                │  WS   /ws/live    │    │
│  ┌──────────────┐                   ▼                         │  Static web UI    │    │
│  │ Loopback     │             ┌──────────────┐  speech spans  └─────────▲─────────┘    │
│  │ (WASAPI)     ├──────────►  │ Silero VAD   ├──────────┐               │ broadcast    │
│  └──────────────┘             │ gate         │          ▼               │ channel      │
│   two independent             └──────────────┘   ┌──────────────┐  ┌────┴─────────┐    │
│   ring buffers                                   │ Chunker      ├─►│ Transcript   │    │
│   (per channel)                                  │ (rolling     │  │ assembler    │    │
│                                                  │  windows +   │  │ partial/final│    │
│                                                  │  overlap)    │  │ + speaker tag│    │
│                                                  └──────┬───────┘  └────┬─────────┘    │
│                                                         ▼               ▼              │
│                                                  ┌──────────────┐  ┌──────────────┐    │
│                                                  │ SttProvider  │  │ SQLite store │    │
│                                                  │ (trait)      │  │ (rusqlite)   │    │
│                                                  ├──────────────┤  └──────────────┘    │
│                                                  │ LocalWhisper │                      │
│                                                  │ Parakeet     │  ┌──────────────┐    │
│                                                  │ Deepgram(ws) │  │ LlmProvider  │    │
│                                                  │ Groq / OAI   │  │ (trait) —    │    │
│                                                  └──────────────┘  │ summaries    │    │
│                                                                    └──────────────┘    │
└────────────────────────────────────────────────────────────────────────────────────────┘
         Clients: bundled web UI • curl/scripts • future TUI • future Obsidian plugin
```

### Process model

- Capture callbacks (cpal/WASAPI) are real-time threads: **allocation-free**, they only write into lock-free ring buffers (`rtrb` crate).
- A pipeline task per channel drains its ring buffer, resamples, runs VAD, and emits speech spans.
- STT runs on a dedicated worker (tokio blocking task for local models; async for cloud streaming) — **inference never blocks capture**.
- The transcript assembler merges per-channel results by timestamp, tags speakers, persists finals to SQLite, and broadcasts partials + finals on a `tokio::sync::broadcast` channel that the WebSocket handler subscribes to.

---

## 4. Component Specifications

### 4.1 Capture layer

- **Mic:** `cpal` default/selected input device.
- **System audio (Windows):** WASAPI loopback. Primary path: `cpal` loopback support on the WASAPI host; fallback: the `wasapi` crate directly. **Phase 1 determines which empirically** — this is the audio equivalent of payload-first: enumerate real devices, record real WAVs, and lock the API choice based on what actually works on the dev machine before any pipeline code exists.
- Each channel produces mono f32 samples at the device's native rate into its own ring buffer. Device sample rate, format, and channel count are logged and recorded in the Phase 1 capture report.
- Device hot-swap / unplug: capture task emits a `DeviceLost` event; session continues with the surviving channel; UI surfaces a warning. (Graceful, not perfect — documented.)

### 4.2 Resampler

- `rubato` sinc resampler to 16 kHz mono (Whisper/Parakeet/Silero native rate). One instance per channel, created once, reused.

### 4.3 VAD gate

- Silero VAD via the `voice_activity_detector` crate (ONNX under the hood); fallback `webrtc-vad` if integration fights back.
- Emits speech spans with hangover padding (e.g., 300 ms pre-roll, 500 ms hangover) so word onsets/offsets aren't clipped.
- Silence is never sent to STT — this is the single biggest lag/cost lever versus naive fixed-interval chunking.

### 4.4 Chunker

- Rolling windows over active speech: emit a chunk when (a) span reaches `max_chunk` (default 5 s) or (b) VAD closes the span. Overlap of ~0.5 s between consecutive chunks within a span; the assembler dedupes overlap via timestamp + longest-common-suffix matching.
- Chunks carry: channel id, session id, monotonic start/end timestamps, sequence number.

### 4.5 SttProvider trait

```rust
#[async_trait]
pub trait SttProvider: Send + Sync {
    fn id(&self) -> &'static str;                       // "whisper-local", "deepgram", ...
    fn kind(&self) -> SttKind;                          // Local | CloudStreaming | CloudBatch
    async fn start_session(&self, cfg: &SessionCfg) -> Result<Box<dyn SttSession>>;
}

#[async_trait]
pub trait SttSession: Send {
    async fn feed(&mut self, chunk: AudioChunk) -> Result<()>;
    async fn next_event(&mut self) -> Option<SttEvent>; // Partial{...} | Final{segment} | Error
    async fn finish(&mut self) -> Result<Vec<Segment>>;
}
```

Implementations, in build order:

| Provider | Crate/API | Mode | Notes |
|---|---|---|---|
| `whisper-local` | `whisper-rs` (whisper.cpp) | pseudo-streaming (chunk-in, text-out) | default: `base.en` or `small.en` GGUF; Vulkan feature flag for GPU |
| `parakeet` | `ort` (ONNX Runtime) + istupakov's parakeet-tdt ONNX | chunk-in, text-out | stretch within Phase 2 or fast-follow; fastest local option |
| `deepgram` | WebSocket API (nova-3), `tokio-tungstenite` | true streaming, native partials | payload-first: capture real WS frames to fixtures before ingestion code |
| `groq-whisper` | REST `audio/transcriptions` | batch per chunk | absurdly fast cloud Whisper |
| `openai-compat` | REST, configurable base URL | batch per chunk | covers OpenAI + any compatible endpoint |

Runtime swap: provider is a per-session setting; changing it mid-session finalizes the current STT session and starts a new one at the next chunk boundary.

### 4.6 Transcript assembler

- Merges per-channel `SttEvent`s into a single ordered transcript by wall-clock timestamps.
- Speaker tags: channel 0 → `You`, channel 1 → `Them` (labels user-renamable per session).
- Partials are broadcast but never persisted; finals are persisted and broadcast with `final: true`.

### 4.7 Storage (SQLite via rusqlite, WAL mode)

```sql
sessions(id TEXT PK, title TEXT, started_at INT, ended_at INT,
         stt_provider TEXT, llm_provider TEXT, meta JSON)
segments(id INTEGER PK, session_id TEXT, channel INT, speaker TEXT,
         t_start_ms INT, t_end_ms INT, text TEXT, provider TEXT)
summaries(id INTEGER PK, session_id TEXT, template TEXT, model TEXT,
          content TEXT, created_at INT)
settings(key TEXT PK, value JSON)
```

Optional raw-audio retention per channel as WAV (config flag, default **off** — privacy default; enables the later "re-transcribe with a different provider" feature).

### 4.8 API surface (axum)

REST (`/api/v1`):
- `GET  /health`, `GET /devices`, `GET /providers`
- `POST /sessions` (start capture; body: title, stt provider, devices) → `201 {id}`
- `POST /sessions/:id/stop`
- `GET  /sessions`, `GET /sessions/:id` (metadata + full transcript)
- `POST /sessions/:id/summarize` (body: template, provider) → summary
- `GET  /sessions/:id/export?format=md`
- `GET/PUT /settings`

WebSocket (`/ws/live`): server pushes JSON events —
`{"type":"partial"|"final","session":..,"speaker":..,"t_start_ms":..,"text":..}` plus `session_started`, `session_stopped`, `device_lost`, `provider_changed`, `error`.

Bind `127.0.0.1` only by default. Config-gated bearer token if binding beyond localhost.

### 4.9 LlmProvider (summaries)

- Single OpenAI-compatible chat-completions client with configurable `base_url` — one implementation covers Ollama, Groq, OpenRouter, OpenAI; plus a native Anthropic variant.
- Summary templates as named markdown prompt files in the config dir (`minutes`, `action-items`, `standup`, `1on1`); user-addable without recompiling.
- Long transcripts: map-reduce (chunk → partial summaries → merge) above a token threshold.

### 4.10 Web UI (the reference client)

- **Vite + React + zustand**, prebuilt and embedded in the binary via `rust-embed`, served by axum. No Tauri, no Electron, no Node at runtime — the anti-lag statement in product form.
- Views: Live (transcript with in-place partial swap→final), Sessions list, Session detail (transcript + summarize button + export), Settings (devices, providers, keys, retention).
- Live transcript is **virtualized** (`@tanstack/react-virtual`); a partial updates a single row in place. Rendering cost is O(visible rows), directly attacking Meetily failure mode #2.
- Design: dark, monospace transcript, VU meters per channel, latency readout in the corner. Screenshot-and-self-critique gate applies to Phase 5.

### 4.11 CLI

`auricle serve` (daemon) • `auricle devices` • `auricle record --stt whisper-local` (headless session, prints finals to stdout) • `auricle transcribe file.wav` (offline file mode — trivially reuses the pipeline, surprisingly demo-friendly) • `auricle export <session-id> --md`.

---

## 5. Crate Layout (Cargo workspace)

```
auricle/
├── Cargo.toml                 # workspace
├── crates/
│   ├── auricle-core/          # types: AudioChunk, Segment, SttEvent, config
│   ├── auricle-capture/       # cpal/wasapi capture, ring buffers, resampler
│   ├── auricle-pipeline/      # VAD, chunker, assembler
│   ├── auricle-stt/           # SttProvider trait + all implementations (feature-gated)
│   ├── auricle-llm/           # LlmProvider + templates
│   ├── auricle-server/        # axum REST/WS, SQLite store, embedded UI
│   └── auricle-cli/           # binary: clap entrypoint wiring everything
├── ui/                        # Vite React app → dist embedded into auricle-server
├── fixtures/                  # captured API payloads (deepgram frames, groq responses)
├── benches/                   # latency benchmark harness + results
└── docs/
```

Feature flags: `whisper` (default), `parakeet`, `cloud` (default), `vulkan`.

---

## 6. Tech Stack Summary

| Concern | Choice |
|---|---|
| Language | Rust (2021), Cargo workspace |
| Audio I/O | `cpal` + `wasapi` (Windows loopback), `rtrb` ring buffers, `rubato` resampling, `hound` for WAV |
| VAD | Silero via `voice_activity_detector` (fallback `webrtc-vad`) |
| Local STT | `whisper-rs`; Parakeet via `ort` |
| Async | `tokio`; `tokio-tungstenite` for Deepgram; `reqwest` for batch APIs |
| Server | `axum`, `tower-http`, `rust-embed` |
| Storage | `rusqlite` (WAL) |
| Serialization/validation | `serde`, `schemars` for config |
| CLI | `clap` |
| UI | Vite + React + TypeScript + zustand + @tanstack/react-virtual |
| Testing | `cargo test` + fixture-driven provider tests; `vitest` for UI logic |

---

## 7. Configuration

`auricle.toml` in the platform config dir (overridable via `--config`):

```toml
[audio]
mic_device = "default"
loopback_device = "default"
retain_raw_audio = false

[stt]
provider = "whisper-local"
[stt.whisper-local]
model = "small.en"          # auto-downloaded to data dir on first use, with checksum
[stt.deepgram]
api_key_env = "DEEPGRAM_API_KEY"   # keys via env only; never stored in SQLite or TOML

[llm]
provider = "ollama"
base_url = "http://localhost:11434/v1"
model = "llama3.1"

[server]
bind = "127.0.0.1:4820"
```

---

## 8. Performance Budget (published in README)

| Metric | Target | How measured |
|---|---|---|
| Capture → partial visible (cloud streaming) | < 1.0 s | bench harness: injected tone-marked WAV, timestamp diff |
| Capture → final segment (local `small.en`, CPU) | < 2.5 s after speech pause | same |
| Idle CPU (capturing, silence) | < 3 % | perf counters, 5-min silence run |
| RAM (whisper small.en loaded) | < 1.5 GB | working set |
| UI frame time during live transcript | < 16 ms | React profiler |
| Binary size (default features) | < 40 MB + models | release build |

Numbers are measured on named hardware and published as a table with the benchmark harness in-repo. Misses are documented with root causes, per house rules.

---

## 9. Risks & Mitigations

| Risk | Mitigation |
|---|---|
| WASAPI loopback quirks (exclusive-mode apps, sample-rate mismatches) | Phase 1 is *only* capture + WAV verification; pick cpal vs `wasapi` crate empirically; document unsupported cases |
| whisper-rs build pain on Windows (CMake/LLVM) | Pin versions; CPU-only default build; Vulkan behind a feature flag; document toolchain in BUILDING.md |
| Latency targets missed on CPU | `base.en` fallback preset; Parakeet ONNX path; honest benchmark table either way |
| Two-channel diarization oversold | Never claim "diarization" in README headline; call it "speaker channels"; list multi-remote-speaker separation as a v2 pluggable |
| Scope creep toward Meetily-clone feature list | The API is the product; UI features beyond §4.10 are out of scope for v1 |
| macOS demand | Explicit roadmap entry; capture layer is the only platform-specific crate, isolated in `auricle-capture` |

---

## 10. Out of Scope (v1)

Meeting-bot joining (that's Attendee's lane) • ML diarization • macOS/Linux capture (roadmap; file-transcribe mode works everywhere) • calendar integration • auto meeting detection • Electron/Tauri anything.

---

## 11. Ship Definition

GitHub repo with README (hero demo GIF, benchmark table, architecture diagram, honest limitations section) • `cargo install auricle` from crates.io • prebuilt Windows release binary in GitHub Releases • BUILDING.md • LinkedIn post • repo topics: `rust`, `speech-to-text`, `whisper`, `local-first`, `meeting-notes`, `privacy`, `real-time`.
