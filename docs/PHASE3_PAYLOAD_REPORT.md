# Phase 3 Payload Report

Every serde type in the cloud providers was derived from payloads captured
verbatim from the live APIs on 2026-07-05 — never from documentation. Raw
captures live in `fixtures/deepgram/` (18 WebSocket frames + gap-behavior
logs in `gap_test/`) and `fixtures/groq/` (default + verbose responses).
`openai-compat` is implemented against the Groq shape (Groq is
OpenAI-compatible) with a configurable `base_url`; no OpenAI key was used.

## Capture method

- **Deepgram**: real `wss://api.deepgram.com/v1/listen` session
  (`model=nova-3&encoding=linear16&sample_rate=16000&channels=1&interim_results=true&smart_format=true`),
  streaming `fixtures/tts_loopback_16k.wav` as linear16 PCM at ~2× realtime;
  every received text frame dumped unmodified to `frame_NNN.json`.
- **Groq**: `POST /openai/v1/audio/transcriptions` (multipart, model
  `whisper-large-v3-turbo`) with the same WAV, raw JSON bodies saved for both
  the default and `verbose_json` response formats.
- Two extra Deepgram sessions probed gap behavior (see below).

## Doc-vs-reality discrepancies

1. **Deepgram result times are cumulative-fed-audio seconds, not wall
   time.** Verified by feeding 5 s of audio, a 15 s KeepAlive-only gap, then
   the rest: post-gap results continue at `start: 2.37` exactly as in the
   gapless run. Docs describe `start` simply as the audio start time; with a
   VAD-gated (gappy) feed the distinction is load-bearing. The provider
   keeps a fed-range → session-time map, with an edge rule for times that
   land exactly on a range boundary (the end of one span == the start of
   the next in fed time, but they can be minutes apart in session time).

2. **Silence kills the socket in ~12 s.** With no audio and no text frames,
   the server sends a Metadata frame with `"sha256": "incomplete"` and then
   a close frame `code: Error, reason: "Deepgram did not receive audio data
   or a text message within the timeout window. See https://dpgr.am/net0001"`
   (captured verbatim in `gap_test/no_keepalive.log`). A
   `{"type":"KeepAlive"}` text message every 5 s bridges arbitrarily long
   VAD gaps (`gap_test/keepalive_gap.log`).

3. **Empty-transcript Results frames are routine.** 5 of the 17 Results
   frames in the clean capture have `transcript: ""` (silence padding /
   session tail). Consumers must skip them or the transcript fills with
   blank finals.

4. **A Results frame arrives with `is_final: true` for pure silence** at
   stream end (frames 015/016), and the final Metadata frame carries
   `transaction_key: "deprecated"` — a literal string. Neither appears
   in the streaming docs' examples.

5. **`from_finalize` field** on Results confirms the `{"type":"Finalize"}`
   control message; the provider sends it at span seams so speech from
   different VAD spans (whose separating silence was never fed) cannot be
   merged into one utterance.

6. **Groq attaches an undocumented `x_groq: {"id": "req_..."}` extension**
   to both response formats.

7. **Groq's `verbose_json.language` is the full English word** ("English"),
   not an ISO 639 code — anything keying on `language == "en"` would break.

8. **Groq accepted the 32-bit-float WAV fixture directly** (no conversion
   needed for the capture). The provider still uploads 16-bit PCM WAVs
   (half the bytes for identical content).

9. **Rate-limit headers** captured on every Groq response
   (`x-ratelimit-remaining-audio-seconds`, `...-requests`, reset timers) —
   available for smarter retry pacing later; current retry policy is
   exponential backoff + jitter on 429/5xx only, per spec.

## Provider design consequences

- **Deepgram session**: one WebSocket per session with reconnect-with-backoff
  (1 s → 30 s cap). Chunks arriving while disconnected buffer in memory and
  are re-fed after reconnect; the fed-audio clock resets per connection and
  the map resets with it. Chunker overlap is trimmed before sending (a
  continuous stream must never carry the same audio twice), and interim
  chunks are not fed at all — Deepgram generates its own interims, which map
  to `SttEvent::Partial`. `is_final` results map to `SttEvent::Final`.
- **Batch (groq-whisper / openai-compat)**: per-chunk in-memory 16-bit WAV
  upload on the shared bounded queue from Phase 2 (same shed policy), retry
  with jitter on 429/5xx, fail-fast on other 4xx, up to 4 attempts.
- **Keys**: read from the env var named in config (`api_key_env`) at
  provider construction, held in memory only, never persisted or logged.

## Measured live results (release build, 50 s of continuous TTS speech)

| Metric | deepgram | groq-whisper | whisper-local base.en (Phase 2) |
|---|---|---|---|
| chunk→final latency p50 | 557 ms | 8 088 ms | 3 718 ms |
| chunk→final latency p95 | 1 740 ms | 28 070 ms | 5 106 ms |
| partial line updates | 35 | 15 | 10 |
| finals | 17 (all correct) | 16 (all correct) | 16 |
| Ctrl+C → exit | immediate | ~16 s (serial upload drain) | ~3.5 s |

Deepgram's numbers required feeding it *incrementally*: interim chunks pass
through the overlap-trim watermark so only new samples are sent (~1 s
granularity). Feeding at final-chunk granularity (5 s bursts) produced
correct transcripts but only 1 visible partial and p95 ≈ 3.9 s. Groq's
latency is the nature of serial batch-per-chunk uploads under continuous
speech; its finals are accurate.
