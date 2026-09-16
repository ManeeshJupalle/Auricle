# Changelog

## Unreleased

Your coding agent can read your meetings.

- **MCP server, off by default.** Turn on Settings → Privacy → *Let local AI
  agents read your transcripts* and the daemon serves Model Context Protocol
  at `/mcp`, so Claude Code or Cursor can answer "what did we just decide?"
  from the meeting you're in. Four tools, all read-only:
  `auricle.live_transcript`, `auricle.search`, `auricle.list_sessions`,
  `auricle.get_session`. Nothing an agent can call starts a recording,
  captures the screen, or runs a model. While the setting is off, `/mcp`
  answers 404 — a disabled endpoint looks like an absent one. The toggle
  takes effect immediately; no restart.
- **The egress ledger gained a third destination: `AGENT`.** An MCP client is
  a local process, so classifying its reads by endpoint host would have filed
  them as "stayed local" while an agent forwarded your transcript to a cloud
  model. The ledger now records what it actually knows — which client read,
  how many characters, when — and says plainly that where the agent sent it
  next is outside Auricle's view. The Home and Egress screens no longer claim
  "nothing has left this machine" when an agent has been reading.
- **Transcript search returns lines, not just sessions.** The existing search
  answers "which meetings mention this"; agents need "which lines", so
  `search_segments` returns matching lines tagged with their meeting and
  offset. Used by `auricle.search`; the dashboard's sidebar search is
  unchanged.

### Fixed (external audit of the above)

- **An MCP read is refused if the ledger can't be written.** The ledger write
  was best-effort, so a locked database meant the transcript went out with no
  row recorded — the one failure mode that would make the ledger untrustworthy
  rather than merely incomplete. MCP reads now log before disclosing and fail
  the call if that write fails.
- **Reads are attributed to the meeting actually read.** The ledger recorded
  whichever session was *recording* at the time, so reading last week's 1:1
  during today's standup filed the read against today's standup.
- **Ledger sizes count everything disclosed.** A `get_session` logged only
  transcript characters while also returning the title and every summary; a
  short meeting with a long summary understated the disclosure badly.
- **The ledger names the calling client, not the MCP library.** Client
  identity now comes from the request's own metadata. Reading it from the
  handshake meant stateless callers were filed against `rmcp`, which looks
  like a real client name and is not one.
- **MCP works on non-loopback binds.** rmcp's default `Host` allowlist accepts
  only loopback, so a daemon bound to a LAN address with a bearer token
  answered every MCP request with 403. Auricle's own middleware already does
  this check, and does it with knowledge of the token.
- **`live_transcript` no longer implies its lines belong to `session_id`.**
  The window is a span of time, not a slice of one meeting: it survives a
  session stopping. Behaviour unchanged (`/ask` depends on it) — the tool now
  says so instead of implying otherwise.
- **Documented what `redact_pii` does not cover for agents**: meetings
  recorded before it was enabled, LLM-written session titles, and summaries.
  Unchanged from what the dashboard has always exposed, but an agent trawling
  old meetings surfaces it far more readily than a human clicking through.
- **The ledger headline no longer scans the ledger.** Egress counts are now
  maintained on insert, in the same transaction as the row they count, and
  seeded from existing history on upgrade. Deriving them with a `GROUP BY`
  cost 1.8 s over a million rows with the store mutex held, so every other
  request queued behind a dashboard load; reading the maintained counts is
  0.04 ms at that size and does not grow with the ledger.
- **MCP database work runs off the async runtime.** rmcp dispatches tool
  functions on a Tokio worker without offloading them, so an unbounded read —
  a whole meeting, a transcript-wide search — occupied a worker while holding
  the store mutex, stalling unrelated requests and the capture pipeline
  sharing that runtime. The tools are async now and their store access goes
  through `spawn_blocking`, as `POST /api/v1/peek` already did.

## 0.4.1 — 2026-08-10

A design pass over the dashboard, and the fixes it turned up.

- **The session map.** A finished session now opens with a band showing who
  spoke when across the whole recording — Them above the centerline, You
  below, the same two-voice geometry as the live listening strip. Click any
  block to jump the transcript there, and the audio with it when the
  recording was retained. Silence shows as silence.
- **One reading column.** The title, metadata, tabs, session map, transcript,
  and summary all resolved to different left edges — the header sat about
  80 px left of the document under it. They now share one measure, so the
  page reads as a single document.
- **Fixed: the sidebar search icon sat on top of its own placeholder.** The
  generic `input[type='search']` rule outranked `.sidebar-search` on
  specificity and silently dropped the padding that made room for the icon.
- **Red means recording again.** Summary headings, list bullets, and the
  summary card header had all borrowed the accent, which is the colour this
  product uses for "a session is live". They're neutral now; the record
  button and the REC dot own red.
- **Summary controls say what they select** — the two unlabelled dropdowns
  are now labelled Template and Written by, in a tighter toolbar.
- Export is a button rather than accent-coloured text buried in the metadata
  row; action-item checkboxes are styled instead of raw form controls; the
  tab is "Summary", not "AI Summary".

## 0.4.0 — 2026-08-07 (one desktop app)

The dashboard moves into the desktop app, and gets redesigned around the
product's two voices.

### One app

- **Launching the app opens the dashboard.** A tray-only launch looked
  like nothing happened, so users launched again — and instances stacked
  up. The app now enforces a single instance (a relaunch focuses the
  dashboard), and the copilot overlay stays on its hotkeys.
- **The dashboard opens in its own app window.** Tray → Open dashboard now
  shows the full dashboard (sessions, transcripts, summaries, settings) in
  a native window instead of launching the browser — one installed app is
  the whole product. The window waits for the engine's health check before
  loading, and closing it hides it; the tray and overlay stay resident.
  The engine still serves the same web UI and API on :4820 for browsers
  and `curl`.
