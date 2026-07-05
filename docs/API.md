# Auricle API

Base URL: `http://127.0.0.1:4820` (configurable via `[server].bind`; binding
to a non-localhost address requires a bearer token in the env var named by
`[server].token_env`, default `AURICLE_TOKEN`, sent as
`Authorization: Bearer <token>` on every request including the WebSocket
upgrade).

Start the daemon with `auricle serve [--bind ADDR]`. The web UI (built from
`ui/`, embedded via rust-embed) is served at `/`; everything under
`/api/v1/` and `/ws/live` is the API. For UI development, `npm run dev` in
`ui/` serves the frontend at `:5173` with `/api` and `/ws` proxied to a
running daemon on `:4820` — no Rust rebuild per UI change.

Every example below is real captured output from a running server
(2026-07-05, deepgram provider, TTS speech played through system audio).

---

## GET /api/v1/health

```
$ curl http://127.0.0.1:4820/api/v1/health
{"active_session":null,"state":"idle","status":"ok","version":"0.1.0"}
```

`state` is the lifecycle state machine: `idle` → `recording` → `stopping` → `idle`.

## GET /api/v1/devices

```
$ curl http://127.0.0.1:4820/api/v1/devices
{"devices":[{"channels":2,"is_default":true,"kind":"input","name":"Microphone (Stealth 600P Gen 3)","sample_format":"F32","sample_rate_hz":48000},{"channels":2,"is_default":false,"kind":"input","name":"Microphone (Realtek(R) Audio)","sample_format":"F32","sample_rate_hz":48000},{"channels":2,"is_default":false,"kind":"input","name":"Microphone (EMEET SmartCam C960 4K)","sample_format":"F32","sample_rate_hz":48000},{"channels":2,"is_default":false,"kind":"loopback","name":"Speakers (Realtek(R) Audio)","sample_format":"F32","sample_rate_hz":48000},{"channels":2,"is_default":false,"kind":"loopback","name":"ED320QR S3 (NVIDIA High Definition Audio)","sample_format":"F32","sample_rate_hz":48000},{"channels":2,"is_default":true,"kind":"loopback","name":"Speakers (Stealth 600P Gen 3)","sample_format":"F32","sample_rate_hz":48000}]}
```

`kind: "loopback"` entries are output devices captured via WASAPI loopback
(system audio). Loopback devices deliver samples only while audio plays.

## GET /api/v1/providers

```
$ curl http://127.0.0.1:4820/api/v1/providers
{"providers":[{"default":true,"detail":"model small.en present","id":"whisper-local","kind":"local","ready":true},{"default":false,"detail":"key present (DEEPGRAM_API_KEY)","id":"deepgram","kind":"cloud-streaming","ready":true},{"default":false,"detail":"key present (GROQ_API_KEY)","id":"groq-whisper","kind":"cloud-batch","ready":true},{"default":false,"detail":"env var OPENAI_API_KEY not set","id":"openai-compat","kind":"cloud-batch","ready":false}]}
```

## POST /api/v1/sessions

Start a recording session. Body fields (all optional): `title`,
`stt_provider` (default: `[stt].provider` from config), `mic_device`,
`loopback_device` (default: `[audio]` config; `"default"` = system default).

```
$ curl -i -X POST http://127.0.0.1:4820/api/v1/sessions \
    -H "Content-Type: application/json" \
    -d '{"title":"API walkthrough","stt_provider":"deepgram"}'
HTTP/1.1 201 Created
content-type: application/json

{"id":"s19f31016d52"}
```

Only one session can be active; a concurrent start conflicts:

```
$ curl -i -X POST http://127.0.0.1:4820/api/v1/sessions \
    -H "Content-Type: application/json" -d '{"title":"second"}'
HTTP/1.1 409 Conflict
content-type: application/json

{"active_session":"s19f30ff7ad5","error":"a session is already active"}
```

Unknown providers/devices return `400` with the error message; other
failures return `500`.

## POST /api/v1/sessions/{id}/stop

