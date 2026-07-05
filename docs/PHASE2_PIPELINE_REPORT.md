# Phase 2 Pipeline Report

Empirical findings and doc-vs-reality corrections from building the VAD +
whisper streaming pipeline. Everything here was verified by running code on
the dev machine, per the capture-first/payload-first global rule.

**Machine:** Windows 11 Home 10.0.26200 · Rust 1.95.0 · whisper-rs 0.16.0
(whisper-rs-sys 0.15.0) · voice_activity_detector 0.2.1 (ort 2.0.0-rc.10)

---

## Model payloads (payload-first)

Checksums were captured from the Hugging Face LFS metadata
(`/api/models/ggerganov/whisper.cpp/tree/main`) on 2026-07-04, not from
documentation, and are hardcoded in `auricle-stt/src/model.rs`:

| Model | File | Size | SHA-256 |
|---|---|---|---|
| base.en | ggml-base.en.bin | 147,964,211 | `a03779c8…36c6d002` |
| small.en | ggml-small.en.bin | 487,614,201 | `c6138d6d…f9c41e5d` |

A real download of base.en was performed and the on-disk hash matched the
LFS metadata exactly, validating the whole ensure-model path.

## Doc-vs-reality corrections

1. **whisper-rs's bundled bindings do not work on Windows.** The crate
   documents `WHISPER_DONT_GENERATE_BINDINGS=1` as a way to skip bindgen
   (and its build script auto-falls back to bundled bindings when bindgen
   errors). In reality the shipped `src/bindings.rs` was generated on Linux:
   it contains glibc struct layout assertions (`_IO_FILE` = 216 bytes,
   `_G_fpos_t` = 16 bytes) that fail to compile against MSVC
   (`attempt to compute 12usize - 16usize`). Additionally, bindgen 0.72
   *panics* when libclang is missing rather than returning an error, so the
   build script's "fall back to bundled bindings" branch is unreachable.
   **Consequence:** LLVM/libclang is a hard build requirement on Windows.
   `.cargo/config.toml` pins `LIBCLANG_PATH` (and `CMAKE`, pointing at the
   VS 2022 Build Tools bundled CMake).

2. **MSBuild FileTracker breaks CMake builds under deep/temp paths.** The
   first whisper.cpp compile attempt ran in a ~250-character path under
   `%TEMP%` and failed with `FTK1011: could not create the new file tracking
   log file` (plus warning MSB8029 about building under the Temporary
   directory). The identical build succeeds from `D:\Auricle\target`. Keep
   the workspace at a short path.

3. **Silero V5 restricts frame sizes.** `voice_activity_detector` 0.2.1
   (Silero V5) accepts *only* 512-sample frames at 16 kHz (256 at 8 kHz);
   the builder's `chunk_size` argument is overridden to 512 regardless of
   what is passed. The VAD gate is therefore built on fixed 32 ms frames.

4. **whisper.cpp skips short inputs.** Inputs shorter than ~1 s are not
   processed, so chunks are zero-padded to 1.2 s before inference
   (`MIN_AUDIO_MS` in `whisper_local.rs`). Chunk timestamps are taken from
   the chunk metadata, not whisper's internal segment times, so padding does
   not affect the transcript timeline.

5. **whisper.cpp logging.** whisper.cpp dumps ~40 lines of model/system info
   to stderr on load, which would corrupt the live in-place partial display.
   `whisper_rs::install_logging_hooks()` with no log backend features
   compiled in routes that output to a no-op sink; called once at provider
   load.

## Gappy-stream design (constraint #1 from Phase 1)

- `Timeline` (per channel): timestamps = wall-clock anchor + cumulative
  sample position. When the wall clock runs ahead of the sample clock by
  more than 250 ms, the batch is flagged `gap_before` and the clock
  re-anchors so the batch ends "now". Catch-up bursts (wall behind samples)
  are not gaps; timestamps never regress.
- `VadGate`: a gap force-closes any open span at the pre-gap clock, clears
  pre-roll and pending buffers, and resets Silero's recurrent state, so a
  gap can never fabricate a span or pull stale audio across the hole.
- Verified by unit tests (timeline re-anchoring, gap-during-silence,
  post-gap span timing) and an integration test that feeds the real fixture
  with a simulated 20 s hole and asserts no phantom chunks and re-anchored
  post-gap timestamps.

## Fixture

`fixtures/tts_loopback_16k.wav` — 15 s of Windows TTS speech played through
the default output and captured by the Phase 1 loopback path (real WASAPI
capture, resampled to 16 kHz by the Phase 1 resampler stage). Used by the
pipeline integration tests (real Silero VAD) and the `#[ignore]`d
whisper end-to-end test.

## Backpressure

Per-session bounded queue (8 chunks ≈ 40 s of audio): on overflow the oldest
*interim* chunk is evicted first; an incoming interim is never allowed to
evict a queued final; only sustained final-on-final overload evicts a final
(counted and surfaced as an `SttEvent::Error` notice at session end).
Dequeue order prioritizes transcript over freshness: finals jump interims,
and any interim older than the popped chunk is discarded as superseded. The
CLI driver additionally stops feeding interims while more than one final is
awaiting inference. Inference runs on `spawn_blocking` with exactly one
in-flight inference per channel; capture and VAD are never blocked by STT.

## whisper.cpp cost model and the audio_ctx fix (measured)

`whisper_full` pads every input to a 30 s mel window, so a naive call costs
the same for a 2 s chunk as for 30 s — on this machine's i7-9750H (MSVC
build, AVX2/FMA verified in the build log) roughly 5 s per call for base.en
and 15-20 s for small.en, which made the live pipeline fall behind real time
with a constantly-full queue. Setting `audio_ctx` proportional to the actual
chunk length (`samples/320 + 128`, clamped to 192..=1500 — the whisper.cpp
streaming technique) makes per-call cost scale with chunk length. A side
effect at small contexts is a higher chance of whisper's
repeated-sentence decode artifact; consecutive duplicate whisper segments
within one call are suppressed when joining.

## Measured results (release build, i7-9750H 6C/12T, live TTS audio)

50 s of near-continuous synthetic speech through the loopback channel:

| Metric | base.en | small.en |
|---|---|---|
| chunk→final latency p50 | 3 718 ms | 73 958 ms (backlogged) |
| chunk→final latency p95 | 5 106 ms | 99 462 ms |
| finals during live phase | 13 of 16 | 4 of 14 |
| partial line updates | 10 | 1 |
| queue sheds | 0 | 1 |
| ring overruns | 0 | 0 |
| Ctrl+C → exit | ~3.5 s | ~70 s (draining queued finals) |

**Honest reading:** on this CPU, base.en runs faster than real time and sits
~1.2 s over the 2.5 s p50 budget (the number includes the 500 ms VAD
hangover that is part of every span-final chunk). small.en runs at ~0.6×
real time — it cannot keep up with continuous speech on this machine; the
pipeline degrades as designed (partials shed first, transcript preserved and
completed after stop, sheds counted and reported) but small.en should not be
the default on hardware of this class. Faster/quantized models are a
Phase 6 benchmarking topic.

45 s silence check (VAD gate): 2.4 % of one core (~0.2 % of the machine),
zero STT calls, zero segments.

## Toolchain note (Windows builds)

`whisper-rs-sys` needs CMake and libclang at build time. `.cargo/config.toml`
pins `CMAKE` to the VS 2022 Build Tools bundled CMake and `LIBCLANG_PATH` to
a per-user `pip install libclang` wheel (a full LLVM install works equally).
The bundled-bindings fallback is not usable on Windows (see correction #1).