- **Fix: the overlay's × button after `Ctrl+Shift+Space`.** When the ask
  input autofocused, the window's focus-loss handler saw the internal
  WebView2 focus change as "user switched away" and made the overlay
  click-through — keyboard worked, every mouse target was dead.
  Click-through now engages only when another app is actually the
  foreground window; the bystander behavior is unchanged.
- Removed the browser-opener plugin the old dashboard link needed.

### Dashboard redesign

- **The listening strip:** the live session header shows a scrolling
  two-voice waveform of what the engine hears — Them (system audio) above
  the centerline, You (mic) below — fed by the real VU stream, rendered on
  canvas outside React's render path. A flat blue line during a call means
  a dead microphone. `prefers-reduced-motion` falls back to the two bars.
- **A real home view:** the You/Them model up front, provider readiness
  chips (what can transcribe and summarize right now), the privacy line
  straight from the egress ledger ("Nothing has left this machine."), and
  the copilot hotkeys — none of which duplicates the sidebar.
- **Two-voice transcript:** each speaker turn carries a colored rail, so a
  long transcript scans as a conversation.
- **Sidebar:** sessions grouped by day (Today / Yesterday / This week /
  Earlier); icon nav for Settings, Egress, and About.
- **Quality floor:** visible keyboard focus rings, reduced motion
  respected everywhere, the connection indicator is a quiet dot when
  healthy and a loud pill only when reconnecting, and full light-theme
  parity.

## 0.3.0 — 2026-07-15 (first public release)

One launch, three layers: the local-first transcription engine, the
embedded dashboard, and a screen-aware desktop copilot built on the
same public API.

### Engine

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
- **Security:** same-origin enforcement for browser requests — foreign
  pages cannot open `/ws/live` and read transcripts (WebSocket handshakes
  bypass CORS); tokenless loopback binds reject non-localhost `Host`
  headers (DNS rebinding); bearer tokens compared in constant time;
  retained-audio serving is path-contained under the engine's sessions
  directory (defense-in-depth against database tampering).
- **Screen peek (`auricle-vision`):** on-demand single-frame capture of
  the active window (Windows.Graphics.Capture) + local OCR
  (Windows.Media.Ocr) behind a `ScreenReader` trait; reading-order
  flattening; typed errors, never panics; warm capture→text 175–298 ms at
  1080p. `POST /api/v1/peek` and `auricle peek [--json]`. Strictly on
  demand: no capture loops, nothing screen-derived persisted, no
  concealment APIs anywhere.
- **Performance:** whisper models load once and are cached across
  sessions; select-based channel driver (up to 50 ms less capture→partial
  latency); retained audio is 16-bit PCM served with HTTP Range support;
  bounded session stop; first-run model downloads and queue shedding are
  announced over the WebSocket.
- **Benchmarks:** real-time-paced latency harness; honest numbers and
  budget misses in benches/RESULTS.md.

### Dashboard (embedded web UI)

- Virtualized live transcript with single-row partial updates, VU meters,
  latency readout, provider pickers.
- Sessions browser with full-text search (titles + transcript content),
  inline rename, delete, markdown export.
- LLM auto-titles after stop (offline fallback: first words of the
  transcript).
- Synchronized audio playback with click-timestamp-to-seek for sessions
  recorded with raw-audio retention (off by default).
- Dark/light themes; one self-contained binary, no Node at runtime.

### Summaries

- OpenAI-compatible LLM client (Ollama / Groq / any base_url), keys via
  environment only.
- Four overridable templates (minutes, action-items, standup, 1on1) plus
  user-added `.md` templates without recompiling.
- Map-reduce for long transcripts; summaries persisted and appended to
  exports.

### Copilot (assistant service + overlay)

- **`POST /api/v1/ask`:** streams an LLM answer assembled from the
  question, an on-demand screen capture, and a rolling in-memory
  transcript window (last 10 min, configurable) — SSE on the response,
  mirrored as `answer_delta` / `answer_done` / `ask_error` on `/ws/live`.
  Follow-up questions see in-memory ask history. Streaming SSE parser
  derived from captured Ollama/Groq frames (reasoning-model chain of
  thought is never surfaced).
- **Privacy default:** nothing question- or screen-derived is persisted
  unless `copilot.retain_context = true`; with it off the database schema
  never even grows the table.
- **Overlay (`overlay/`, Tauri v2):** Ctrl+Shift+Space summons an
  always-on-top ask card in ~100 ms; Ctrl+Shift+A is one-keystroke
  quick assist (screen + transcript); Esc dismisses. Context chips name
  exactly what was captured. Streaming markdown answers, copy button,
  follow-ups, provider + elapsed-time readout. Tray icon starts/stops
  recording, opens the dashboard, and spawns the engine if it isn't
  running. The overlay is a normal, visible window — no concealment
  APIs — and talks only to the public API, passing its own window
  handle so captures never include the overlay itself.
- **MSI installer:** per-user by default (no elevation), engine bundled
  as a sidecar; per-machine via documented msiexec flags.
- **Overlay hardening:** strict webview CSP; the engine attach validates
  the health payload shape before routing asks, so an unrelated process
  squatting the port is never mistaken for the engine.

Measured on the reference laptop (i7-9750H): capture→partial 0.14 s
(Deepgram p50), hotkey→overlay visible 91–122 ms, ask time-to-first-token
0.7 s (Groq) — local-model copilot numbers and their misses are
documented in docs/PHASE8_ASSISTANT_REPORT.md.
