# Auricle

<!-- HERO GIF (copilot): docs/demo_copilot.gif — quick-assist over a real meeting: hotkey → context chips → streaming answer → follow-up → Esc -->

**A local-first meeting engine — not another meeting app — that ships
with a desktop copilot.** Auricle is a single Rust binary that captures
your meeting audio (system loopback + microphone, no bot joining your
calls), transcribes it in real time through swappable STT backends, and
exposes everything over a REST/WebSocket API. On top of that API sit two
bundled clients: an embedded web dashboard, and an always-on-top copilot
overlay that can answer "what's happening right now?" from the live
transcript and whatever is on your screen. Privacy by default: with the
local Whisper backend, no audio ever leaves your machine.

> *Auricle: the outer ear — the part of you that listens. It sits on your
> head, not in the cloud.*

Three things, one engine:

- **📝 Meeting notes** — live two-speaker transcript, LLM auto-titles,
  one-click summaries, full-text search, markdown export, optional
  synchronized audio playback.
- **⚡ Copilot** — `Ctrl+Shift+A` mid-meeting: one keystroke captures
  the active window (local OCR) + the last 10 minutes of transcript and
  streams an answer into a small overlay, with context chips naming
  exactly what was captured.
- **🔌 API** — everything above is `curl`-able: REST + WebSocket +
  SSE on one localhost port. The UIs are just clients; yours can be too.

## Why

The two incumbent shapes both have the same flaw for personal use: meeting
apps (Meetily) couple capture, inference, and a heavy desktop UI into one
laggy process, and bot platforms (Attendee) put a visible bot in your
meeting and your audio in their cloud. Auricle takes Meetily's virtue
(local, bot-free capture) and Attendee's virtue (API-first, swappable STT)
and ships them as one lightweight daemon.

|  | Meetily | Attendee | **Auricle** |
|---|---|---|---|
| Capture | local system audio + mic | bot joins the meeting | local system audio + mic |
| STT | local only | cloud, swappable | **local + cloud, swappable per session** |
| Interface | the desktop app | REST API | **REST/WS API; dashboard + overlay are just clients** |
| Runtime | Tauri + Next.js | Django/Postgres/Redis fleet | **one ~31 MB binary, SQLite** |
| Audio leaves your machine | no | yes | **only if you opt into a cloud provider** |

## Quickstart

**Engine + dashboard:**

```
# 1. Get auricle:  cargo install auricle-cli   (or grab auricle.exe from Releases)
auricle devices          # see your microphones + loopback outputs
auricle serve            # daemon + web dashboard on http://127.0.0.1:4820

# or skip the server entirely:
auricle record --stt whisper-local --model base.en   # live transcript in your terminal
auricle peek             # OCR the window you're looking at (local, on-demand)
```

Open `http://127.0.0.1:4820`, press **Start recording**, play your meeting —
the system-audio side is labeled **Them**, your microphone is **You**.
Sessions get LLM auto-titles after stop, full-text search in the sidebar,
synchronized audio playback when raw-audio retention is on, and one-click
summaries (minutes, action items, standup, 1:1) with a local (Ollama) or
cloud LLM.

**Copilot:** install `Auricle Copilot_0.3.0_x64_en-US.msi` from Releases
(per-user, no admin prompt; the engine is bundled and spawns
automatically). Then `Ctrl+Shift+Space` summons the ask card,
`Ctrl+Shift+A` asks "what's happening?" in one keystroke, `Esc`
dismisses. The tray icon starts/stops recording and opens the dashboard.

Cloud providers are opt-in via environment variables: `DEEPGRAM_API_KEY`
(streaming STT, the low-latency option), `GROQ_API_KEY` (batch Whisper STT
+ LLM answers/summaries), or any OpenAI-compatible endpoint via config.
Local Whisper models download automatically with SHA-256 verification on
first use.

## The engine

<!-- ENGINE GIF: docs/demo_engine.gif — live dashboard transcript with a mid-sentence provider swap and the latency readout -->

```
 mic ──cpal──► ring buffer ─► resample 16k ─► Silero VAD ─► chunker ─┐
                                                                     ├─► STT provider ─► assembler ─► SQLite
 system audio ─WASAPI loopback─► ring buffer ─► … (same pipeline) ───┘   (trait object)      │
                                                                                             ├─► REST /api/v1
        whisper-local │ deepgram │ groq-whisper │ openai-compat                              └─► WS /ws/live ─► clients
```

- Capture callbacks are allocation-free and lock-free (rtrb ring buffers).
- Silence never reaches STT: Silero VAD gates the stream, with gap-aware
  timestamps (WASAPI loopback delivers *nothing* while the system is
  silent — measured, not assumed).
- Two-channel speaker labels come free from the capture topology.
- The daemon defends its localhost boundary: browser requests are
  same-origin enforced (WebSocket handshakes bypass CORS — a foreign page
  cannot read your live transcript) and tokenless loopback binds reject
  non-localhost `Host` headers (DNS rebinding). Bearer tokens are compared
  in constant time, and retained-audio serving is path-contained under the
  engine's sessions directory (defense-in-depth against database tampering).
- The overlay ships a strict webview CSP, and its engine attach validates
  the health payload shape before routing asks — a stray process squatting
  the port is never mistaken for the engine.
- Full API reference with real captured examples: [docs/API.md](docs/API.md).

## The copilot

