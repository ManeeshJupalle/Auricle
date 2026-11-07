# Auricle API

Base URL: `http://127.0.0.1:4820` (configurable via `[server].bind`). Binding
to a non-localhost address requires a bearer token, read from the env var
named by `[server].token_env` (default `AURICLE_TOKEN`) or the OS credential
store, and sent as `Authorization: Bearer <token>` on every request including
the WebSocket upgrade. `/ws/live` and retained audio also take it as
`?token=`, because a browser cannot set a header on a WebSocket handshake or
an `<audio>` request; no other route does. The web UI's static files need no
token, so the dashboard can load and ask for one.

Start the daemon with `auricle serve [--bind ADDR]`. The web UI (built from
`ui/`, embedded via rust-embed) is served at `/`; everything under
`/api/v1/` and `/ws/live` is the API. For UI development, `npm run dev` in
`ui/` serves the frontend at `:5173` with `/api` and `/ws` proxied to a
running daemon on `:4820` — no Rust rebuild per UI change.

Configuration comes from `--config PATH` if given, else
`%LOCALAPPDATA%\auricle\auricle.toml` if it exists, else the built-in
defaults. The file in use prints as `using config <path>` on stderr.

A bad value fails a command only if that command needs the key: `serve` needs
`server.bind` (unless `--bind` replaces it), `capture` needs `[audio]`, and
`record` needs `[audio]`, the language (unless `--language` replaces it), and
the section of its active provider. Every other bad value, and any unknown key,
only warns on stderr; whatever uses that value reports its own error when it
gets there.

A file given with `--config` must exist and parse. A file that was only found
can't fail the load: if it can't be read or parsed, or a key the command needs
is bad, a warning says so and the built-in defaults replace the whole file.
`record` checks its provider after loading, so a bad `stt.provider`, or a bad
section for the provider in use, stops it wherever the file came from.

One daemon per data root: a second `auricle serve` is refused, even on another
port, with `another auricle daemon is already running on <root>`. The lock is
released however the process ends, so a crash leaves nothing to clean up.

Unless a section says otherwise, examples are real captured output from a
running server (2026-07-05, deepgram provider, TTS speech played through
system audio). Routes and fields added since then give their shape instead,
read from the code rather than captured.

---

## Trust model

The daemon trusts the machine it runs on and does not trust the browser.

**Local processes are trusted.** A loopback bind has no token, and loopback
processes are mutually trusted by design. Anything that can open a socket to
`127.0.0.1:4820` can use the whole API: read every transcript and summary,
fetch retained audio, start and stop recordings, capture the screen with
`POST /api/v1/peek`, change settings, and shut the daemon down. A client that
sends no `Origin` header and addresses the daemon as localhost passes every
check, which is how `curl` and the overlay work. The overlay's webview is a
browser, so it is refused like one; its requests go through the overlay's
native side instead. The daemon does not look at which user a connection comes
from, so on a shared machine this reach extends to other logged-in users'
processes. Against a process running as you, a token would add little: the
database and retained WAVs are ordinary unencrypted files under
`%LOCALAPPDATA%\auricle`.

**The MCP toggle and the egress ledger govern MCP, not local access.**
`mcp_enabled` opens and closes `/mcp` and nothing else, and a local process can
set it through `PUT /api/v1/settings` like any other setting. Reading a
transcript over REST writes no ledger row, so an agent that can run `curl` can
read every meeting without showing up there. Treat a local agent with shell
access like any other program running as you.

**Browsers are not trusted.** A page in your browser can reach loopback too,
so:

- REST, `/mcp`, and `/ws/live` answer `403` to a request whose `Origin` is not
  same-origin with its `Host`. A page on another localhost port gets no more
  than any other site, and `null` and `file://` origins are refused.
- Tokenless binds answer `403` to any `Host` that does not address localhost,
  which stops DNS rebinding.
- Every response carries `X-Content-Type-Options: nosniff`, a same-origin
  `Content-Security-Policy`, and `Cross-Origin-Resource-Policy: same-origin`.
  The last one covers what the `Origin` check cannot see: an `<audio>` or
  `<img>` on a foreign page sends no `Origin`.

**Beyond loopback, the token is the whole defense.** Every API route needs it,
`/mcp` and `/ws/live` included. Only the static UI answers without one, and an
unknown `/api/` path is a plain `404`. The daemon speaks plain HTTP, so on a
network the token and your transcripts travel unencrypted: keep such a bind on
a network you trust, or put TLS in front of it. A `?token=` URL can also end up
in logs and browser history, which is why only two routes take one.

**Clients cannot authenticate the daemon.** Whatever holds the port answers.
The overlay checks the shape of `/api/v1/health` before attaching, which stops
an accidental squatter, not a hostile one.

---

## GET /api/v1/health

```
$ curl http://127.0.0.1:4820/api/v1/health
{"active_session":null,"state":"idle","status":"ok","version":"0.1.0"}
```

`state` is the lifecycle state machine: `idle` → `recording` → `stopping` → `idle`.
The response also carries `pid`, the daemon's process id, which this capture
predates.

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

