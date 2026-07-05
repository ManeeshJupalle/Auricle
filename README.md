# Auricle

<!-- DEMO GIF -->

**A local-first meeting transcription engine.** Auricle is not another
meeting app — it is a single Rust binary that captures your meeting audio
(system loopback + microphone, no bot joining your calls), transcribes it in
real time through swappable STT backends, and exposes everything over a
REST/WebSocket API. The bundled web UI is just a client; curl is an equally
first-class one. Privacy by default: with the local Whisper backend, no
audio ever leaves your machine.

> *Auricle: the outer ear — the part of you that listens. It sits on your
> head, not in the cloud.*

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
| Interface | the desktop app | REST API | **REST/WS API; web UI is just a client** |
| Runtime | Tauri + Next.js | Django/Postgres/Redis fleet | **one ~31 MB binary, SQLite** |
| Audio leaves your machine | no | yes | **only if you opt into a cloud provider** |

## Quickstart

```
# 1. Get auricle (release binary, or see BUILDING.md to build from source)
auricle devices          # see your microphones + loopback outputs
auricle serve            # daemon + web UI on http://127.0.0.1:4820

# or skip the server entirely:
auricle record --stt whisper-local --model base.en   # live transcript in your terminal
```

Open `http://127.0.0.1:4820`, press **Start session**, play your meeting —
the system-audio side is labeled **Them**, your microphone is **You**.
Stop, then export markdown or summarize with a local (Ollama) or cloud LLM.

Cloud providers are opt-in via environment variables: `DEEPGRAM_API_KEY`
(streaming STT, the low-latency option), `GROQ_API_KEY` (batch Whisper STT
+ LLM summaries), or any OpenAI-compatible endpoint via config. Local
Whisper models download automatically with SHA-256 verification on first
use.

## Architecture

```
 mic ──cpal──► ring buffer ─► resample 16k ─► Silero VAD ─► chunker ─┐
                                                                     ├─► STT provider ─► assembler ─► SQLite
 system audio ─WASAPI loopback─► ring buffer ─► … (same pipeline) ───┘   (trait object)      │
                                                                                             ├─► REST /api/v1
        whisper-local │ deepgram │ groq-whisper │ openai-compat                              └─► WS /ws/live ─► web UI
```

- Capture callbacks are allocation-free and lock-free (rtrb ring buffers).
- Silence never reaches STT: Silero VAD gates the stream, with gap-aware
  timestamps (WASAPI loopback delivers *nothing* while the system is
  silent — measured, not assumed).
- Two-channel speaker labels come free from the capture topology.
- Full API reference with real captured examples: [docs/API.md](docs/API.md).

## Performance

Measured on an i7-9750H (2019 6-core laptop), real-time-paced fixture through
the production pipeline — full methodology and budget misses in
[benches/RESULTS.md](benches/RESULTS.md):

| provider | capture→partial p50 | capture→final p50 |
|---|---|---|
| deepgram (nova-3, streaming) | **0.14 s** | **0.54 s** |
| groq-whisper (large-v3-turbo, batch) | 0.25 s | 0.27 s (p95 5.8 s) |
| whisper-local (base.en, CPU) | 1.9 s | 2.6 s |

## Limitations (honest ones)

- **Speaker channels ≠ diarization.** "You"/"Them" comes from which device
  the audio arrived on. Multiple remote speakers are all "Them"; separating
  them needs a pluggable diarizer (roadmap).
- **Windows-only capture in v1.** The capture layer is the only
  platform-specific crate; macOS/Linux are roadmap.
- **Live-transcription quality is provider- and hardware-bound.**
  whisper-local `small.en` cannot keep up with continuous speech on
  2019-class laptop CPUs (numbers in benches/RESULTS.md); `base.en` or a
  cloud provider are the realistic choices there.
- One recording session at a time, by design.

## Repo layout

`crates/` — core, capture, pipeline, stt, llm, server, cli ·
`ui/` — Vite/React client (embedded into the binary) ·
`fixtures/` — real captured API payloads driving the tests ·
`benches/` — latency harness + results ·
`docs/` — API reference + phase engineering reports.

MIT licensed. See [BUILDING.md](BUILDING.md) to build from source and
[CONTRIBUTING.md](CONTRIBUTING.md) to contribute.