```
$ curl -X POST http://127.0.0.1:4820/api/v1/sessions/s19f31016d52/stop
{"id":"s19f31016d52","status":"stopping"}
```

Returns immediately; the engine flushes open speech spans, finishes
in-flight STT, persists, then emits `session_stopped` on the WebSocket and
returns to `idle`. Stopping a session that is not active returns `409`.

## GET /api/v1/sessions

```
$ curl http://127.0.0.1:4820/api/v1/sessions
{"sessions":[{"ended_at":1783233667,"id":"s19f31016d52","meta":{"audio":{"loopback":"C:\\Users\\manee\\AppData\\Local\\auricle\\sessions\\s19f31016d52\\loopback_16k.wav","mic":"C:\\Users\\manee\\AppData\\Local\\auricle\\sessions\\s19f31016d52\\mic_16k.wav"},"devices":{"loopback":"Speakers (Stealth 600P Gen 3)","mic":"Microphone (Stealth 600P Gen 3)"}},"started_at":1783233605,"stt_provider":"deepgram","title":"API walkthrough"}, …]}
```

`meta.audio` appears only when `[audio].retain_raw_audio = true`: the
per-channel 16 kHz WAVs written beside the DB (privacy default: off).
Timestamps are unix seconds.

## GET /api/v1/sessions/{id}

Metadata plus the full transcript (finals only — partials are never
persisted), ordered by start time:

```
$ curl http://127.0.0.1:4820/api/v1/sessions/s19f31016d52
{"ended_at":1783233667,"id":"s19f31016d52","meta":{…},"started_at":1783233605,"stt_provider":"deepgram","title":"API walkthrough","transcript":[{"channel":1,"provider":"deepgram","speaker":"Them","t_end_ms":2161,"t_start_ms":12,"text":"Welcome to the weekly engineering sync."},{"channel":1,"provider":"deepgram","speaker":"Them","t_end_ms":6890,"t_start_ms":2161,"text":"Today, we are going to review the transcription pipeline and the latency numbers"}, …]}
```

`channel`: 0 = mic ("You"), 1 = loopback ("Them"). `t_*_ms` are
milliseconds from session start (gap-aware timeline; see the Phase 2
report). Unknown ids return `404 {"error":"no session \"nope\""}`.

## GET /api/v1/sessions/{id}/export?format=md

```
$ curl "http://127.0.0.1:4820/api/v1/sessions/s19f31016d52/export?format=md"
# API walkthrough

- session: `s19f31016d52`
- date: 2026-07-05 06:40 UTC
- duration: 01:02
- stt provider: deepgram

## Transcript

**[00:00] Them:** Welcome to the weekly engineering sync.

**[00:02] Them:** Today, we are going to review the transcription pipeline and the latency numbers

**[00:06] Them:** from the benchmark run.
…
```

Content type `text/markdown`. Unsupported formats return `400`.
`auricle export <id> --md` prints the same markdown from the same code.

## POST /api/v1/sessions/{id}/summarize

Summarize a stored transcript with an LLM. Body (both optional):
`template` (default `minutes`; see `templates` in `/providers`) and
`provider` (default `[llm].provider`; one of `ollama`, `groq`,
`openai-compat`). Transcripts above ~6k estimated tokens are map-reduced.
Real captured exchange (Groq, llama-3.3-70b-versatile):

```
$ curl -i -X POST http://127.0.0.1:4820/api/v1/sessions/s19f31016d52/summarize \
    -H "Content-Type: application/json" \
    -d '{"template":"minutes","provider":"groq"}'
HTTP/1.1 201 Created
content-type: application/json

{"content":"The purpose of the meeting was to review the transcription pipeline and latency numbers from a benchmark run. \n* The transcription pipeline was discussed, including the voice activity detector, which opens a span when speech is present, and the chunker, which slices long spans into 5-second windows with 0.5 seconds of overlap.\n…\n**Decisions**: No decisions were made during the meeting.","created_at":1783268416,"id":1,"model":"llama-3.3-70b-versatile","provider":"groq","session":"s19f31016d52","template":"minutes"}
```