`default` marks the provider a start with no `stt_provider` would use: the
`default_provider` setting if there is one, else `[stt].provider`.

## POST /api/v1/sessions

Start a recording session. Body fields (all optional): `title`,
`stt_provider` (default: the `default_provider` setting, then `[stt].provider`
from config), `mic_device`, `loopback_device` (default: the settings of the same
name, then `[audio]` config; `"default"` = system default).

**Auto-titling:** when `title` is omitted, the session starts as
`"Untitled session"` and, once stopped, gets a 3–6 word title generated
from its transcript (configured `[llm]` provider; falls back to the first
words of the transcript when no LLM is reachable, so it works offline). A
`session_updated` WS event announces the new title. Sessions renamed via
PATCH are never auto-retitled (`meta.auto_title` is cleared).

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
failures return `500`. Once a shutdown has been requested, a start returns
`503 {"error":"the daemon is shutting down"}`.

The request blocks until capture is running, which on first use of a local
model includes the download. The start runs to completion even if the client
goes away, so reloading the dashboard mid-download does not abandon it. While
it runs, `/health` already names the session. A
`POST /sessions/{id}/stop` that lands before capture opens ends the start: the
pending request gets `409 {"error":"session was stopped before capture began"}`,
no session row is written, and `session_stopped` goes out on the WebSocket. If
a start fails after its client has left, an `error` event carrying the reason
follows that `session_stopped`.

## POST /api/v1/sessions/{id}/stop

```
$ curl -X POST http://127.0.0.1:4820/api/v1/sessions/s19f31016d52/stop
{"id":"s19f31016d52","status":"stopping"}
```

Returns immediately; the engine flushes open speech spans, finishes
in-flight STT, persists, then emits `session_stopped` on the WebSocket and
returns to `idle`. Stopping a session that is not active returns `409`.

## POST /api/v1/dictation/start · stop · cancel

Push-to-talk dictation from the mic alone. A dictation and a meeting never hold
the mic together: each start answers `409` with a plain `error` while the other
is running, as does a second dictation.

- **`start`**: optional body `{"language":"en"}`. Answers once the mic is
  listening with `201 {"id":"d…","provider":"whisper-local","local":true}`,
  where `local` is false when the audio leaves the machine. The provider is the
  `dictation_provider` setting, `whisper-local` when unset. It never follows
  `default_provider`, so setting meetings up for the cloud does not send
  dictation there. The language is the body's, else `dictation_language`, else
  `stt_language`, else `[stt].language`. A bad language or unknown provider is
  `400`; after a shutdown request it is `503`.
- **`stop`**: stops the mic, transcribes what is left, and answers
  `200 {"id":"d…","text":"…","duration_ms":n}`. `text` may be empty. `404`
  when there is nothing to stop, `502 {"error":…}` when transcription failed
  and nothing was heard, `504` if the dictation runs past its own bounds on
  finishing (150 s).
- **`cancel`**: discards it, `200 {"id":"d…"}` or `404`. It answers once the
  mic is free, so a new start can follow at once.

A dictation stops itself after 5 minutes, so a stuck key can't hold the mic.
That works as a stop does and sends `dictation_stopped`. Its text waits in
memory for the next `stop` to collect, and a new start discards it.

Nothing of a dictation is stored: no session row, no segment, no audio file
even with `retain_raw_audio` on, and nothing in the transcript ring, so `/ask`
and MCP never see it. A provider that is not on-device gets one `dictation`
egress row before any audio is sent, with no text in it. Local dictation
writes no row. While a dictation runs, `/health` still says `idle` with no
`active_session`.

Only finals are transcribed, with no interim passes, so a stop never waits
behind a preview. `dictation_partial` carries the text so far as each final
lands. A start that is still loading its model can be stopped: the start then
answers `409` and the stop gets empty text.

## GET /api/v1/sessions

Newest first. Optional `?q=` filters sessions whose **title or transcript
text** contains the query: a substring match (SQLite LIKE; `%`/`_` in the query
are treated literally) that folds case for ASCII only, so `MÜLLER` does not
find `Müller`. `GET /api/v1/sessions?q=budget`.

With neither `limit` nor `offset` the response holds every match. With either,
it is one page: `limit` defaults to 200 and is clamped to 1–1000, `offset`
defaults to 0. `total` counts every match either way, so a client can tell when
there is more.

```
$ curl http://127.0.0.1:4820/api/v1/sessions
{"sessions":[{"ended_at":1783233667,"id":"s19f31016d52","meta":{"audio":{"loopback":"C:\\Users\\you\\AppData\\Local\\auricle\\sessions\\s19f31016d52\\loopback_16k.wav","mic":"C:\\Users\\you\\AppData\\Local\\auricle\\sessions\\s19f31016d52\\mic_16k.wav"},"devices":{"loopback":"Speakers (Stealth 600P Gen 3)","mic":"Microphone (Stealth 600P Gen 3)"}},"started_at":1783233605,"stt_provider":"deepgram","title":"API walkthrough"}, …],"total":…}
```

