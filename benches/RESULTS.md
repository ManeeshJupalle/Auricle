# Latency Benchmark Results

Measured 2026-07-05 with `benches/latency` (release build), on:

- **CPU:** Intel Core i7-9750H @ 2.60 GHz (6C/12T), 32 GB RAM
- **OS:** Windows 11 Home 10.0.26200 · **Network:** residential Wi-Fi
- **Fixture:** `fixtures/tts_loopback_16k.wav` (15 s of TTS speech captured
  through the real WASAPI loopback path in Phase 1), fed at exactly
  real-time pacing (10 ms batches on a wall-clock schedule) through the
  production pipeline (Silero VAD → chunker → provider), 2 runs per
  provider.

Latency = wall-clock arrival of an event minus the end time of the audio it
covers. This covers everything after capture; WASAPI callback granularity
adds ~10 ms on top (Phase 1 measurement). Span-final chunks inherently
include the 500 ms VAD hangover inside this number — the pipeline cannot
know speech ended until the hangover elapses.

| provider | capture→partial p50 (ms) | p95 | capture→final p50 (ms) | p95 | partials | finals |
|---|---|---|---|---|---|---|
| whisper-local (base.en) | 1 868 | 3 036 | 2 626 | 4 251 | 10 | 8 |
| deepgram (nova-3) | 140 | 215 | 539 | 566 | 22 | 8 |
| groq-whisper (large-v3-turbo) | 247 | 445 | 269 | 5 805 | 16 | 8 |

## Budget scorecard (architecture §8)

| Budget | Target | Measured | Verdict |
|---|---|---|---|
| capture→partial, cloud streaming | < 1.0 s | deepgram p50 0.14 s / p95 0.22 s | **pass** |
| capture→final, local CPU | < 2.5 s after speech pause | whisper base.en p50 2.63 s | **miss by ~0.13 s** |
| idle CPU (capturing, silence) | < 3 % | 2.4 % of one core (Phase 2, 45 s silence run) | pass (single-core basis) |
| RAM (whisper small.en loaded) | < 1.5 GB | not re-measured this phase | pending |
| UI partial update re-render | single row | by construction (Phase 5); profiler pass pending | pending (manual gate) |
| binary size, default features | < 40 MB + models | 31.3 MB incl. embedded UI | pass |

### Miss root causes (not hidden)

- **whisper-local final p50 2.63 s vs 2.5 s:** two stacked costs. (1) The
  500 ms VAD hangover is inside every span-final measurement — speech-pause
  finals cannot beat 0.5 s even with instant inference. (2) base.en
  inference on this 2019 6-core laptop runs ~1.4–2.9 s per ~2–5 s chunk
  even with the proportional `audio_ctx` optimization (Phase 2 report).
  Newer CPUs, the Vulkan feature, or quantized models would close the gap;
  `small.en` is substantially over budget on this machine (Phase 2: p50
  ~74 s backlogged — do not use it for live transcription on hardware of
  this class).
- **groq-whisper final p95 5.8 s:** batch-per-chunk uploads serialize; when
  a span closes right after a max-chunk final, the tail chunk queues behind
  the in-flight upload. The p50 (0.27 s) reflects the fast path; the p95
  reflects that queueing. Structural to batch APIs, mitigated by the
  drop-oldest-interim queue, and the reason deepgram is the recommended
  low-latency cloud provider.
- Tail events that only flush on Deepgram's `CloseStream` are stamped at
  drain time (slight overstatement for those few events; affects p95 only).

Reproduce: `cargo run --release -p auricle-latency-bench -- --stt <provider>
[--model base.en] [--repeat 2]` with provider keys in env.