Auricle can read your screen — deliberately, one frame at a time.
`auricle peek` (or `POST /api/v1/peek`) captures the active window via
Windows.Graphics.Capture, OCRs it with the OS-local Windows.Media.Ocr,
and returns reading-order text. Warm capture→text is 175–300 ms on 1080p
windows ([docs/PHASE7_VISION_REPORT.md](docs/PHASE7_VISION_REPORT.md)).

The assistant service builds on that: `POST /api/v1/ask` combines your
question, an on-demand screen capture, and the last 10 minutes of live
transcript (an in-memory rolling window — no database reads), and streams
an LLM answer as SSE, mirrored on the same WebSocket the dashboard holds.
Follow-ups see your earlier asks from in-memory history. Ollama, Groq, or
any OpenAI-compatible endpoint:

```
curl -N -X POST http://127.0.0.1:4820/api/v1/ask \
  -H "Content-Type: application/json" \
  -d '{"question":"what was just being discussed and what is on my screen?",
       "include_screen":true,"include_transcript":true}'
```

The overlay (`overlay/`, Tauri v2) is the copilot's face: summons in
~100 ms, streams markdown answers, shows **context chips** naming exactly
what was captured, and passes its own window handle to `/ask` so a
capture never reads the overlay's previous answer back into the next
one. It talks only to the public API
([docs/PHASE9_OVERLAY_REPORT.md](docs/PHASE9_OVERLAY_REPORT.md)).

## Transparency

The copilot's constraints are design rules, not marketing — from the
architecture doc, verbatim:

- **No concealment.** The overlay is a normal window. No
  hide-from-screen-capture tricks (`SetWindowDisplayAffinity` /
  `WDA_EXCLUDEFROMCAPTURE` is out of scope and will not be added). If you
  share your screen, the overlay is visible — that's the point.
- **No continuous surveillance.** Screen capture happens only on explicit
  hotkey press. There is no background screenshot loop, no keylogging, no
  watching.
- **Privacy defaults preserved.** OCR text and questions are processed in
  memory; nothing screen-derived is persisted unless
  `copilot.retain_context = true`. Fully-local operation (Windows OCR +
  Ollama) is the default configuration.

## Performance

Measured on an i7-9750H (2019 6-core laptop) — methodology and budget
misses in [benches/RESULTS.md](benches/RESULTS.md) and the phase reports:

| metric | measured |
|---|---|
| capture → partial, deepgram nova-3 streaming (p50) | **0.14 s** |
| capture → final, deepgram (p50) | **0.54 s** |
| capture → partial / final, groq-whisper batch (p50) | 0.25 s / 0.27 s (p95 5.8 s) |
| capture → partial / final, whisper-local base.en CPU (p50) | 1.9 s / 2.6 s |
| hotkey → screen text ready (warm, 1080p) | 0.18–0.30 s |
| hotkey → overlay visible | **0.09–0.12 s** |
| ask → first answer token, Groq llama-3.3-70b | **0.7 s** |
| ask → first answer token, local Ollama (qwen2.5:14b, warm) | 10.1 s |
| ask → first answer token, local Ollama (qwen3:8b, reasoning) | 104 s |

## Limitations (honest ones)

- **Speaker channels ≠ diarization.** "You"/"Them" comes from which device
  the audio arrived on. Multiple remote speakers are all "Them"; separating
  them needs a pluggable diarizer (roadmap).
- **Windows-only.** The capture, OCR, and overlay layers are
  platform-specific; macOS/Linux are roadmap.
- **A fully-local copilot is slow on modest CPUs.** The measured
  time-to-first-token on a 2019 laptop is 10.1 s with qwen2.5:14b (prompt
  evaluation dominates) and 104 s with qwen3:8b (reasoning models think
  before they speak) — against 0.7 s for Groq. Local answers are private,
  not fast; the numbers and root causes are in
  [docs/PHASE8_ASSISTANT_REPORT.md](docs/PHASE8_ASSISTANT_REPORT.md).
- **Live-transcription quality is provider- and hardware-bound.**
  whisper-local `small.en` cannot keep up with continuous speech on
  2019-class laptop CPUs; `base.en` or a cloud provider are the realistic
  choices there.
- **Screen-peek OCR is Windows' built-in engine.** Near-perfect on
  article text, honest-but-imperfect on dense small fonts (`auricle` can
  come back as `auride` in a file tree).
- One recording session at a time, by design.

## Roadmap

- **v0.4 — system-wide dictation: speak into any app.** The capture →
  VAD → STT pipeline already exists; dictation is pointing it at the
  focused window's input.
- macOS capture (the capture layer is the only platform-specific engine
  crate).
- Pluggable diarization for multi-speaker separation.
- Vision-LLM screen understanding as an alternative to OCR (the
  `ScreenReader` trait is the extension point).

## Repo layout

`crates/` — core, capture, pipeline, stt, llm, vision, server, cli ·
`ui/` — Vite/React dashboard (embedded into the binary) ·
`overlay/` — Tauri v2 copilot overlay + tray (a pure API client) ·
`fixtures/` — real captured API payloads driving the tests ·
`benches/` — latency harness + results ·
`docs/` — API reference + phase engineering reports.

MIT licensed. See [BUILDING.md](BUILDING.md) to build from source and
[CONTRIBUTING.md](CONTRIBUTING.md) to contribute.