`meta.audio` appears only when `[audio].retain_raw_audio = true`: the
per-channel 16 kHz WAVs written beside the DB (privacy default: off). They
follow the session timeline: capture delivers nothing during system silence,
so gaps are padded with silence to keep the file aligned with `t_*_ms`. Each
file is flushed every 5 s, so a hard kill leaves it playable up to the last
flush. Timestamps are unix seconds.

A file has two limits, each per channel: 12 h of padding in total, counted across
the whole track rather than per gap, and a 36 h track length, near the most one
WAV can hold. When audio returns after a gap that would pass the first, or the
track reaches the second, that channel's file closes where it is and an `error`
event says so; the session and its transcript carry on. Resuming without the
padding would put everything after it at the wrong transcript time. Silence
that runs to the end of a session just ends the track early, with no event.

`meta.dropped` appears only on a session that lost something. Its channel keys
say what each channel lost:
`{"mic":{"capture_ms":…,"stt_chunks":…,"stt_finals":…}}`. `capture_ms` is audio
the capture ring buffer overflowed, `stt_chunks` is chunks the provider's queue
shed under load, and `stt_finals` is how many of those were finals, the ones that
leave a hole in the transcript. A hole can be told apart from silence.

`bleed_finals` sits beside the channel keys, not under one:
`{"mic":{…},"bleed_finals":3}`. It counts mic lines dropped as speaker bleed,
and is absent when there were none. `[stt] drop_mic_bleed = false` turns that
filter off. It is on by default, and worth turning off with a monitor mix, where
your own voice reaches the output and a match can delete your words. It only
drops a mic line that starts 0 to 500 ms after a matching loopback line, so some
bleed gets through, such as chunks of long speech and the first lines after
loopback silence. The thresholds have not been measured on real bleed.

## GET /api/v1/search

Matching lines across every meeting. `?q=` on `/api/v1/sessions` says which
meetings mention something; this says where. Takes `q`, `limit` (default 200,
clamped to 1–1000) and `offset`. Most recent meeting first, then in order
through it. `total` counts every match, not just the page. Matching is the same
substring search as everywhere else: case-insensitive for ASCII only, with
wildcards taken literally. No `q` returns no matches.

```json
{"matches":[{"session_id":"s19f3","session_title":"Retro","started_at":1721348051,
  "channel":0,"speaker":"You","t_start_ms":5000,"t_end_ms":5500,
  "text":"budget review on Thursday"}],"total":3}
```

`channel` is the same number transcript segments carry, and `speaker` is the
label. This route is for the dashboard and scripts; agents have `auricle.search`.

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

## PATCH /api/v1/sessions/{id}

Rename: body `{"title": "New name"}` → `200 {"id": …, "title": …}`.
`400` empty title, `404` unknown id.

## DELETE /api/v1/sessions/{id}

Deletes any retained audio files, then the session row, transcript, summaries,
and any asks retained for it → `200 {"id": …, "deleted": true}`. `409` while
that session is actively recording; `404` unknown id. Audio goes first because
it lives outside the database: if it can't be removed, for example because
another program holds a file open, the answer is `500`, the meeting stays
listed, and some of its audio may already be gone. Deleting again works. Freed
database pages are zeroed rather than left holding the text, but a backup taken
earlier still has the session.

The text also leaves the rolling window and the session's follow-up history, so
`/ask` and `auricle.live_transcript` can't return it. One gap remains: asks made
outside any session share one history, so an earlier answer there that quoted
the meeting can still reach a follow-up until a new conversation starts or the
daemon exits.

## GET /api/v1/sessions/{id}/audio/{channel}

Streams a retained per-channel 16 kHz WAV (`channel` = `mic` |
`loopback`), content-type `audio/wav`, with HTTP Range support (`206`), which
is what lets the dashboard seek without re-downloading the file. Only exists
for sessions recorded with `retain_raw_audio`; otherwise `404`. Any other
`channel` is `400`. Same bearer middleware as every other route, and it also
takes `?token=` (see the top of this page).

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
Errors: `404` unknown session (including one deleted during the LLM call),
`400` empty transcript / unknown template or provider / missing key env, `502`
when the LLM endpoint itself fails. A summary the model cut off at its output
limit still returns, ending with
`[Truncated: the model reached its output limit before finishing.]`.

On a map-reduced transcript, only a failure on the first portion is a `502`. A
later failure keeps what finished: the remaining portions are skipped, the
finished ones are merged, and the summary is saved and returned as usual, ending
with `[Incomplete: N of M portions were not summarized: …]`. If the merge is
what failed, the finished portions come back unmerged, ending with
`[Incomplete: the portions above could not be merged: …]`.

Summaries and asks give up after 10 minutes with no bytes from a cloud endpoint,
and after an hour from Ollama or any other endpoint on this machine, because a
long prompt on a CPU can take over 10 minutes before the first byte.

`GET /api/v1/providers` now also returns `llm` (LLM provider readiness:
Ollama is always listed and probed only at use; Groq/openai-compat gate on
key presence) and `templates` (built-ins `minutes`, `action-items`,
`standup`, `1on1`, plus any `*.md` the user drops into
`%LOCALAPPDATA%\auricle\templates` — same-named files override built-ins).