Summaries are persisted, returned in `GET /sessions/{id}` under
`summaries`, appended to the markdown export as `## Summary — <template>
(<model>)` sections, and rendered in the web UI's session detail view.
Errors: `404` unknown session, `400` empty transcript / unknown template
or provider / missing key env, `502` when the LLM endpoint itself fails.

`GET /api/v1/providers` now also returns `llm` (LLM provider readiness:
Ollama is always listed and probed only at use; Groq/openai-compat gate on
key presence) and `templates` (built-ins `minutes`, `action-items`,
`standup`, `1on1`, plus any `*.md` the user drops into
`%LOCALAPPDATA%\auricle\templates` — same-named files override built-ins).

## GET / PUT /api/v1/settings

Free-form key→JSON-value store. Recognized keys overlay the daemon's TOML
config for *subsequent* session starts (request params still win):

| key | type | overrides |
|---|---|---|
| `default_provider` | string | `[stt].provider` |
| `whisper_model` | string | `[stt.whisper-local].model` |
| `mic_device` | string | `[audio].mic_device` |
| `loopback_device` | string | `[audio].loopback_device` |
| `retain_raw_audio` | bool | `[audio].retain_raw_audio` |
| `ollama_model` | string | `[llm.ollama].model` (summaries) |

Unrecognized keys are stored and returned but have no engine effect.

```
$ curl -X PUT http://127.0.0.1:4820/api/v1/settings \
    -H "Content-Type: application/json" \
    -d '{"default_export":"md","ui_theme":"dark"}'
{"default_export":"md","ui_theme":"dark"}

$ curl http://127.0.0.1:4820/api/v1/settings
{"default_export":"md","ui_theme":"dark"}
```

PUT upserts the keys in the body (non-object bodies → `400`) and returns
the full settings object.

## WebSocket /ws/live

Server-push JSON events, one per text frame. Real captured sequence:

```
{"type":"session_started","session":"s19f30ff7ad5","title":"API walkthrough","stt_provider":"deepgram"}
{"type":"partial","session":"s19f30ff7ad5","channel":"loopback","speaker":"Them","t_start_ms":48037,"t_end_ms":49607,"text":"And the chunker slices long"}
{"type":"partial","session":"s19f30ff7ad5","channel":"loopback","speaker":"Them","t_start_ms":48037,"t_end_ms":51016,"text":"And the chunker slices long spans into five seconds"}
{"type":"final","session":"s19f30ff7ad5","channel":"loopback","speaker":"Them","t_start_ms":44497,"t_end_ms":48037,"text":"activity is a opens a span whenever speech is present."}
{"type":"session_stopped","session":"s19f31016d52"}
```

Event types:

| type | fields | notes |
|---|---|---|
| `partial` | session, channel, speaker, t_start_ms, t_end_ms, text | transient; superseded by the next partial/final; never persisted |
| `final` | same as partial + `latency_ms` | persisted to the transcript; `latency_ms` is chunk→final latency as seen by the engine |
| `vu` | session, channel, rms | ~10 Hz per channel while capturing; rides the lossy lane (drives the UI meters) |
| `session_started` | session, title, stt_provider | |
| `session_stopped` | session | emitted after flush + persistence complete |
| `device_lost` | session, channel, message | a capture device failed mid-session |
| `error` | session, message | recoverable engine/provider errors |
| `lag` | dropped_partials | sent to a slow consumer: partials were dropped for it (finals are never dropped) |

Slow-consumer policy: finals and lifecycle events ride a deep (1024) buffer;
partials ride a shallow lossy one. A consumer that can't keep up loses
partials only and is told how many via `lag`.

## Crash recovery

Sessions interrupted by a crash/kill are closed on the next store open:
`ended_at` is set from the last persisted segment and `meta.interrupted`
is flagged. Captured after a real `taskkill /F` mid-session and restart:

```
$ curl http://127.0.0.1:4820/api/v1/sessions/s19f3102fb19
{"ended_at":1783233740,…,"meta":{…,"interrupted":true},"transcript":[…12 segments persisted before the kill…]}
```
