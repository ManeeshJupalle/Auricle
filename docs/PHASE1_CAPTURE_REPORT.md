# Phase 1 Capture Report

Empirical results from capture-first verification on the dev machine, per the
Global Rules: a scratch probe binary was built and run against real hardware
*before* any workspace capture code was written. This report records which
loopback API won, the actual device formats encountered, and every place
reality disagreed with documentation or the architecture doc's assumptions.

**Machine:** Windows 11 Home 10.0.26200 · WASAPI host · Rust 1.95.0 · cpal 0.18.1

---

## Verdict: cpal wins; the `wasapi` crate fallback is not needed

cpal 0.18.1's WASAPI host transparently enables `AUDCLNT_STREAMFLAGS_LOOPBACK`
when an *input* stream is built on an *output* (render) device
(`cpal-0.18.1/src/host/wasapi/device.rs:863`). The probe confirmed it works
reliably on this machine:

| Probe run | Callbacks | Frames captured | Coverage | Signal |
|---|---|---|---|---|
| Loopback, 5 s, system silent | 0 | 0 / 240 000 | 0 % | — |
| Loopback, 5 s, WAV looping | 501 | 240 480 / 240 000 | 100.2 % | peak 0.356, RMS 0.061 |
| Mic, 3 s | 300 | 144 000 / 144 000 | 100.0 % | (see mic note below) |

Callback cadence was ~10 ms (≈480 frames per callback), max inter-callback gap
10.8 ms under load. No stream errors, no drops. The `wasapi`-crate fallback
path was therefore never implemented (Global Rule 1: no speculative code).

## Devices encountered

All endpoints on this machine share the same shared-mode mix format:
**48 000 Hz, 2 ch, F32**.

| Device | Kind | Notes |
|---|---|---|
| Microphone (Stealth 600P Gen 3) | input, default | USB wireless headset |
| Microphone (Realtek(R) Audio) | input | onboard |
| Microphone (EMEET SmartCam C960 4K) | input | USB webcam |
| Speakers (Stealth 600P Gen 3) | output, default | USB wireless headset |
| Speakers (Realtek(R) Audio) | output | onboard |
| ED320QR S3 (NVIDIA High Definition Audio) | output | monitor via HDMI/DP |

`supported_input_configs()` on the mics also offered U8/I16/I24/I32 at a fixed
48 000 Hz. The capture code handles F32/I16/I32/U8 generically and errors
cleanly on anything else; in practice shared-mode WASAPI always hands out the
F32 mix format.

## Doc-vs-reality corrections

1. **cpal 0.18 is a different API from the cpal the architecture doc era
   assumed (0.15).** `Device::name()` is gone — devices implement `Display`
   and a structured `description()`. `SampleRate` is now a plain `u32` alias,
   not a newtype. `build_input_stream` takes `StreamConfig` by value, not by
   reference. Streams are now required to be `Send`/`Sync`.

2. **Loopback devices reject the input-config APIs.** Despite loopback being
   supported, `default_input_config()` on a render device returns
   `UnsupportedOperation ("Device does not support input")` and
   `supported_input_configs()` yields zero ranges. The working recipe is:
   take `default_output_config()`, convert it to a `StreamConfig`, and pass
   that to `build_input_stream` on the output device. This is baked into
   `auricle-capture::start_loopback` with a comment.

3. **Loopback delivers *nothing* while the system is silent.** cpal's WASAPI
   capture is event-driven (`SetEventHandle`), and a loopback client's events
   only fire while the render engine is pumping. Probe-measured: 0 callbacks
   over 5 s of silence; 100.2 % coverage the moment audio plays. Consequences:
   - A loopback WAV can be shorter than wall-clock time if playback pauses;
     the CLI warns when loopback captured 0 samples.
   - `PHASE-2`: pipeline timestamps must not assume continuous sample flow on
     the loopback channel; gaps are real time that produced no frames.

4. **rubato's `process_partial` zero-pads the tail to a full chunk** and emits
   a full chunk of output (~371 frames for a 68-frame remainder at 44.1k→16k),
   silently inflating short streams by ~2 % in a naive implementation.
   `MonoResampler::finish` truncates the flush to `round(pending × ratio)`
   frames. Additionally the sinc filter swallows its startup delay
   (`sinc_len × ratio / 2` ≈ 21 output frames ≈ 1.3 ms at 16 kHz) from the
   total; the unit tests assert output length to within exactly that bound.

5. **A silent mic can be *digitally* silent.** The default mic (wireless
   headset) produced exact 0.0 samples at 100 % frame coverage in unattended
   runs — its hardware mute/noise gate, not a capture bug. The EMEET webcam
   mic captured real ambient signal (peak 0.058, RMS 0.004) through the same
   code path. Gate verification must involve actually speaking into the mic.

## End-to-end verification runs (workspace code, not the probe)

`auricle capture --seconds 15 --out-dir ./captures` with a WAV looping:

| File | Samples | Rate | Duration | Signal |
|---|---|---|---|---|
| mic_raw.wav | 720 480 | 48 000 | 15.01 s | headset mic muted (see #5) |
| mic_16k.wav | 240 138 | 16 000 | 15.01 s | — |
| loopback_raw.wav | 720 480 | 48 000 | 15.01 s | peak 0.201, RMS 0.030 |
| loopback_16k.wav | 240 138 | 16 000 | 15.01 s | peak 0.201, RMS 0.030 |

Drop counters: 0 on both channels. Raw and 16k stats match, confirming the
resampler preserves amplitude and duration. A second run using a config file
selecting the EMEET mic by exact name verified named-device resolution and
captured real room tone; a run with a nonexistent device name exited cleanly
with the available-device list (no panic).

## Decisions locked for Phase 2+

- Loopback API: **cpal only**; configure loopback streams from
  `default_output_config()`.
- Ring buffers: `rtrb`, capacity 4 s of mono audio per channel; callbacks are
  allocation-free/lock-free (downmix + push + atomic drop counter only).
- Resampler: rubato `SincFixedIn`, 1024-frame chunks, per-channel instance,
  flush-truncation as described in #4.
- WAV output: 32-bit float, mono.