## GET / PUT /api/v1/settings

Free-form key→JSON-value store. Recognized keys overlay the daemon's TOML
config (request params still win). Session keys apply from the *next* session
start; `mcp_enabled` applies at once, and `retention_days` at the next sweep:

| key | type | overrides |
|---|---|---|
| `default_provider` | string | `[stt].provider` |
| `whisper_model` | string | `[stt.whisper-local].model` |
| `stt_language` | string | `[stt].language` — `auto` or a tag like `en` or `pt-BR`; anything else is `400`. `base.en` and `small.en` ignore it, and a session that asks them for another language gets a warning as an `error` event |
| `dictation_provider` | string | none — dictation's STT provider, `whisper-local` when unset; deliberately separate from `default_provider` |
| `dictation_language` | string | `stt_language` — validated the same way |
| `mic_device` | string | `[audio].mic_device` |
| `loopback_device` | string | `[audio].loopback_device` |
| `retain_raw_audio` | bool | `[audio].retain_raw_audio` |
| `ollama_model` | string | `[llm.ollama].model` (summaries/titles) |
| `llm_provider` | string | `[llm].provider` — default for summaries and post-stop auto-titles |
| `redact_pii` | bool | none — when true, transcript text is scrubbed of PII/secrets before it is persisted, shown, or sent to an LLM (read at session start). Text only: a cloud STT provider receives the raw audio first, retained WAVs are not redacted, and a value split across two chunks can't match |
| `mcp_enabled` | bool | none — when true, serves `/mcp` (read per request; see [POST /mcp](#post-mcp--model-context-protocol)) |
| `retention_days` | integer | none — delete sessions that ended more than this many days ago; `0` or unset is off |
| `retention_clock` | object | none — the sweep's own record of the system clock, written by every sweep and read by nothing else; not for editing |
| `onboarded` | bool | none — set by the dashboard's first-run setup so it isn't shown again |
| `ui_theme` | string | none — the dashboard's theme, `dark` or `light`; the engine never reads it |

Unrecognized keys are stored and returned but have no engine effect.

`retention_days` deletes for good, with no trash: each session goes with its
transcript, summaries, retained asks, and retained audio. The sweep runs at
daemon start and every 24 hours after, and reads the value each time, so a
change lands at the next sweep, up to a day later. A value that is negative or
not a whole number is `400`, and the key is absent from `GET` until first set.
A session still recording is never touched, and neither are backups. A session
whose audio can't be removed stays, and the next sweep tries it again.

A sweep deletes by date, so a wrong system clock could make every meeting look
expired. Each sweep keeps its reading in `retention_clock`. One that finds the
clock more than 3 days past the last reading it believed deletes nothing until
the new reading has held for 7 days, and logs why on stderr. With no reading
kept yet, the end of the newest meeting stands in. Nothing local can tell a clock
jump from a machine left off, so more than 3 days off delays expiry by about a
week.

This narrows the risk and does not remove it, and a delete can't be undone:

- A clock that stays wrong for over 7 days still deletes.
- A jump under 3 days is believed, so meetings can expire up to 3 days early.
- A clock running slow stamps meetings too old. A meeting counts as no older
  than anything recorded before it, but one with no earlier meeting left to vouch
  for it expires by its own stamp once any pause is over.

```
$ curl -X PUT http://127.0.0.1:4820/api/v1/settings \
    -H "Content-Type: application/json" \
    -d '{"default_export":"md","ui_theme":"dark"}'
{"default_export":"md","ui_theme":"dark"}

$ curl http://127.0.0.1:4820/api/v1/settings
{"default_export":"md","ui_theme":"dark"}
```

PUT upserts the keys in the body (non-object bodies → `400`) and returns
the full settings object. On a running daemon that also holds
`retention_clock`, which this capture predates.

## PUT / DELETE /api/v1/secrets/{provider}

Store or remove an API key / bearer token in the OS credential store
(Windows Credential Manager). `{provider}` is one of `deepgram`, `groq`,
`openai`, or `token`; several providers share one entry (e.g. `groq`
covers batch STT and the LLM). Key resolution prefers an environment
variable of the same configured name, then the credential store, so a set
env var always wins. The value is written to the credential store only —
never to config, the database, logs, or any response; readiness surfaces
through `GET /api/v1/providers` (presence only).

```
$ curl -X PUT http://127.0.0.1:4820/api/v1/secrets/deepgram \
    -H "Content-Type: application/json" \
    -d '{"value":"dg_..."}'
{"id":"deepgram","stored":true}

$ curl -X DELETE http://127.0.0.1:4820/api/v1/secrets/deepgram
{"id":"deepgram","deleted":true}
```

Unknown provider id → `404`; empty value → `400` (both before the store is
touched). DELETE is idempotent and does not clear an env-var-provided key.

## GET /api/v1/egress

The egress ledger: one row per time the daemon sent user data to a third
party — and per time it explicitly stayed local. Metadata only; the audio,
prompt, and answer themselves are never recorded, only where they went and
roughly how much. Newest first.

The ledger is the daemon's own. `auricle record` writes nothing to it, even
with a cloud provider, and REST reads by local processes are not on it either
(see [Trust model](#trust-model)).

Query params: `session` (scope to one session id), `limit` (default 200,
clamped to 1–1000), `offset` (default 0, to page past the first `limit` rows).

| field | meaning |
|---|---|
| `ts` | unix seconds |
| `session_id` | owning session, or null |
| `destination` | `"cloud"` (Auricle sent it off the machine), `"local"` (stayed on-device), or `"agent"` (a local MCP client read it — see `POST /mcp`) |
| `provider` | e.g. `deepgram`, `groq`, `whisper-local`; for `agent` rows, the client's self-reported name |
| `host` | destination host for cloud rows, else null/local host |
| `kind` | `"audio"` (a session's live capture, one row per session), `"dictation"` (one dictation's audio, sent to a provider that is not on-device), `"prompt"` (an ask), `"summary"`, `"title"` (a post-stop auto-title sent to an LLM), or an MCP read kind |
| `items` | rough size — characters for text; null for audio |
| `detail` | short human note (e.g. `"ask + screen + transcript"`) |

```
$ curl http://127.0.0.1:4820/api/v1/egress?limit=2
{"entries":[
  {"id":8,"ts":1721348051,"session_id":"s_...","destination":"cloud",
   "provider":"groq","host":"api.groq.com","kind":"prompt","items":4213,
   "detail":"ask + transcript"},
  {"id":7,"ts":1721348010,"session_id":"s_...","destination":"local",
   "provider":"whisper-local","host":null,"kind":"audio","items":null,
   "detail":"live capture"}
],
 "totals":[{"destination":"cloud","who":"api.groq.com","count":14},
           {"destination":"local","who":"whisper-local","count":31}]}
```

`entries` is one page; `totals` counts the **whole** ledger by destination
and counterparty (the cloud host, or the provider / agent client name where
there is no host). The dashboard's headline reads from `totals`, so "nothing
has left this machine" can never be an artifact of the page size — which
matters once agents are reading, since their rows accrue much faster than
session or summary rows.

The counts are maintained on insert, in the same transaction as the row they
count, rather than derived with a `GROUP BY` at read time. Deriving them was
measured at 1.8 s over a million rows — all of it holding the store mutex,
so every other request waited on it. Reading the maintained counts is 0.04 ms
at the same size, and does not grow with the ledger.

## GET /api/v1/diagnostics

Build/runtime info plus recent crash records captured locally (a panic hook
appends to `<data_root>/diagnostics.jsonl`). Read-only, for pasting into a
bug report. **Local only** — nothing is sent anywhere, and no user content
(transcripts, prompts, audio) is included; records hold only the panic
message, source location, thread, and backtrace.

```
$ curl http://127.0.0.1:4820/api/v1/diagnostics
{"system":{"version":"0.3.0","os":"windows","arch":"x86_64"},
 "crashes":[{"ts":1721348051,"version":"0.3.0","os":"windows","arch":"x86_64",
   "thread":"tokio-runtime-worker","message":"...","location":"src/....rs:120:9",
   "backtrace":"..."}]}
```

## POST /api/v1/peek

Capture the active window and OCR it — on demand, one frame, locally
(Windows.Graphics.Capture + Windows.Media.Ocr). No body. Nothing
screen-derived is persisted; the result exists only in the response.
The "active" window is the foreground window, or the topmost eligible
window when the foreground is not capturable (untitled/shell/cloaked/
tool windows are skipped). `text` is flattened to reading order (bands
top-to-bottom, left-to-right within a band). `app_name` is the owning
executable's stem; `captured_at` is unix epoch ms; `ocr_ms` is OCR time
(capture typically adds ~90 ms warm). Real captured exchange (text field
abridged — the full response carried 2 640 chars of a VS Code window):

```
$ curl -i -X POST http://127.0.0.1:4820/api/v1/peek
HTTP/1.1 200 OK
content-type: application/json

{"window_title":"windows_ocr.rs - Visual Studio Code","app_name":"Code",
 "text":"File Edit Selection View Go Run Terminal Help\n@ windows_ocr.rs X Release Notes: 1.124.2\nD: > Auricle > crates > auricle-vision > src > windows_ocr.rs\nWindows implementation: Windows .Graphics.Capture (one frame, session\nclosed immediately) + Windows . Media.Ocr. No concealment APIs the\n…",
 "captured_at":1784088083684,"ocr_ms":199}
```

Errors (`{"error": …}` as everywhere): `409` when the desktop state
can't produce a capture right now (no eligible window, window minimized
or gone — retry later), `500` for machine-level failures (capture
unsupported/blocked, no OCR language pack, capture/OCR error).

## POST /api/v1/ask

The assistant service (the copilot's engine-side half): builds a prompt
from the question, optional on-demand screen OCR, an in-memory rolling
window of the live transcript (last `copilot.transcript_window_min`
minutes, default 10), and optional follow-up history — then streams the
LLM answer as Server-Sent Events. Every fragment is simultaneously
mirrored on `/ws/live` as `answer_delta` / `answer_done` / `ask_error`
events, so a UI holding the socket needs no second connection.

Body fields: `question` (required), `include_screen`,
`include_transcript` (both default false), `provider` (default:
`[copilot].provider`, falling back to the LLM settings/config default),
`session_id` (attach to a stored session — its title enters the prompt
and scopes follow-up history; default: the active session, if any),
`follow_up` (include prior Q/A from in-memory history; `false` starts a new
conversation and clears that history), `exclude_hwnd` (window handle excluded
from screen capture; used by the Phase 9 overlay so it never OCRs its own
answer).

Nothing question- or screen-derived is persisted unless
`[copilot].retain_context = true` (default off — with retention off the
`asks` table exists but has no rows). Ask history for follow-ups is
in-memory only and dies with the daemon. It is kept per session, the explicit
`session_id` or else the active session; asks made outside any session share
one history. An ask whose client hung up before it finished leaves no history
and no retained row.

Real captured exchange (Groq, while a session was recording a meeting
and a sprint-board page was the foreground window; abridged mid-stream):

```
$ curl -N -X POST http://127.0.0.1:4820/api/v1/ask \
    -H "Content-Type: application/json" \
    -d '{"question":"what was just being discussed and what'\''s on my screen?",
         "include_screen":true,"include_transcript":true,"provider":"groq"}'
data: {"ask_id":"a19f69571183","model":"llama-3.3-70b-versatile","provider":"groq","screen":{"app_name":"msedge","ocr_ms":74,"window_title":"Sprint Board — Demo Project - Personal - Microsoft Edge"},"transcript_segments":4,"type":"ask_started"}

data: {"ask_id":"a19f69571183","text":"The","type":"answer_delta"}

data: {"ask_id":"a19f69571183","text":" recent","type":"answer_delta"}

data: {"ask_id":"a19f69571183","text":" discussion","type":"answer_delta"}
…
data: {"ask_id":"a19f69571183","text":")","type":"answer_delta"}

data: {"ask_id":"a19f69571183","type":"answer_done","usage":{"completion_tokens":151,"prompt_tokens":546,"total_tokens":697}}
```

The opening `ask_started` event echoes exactly what context was
captured (the honesty affordance the overlay's context chips are built
from). `usage` is the provider's token report, when it sends one. SSE
keep-alive comments (`:`) bridge long silent stretches — a local
reasoning model can think for a long time before its first visible
token (see docs/PHASE8_ASSISTANT_REPORT.md for measured numbers).

Errors before the stream starts are plain JSON: `400` empty question /
unknown provider, `404` unknown `session_id`, `409` when the chosen LLM
provider is not ready (e.g. key env var unset) or when the screen
capture hit a transient desktop state (window minimized/gone — retry),
`500` for machine-level capture failures. After the stream has started,
failures arrive as an `ask_error` event on both surfaces:

```
data: {"ask_id":"a19f671…","type":"ask_error","message":"llm ollama returned HTTP 404: …"}
```

That includes a stream that ends before the model finished. An answer the
model cut off at its output limit is not an error: it ends with
`[Truncated: the model reached its output limit before finishing.]`.

Config (`[copilot]` in auricle.toml): `transcript_window_min` (10; `0` warns,
and asks and `auricle.live_transcript` then get no transcript),
`max_screen_chars` (6000), `retain_context` (false),
`provider` (unset → LLM default). The overlay's hotkeys are not set here: it
reads them from `[hotkeys]` (`summon`, `quick`) in its own
`%LOCALAPPDATA%\auricle\overlay.toml`. `[copilot].hotkey_summon` and
`hotkey_quick` still parse, but nothing reads them.

## GET /api/v1/asks

Asks kept under `[copilot].retain_context`, newest first. Empty unless it was on
when they were asked. Query params: `session` (scope to one session id),
`limit` (default 200, clamped to 1–1000), `offset` (default 0). There is no
`total`: a page shorter than `limit` is the last one.

The response is `{"asks":[…]}`, each row:

| field | meaning |
|---|---|
| `id` | row id, for `DELETE` |
| `session_id` | owning session, or null for an ask made outside one |
| `question` | the question as asked |
| `screen_context` | what the capture saw, including its OCR text, as a JSON string in the `/peek` shape; null when no screen was included |
| `answer` | the full answer |
| `provider` | LLM provider id |
| `created_at` | unix seconds |

## DELETE /api/v1/asks/{id}

Deletes one retained ask → `204`, no body. `404` unknown id, `400` an id that
is not an integer.

## POST /api/v1/backup

Writes a consistent copy of the database to
`<data_root>\backups\auricle-<unix seconds>.db`. No body → `201 {"path": …}`.
Recording and reads carry on throughout: the copy runs on a connection of its
own. `409` when a backup was already taken in the same second, because a backup
never overwrites.

A backup is the database only: retained audio is not included, and API keys
were never in it. It is an ordinary unencrypted SQLite file. Neither retention
nor `DELETE /sessions/{id}` touches it, so it keeps sessions deleted later.

## POST /api/v1/shutdown

Stops any active session cleanly, then exits. No body →
`202 {"status":"shutting_down"}` at once; the caller sees completion as the
port closing. The session ends as a normal stop, not an interrupted one. The
wait is bounded: up to 150 s for the stop, after which the session is left for
crash recovery, then up to 10 s for a post-stop auto-title, after which the
placeholder title stays. A session still starting, with no row yet, is not
waited for. From the moment it is asked for, `POST /sessions` returns `503`.

The overlay uses this before an update. An engine the overlay started and you
stop this way comes straight back: to the overlay, an exit it did not ask for
looks like a crash.

## WebSocket /ws/live

Server-push JSON events, one per text frame. Same-origin only, and on a
token-protected bind it takes `?token=` as well as the header (see
[Trust model](#trust-model)). Real captured sequence:

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
| `partial_cleared` | session, channel | that channel's current partial is abandoned: its final was dropped as speaker bleed, or had nothing left once overlap was trimmed, so no `final` will follow. Rides the deep lane, and any partials already queued go out ahead of it |
| `vu` | session, channel, rms | ~10 Hz per channel while capturing; rides the lossy lane (drives the UI meters) |
| `model_download_started` | session, model, size_mb | a start is waiting on a first-run model download |
| `model_download_progress` | session, model, done_mb, size_mb | about once a second while that download grows; `done_mb` reaching `size_mb` means the file is being verified |
| `session_started` | session, title, stt_provider | |
| `session_stopped` | session | emitted after flush + persistence complete |
| `session_updated` | session, title | post-stop auto-title landed (see POST /sessions) |
| `dictation_started` | id, provider | a dictation has the mic |
| `dictation_partial` | id, text | the dictation's text so far, sent as each final lands; rides the lossy lane |
| `dictation_stopped` | id | the dictation is over: stopped, cancelled, capped at 5 minutes, or failed to start. The text goes only to the stop's reply |
| `answer_delta` | ask_id, text | one fragment of an ask's streamed answer (mirrors the ask's SSE response) |
| `answer_done` | ask_id, usage | an ask completed; `usage` when the provider reported tokens |
| `ask_error` | ask_id, message | an ask failed (upstream LLM error, screen capture failure) |
| `device_lost` | session, channel, message | a capture device failed mid-session; sent within about a second, once per channel. The session keeps running on the other channel and the lost device is not reopened |
| `error` | session, message | recoverable engine/provider errors; `session` is empty on a stream-lag notice |
| `lag` | dropped_partials, dropped_answer_deltas | sent to a slow consumer after partials or answer fragments were dropped for it; both are running totals for the connection |

Slow-consumer policy: finals, lifecycle events, `partial_cleared`, `answer_done`,
and `ask_error` ride a deep (1024) buffer, so lag that sheds partials can't shed
the event clearing one; partials and `vu` ride a shallow (16) lossy one; and
`answer_delta` gets a 1024 buffer of its own, so a long answer can't use up the
slots finals wait in. A consumer that can't keep up loses partials and answer
fragments first and is told how many via `lag`. The answer on that socket then
has holes in it; the ask's own SSE response stays complete. A consumer more than
1024 events behind on the deep lane does lose finals, and gets an `error` event
saying how many (`event stream lagged; N events missed`). Those finals are still
in the database: re-read `GET /sessions/{id}`.

## POST /mcp — Model Context Protocol

A read-only view of the transcript for local agents (Claude Code, Cursor,
anything that speaks MCP), served over the Streamable HTTP transport on the
same port as everything else. **Off by default**: the endpoint answers `404`
until `mcp_enabled` is set, so a disabled server is indistinguishable from
one that was never built. Set it through the same settings route the UI
uses (Settings → Privacy → *Let local AI agents read your transcripts*):

```
$ curl -X PUT http://127.0.0.1:4820/api/v1/settings \
    -H 'Content-Type: application/json' -d '{"mcp_enabled":true}'
```

The setting is read per request, so the toggle takes effect without a
restart — including turning it back off. It governs `/mcp` only, not local
access to the REST API; see [Trust model](#trust-model). Install into Claude
Code with:

```
claude mcp add --transport http auricle http://127.0.0.1:4820/mcp
```

### Tools

All four are read-only and carry `readOnlyHint: true`. There is deliberately
no tool that starts or stops a recording, captures the screen, or calls an
LLM — through MCP, an agent gets the transcript and nothing else.

| Tool | Returns |
|---|---|
| `auricle.live_transcript` | The last `[copilot].transcript_window_min` minutes of speech (default 10), oldest line first, plus `session_id`, the session recording right now (null when none is). The window is a span of time, not a slice of one session: it survives a session stopping, so its lines need not belong to `session_id`. Reads the in-memory ring — no database hit. Deleting a meeting takes its lines out of the ring. |
| `auricle.search` | Transcript lines matching `query` across every recording, newest meeting first, each tagged with its session id, title, and offset. `limit` defaults to 20, capped at 200. |
| `auricle.list_sessions` | Recorded meetings, newest first, one page at a time: `limit` defaults to 50, capped at 200; `offset` defaults to 0; `total` counts every match. Optional `query` filters to meetings whose title or transcript contains that text. |
| `auricle.get_session` | One meeting: full transcript plus any summaries already generated. Transcripts are cut at 40 000 characters on a segment boundary, with `truncated: true` in the response — never a silent truncation. |

`speaker` is `"You"` (microphone) or `"Them"` (system audio), the same
two-voice model as the rest of the API.

The tool descriptions and server instructions tell clients to treat everything
returned as data, never as instructions: anyone on a call chooses the `"Them"`
text, and titles and summaries are generated from it. That is guidance to the
client, not something Auricle can enforce.

**What `redact_pii` does and does not cover here.** Redaction runs at the
engine's fan-out point, so transcript text captured *while the setting was
on* reaches these tools already scrubbed — in the ring and in the database
alike. Three things it does not cover, and an agent reading your history
will see all three:

- **Meetings recorded before you enabled it.** The setting applies to
  future sessions; it does not rewrite stored transcripts. Turning it on
  today does not redact last month's meeting.
- **Session titles.** They are written by an LLM from the transcript and
  are not passed through the redactor, so a title like
  `Budget call with alice@example.com` survives.
- **Summaries.** Also LLM output, also unredacted.

This is the same exposure the dashboard and `GET /api/v1/sessions/{id}`
have always had — MCP does not widen it — but an agent trawling old
meetings will surface it far more readily than a human clicking through
the UI. If that matters to you, keep the MCP server off for accounts with
unredacted history.

### Egress

Every tool call is written to the egress ledger with destination `agent` —
a third value alongside `cloud` and `local`. The ledger's usual rule is to
classify by endpoint host, which cannot work here: an MCP client is a local
process, but a local process may be the front end of a cloud model. Filing
these reads as `local` would let the dashboard claim "nothing has left this
machine" while an agent forwarded a transcript to a cloud model, so the
ledger records what it actually knows instead:

- `provider` — the client's self-reported name from the MCP handshake
  (`claude-code`, `cursor-vscode`). Self-reported, so it identifies a
  well-behaved client rather than authenticating anything.
- `kind` — `live_transcript`, `transcript_search`, `session_list`, or
  `session_read`.
- `items` — characters of user content in the response.
- `host` — always null. There is no remote host to name.

### Captured exchange

Real output from a running daemon seeded with one short meeting. The client
here is the integration harness, which identifies itself as
`integration-test`; Claude Code reports `claude-code`.

`initialize` (the handshake — the reply carries an `Mcp-Session-Id` header
that every later request must echo back):

```
$ curl -i -X POST http://127.0.0.1:4820/mcp \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
         "protocolVersion":"2025-06-18","capabilities":{},
         "clientInfo":{"name":"claude-code","version":"1"}}}'
HTTP/1.1 200 OK
content-type: text/event-stream
mcp-session-id: 7f3a…

data: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18",
 "capabilities":{"tools":{}},"serverInfo":{"name":"auricle","version":"0.4.1"},
 "instructions":"Auricle records the user's meetings locally — …"}}
```

`tools/call` on `auricle.search`. The result carries both a `content` text
block and `structuredContent` (schema-typed), which is what an agent reads:

```
{"jsonrpc":"2.0","id":3,"result":{"isError":false,"structuredContent":{
  "hits":[{"session_id":"s7c1f0a","session_title":"Pricing review",
           "speaker":"Them","started_at":1789000000,"t_start_ms":52800,
           "text":"and the annual discount stays at twenty percent"}]}}}
```

`auricle.live_transcript` with nothing recording and nothing said in the window
— an empty result, not an error. `session_id: null` is how a caller tells "no
meeting" from "a meeting where nobody has spoken for ten minutes":

```
{"jsonrpc":"2.0","id":4,"result":{"isError":false,
  "structuredContent":{"lines":[],"session_id":null,"window_minutes":10}}}
```

Those two calls, on the ledger afterwards. Note `destination: "agent"`,
the null `host`, and `items` counting characters of transcript — 47 for the
one matched line, 0 for the empty live window:

```
$ curl http://127.0.0.1:4820/api/v1/egress
{"entries":[
  {"id":2,"ts":1789523838,"session_id":null,"destination":"agent",
   "provider":"integration-test","host":null,"kind":"live_transcript",
   "items":0,"detail":null},
  {"id":1,"ts":1789523838,"session_id":null,"destination":"agent",
   "provider":"integration-test","host":null,"kind":"transcript_search",
   "items":47,"detail":null}],
 "totals":[{"destination":"agent","who":"integration-test","count":2}]}
```

`totals` counts the whole ledger while `entries` is one page, so the
dashboard's "nothing has left this machine" line is a fact about the ledger
rather than about how many rows fit on a page — agent reads arrive far
faster than session or summary rows and would otherwise push them off.

## Crash recovery

Sessions interrupted by a crash/kill are closed the next time the engine
starts: `ended_at` is set from the last persisted segment and
`meta.interrupted` is flagged. Read-only openers like `auricle export` leave
them alone. Captured after a real `taskkill /F` mid-session and restart:

```
$ curl http://127.0.0.1:4820/api/v1/sessions/s19f3102fb19
{"ended_at":1783233740,…,"meta":{…,"interrupted":true},"transcript":[…12 segments persisted before the kill…]}
```
