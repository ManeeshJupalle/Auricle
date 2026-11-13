# Changelog

## 0.5.1 — 2026-11-13

A new look, a tray switch for screen shares, and two things that could stop a
first install from working.

0.5.0 never shipped publicly: its release stayed a draft. Upgrading from 0.4.0 also brings the 0.5.0 and 0.4.1 changes listed below.

- **The installer now carries the Visual C++ runtime.** The engine needs four
  of Microsoft's runtime DLLs to start, and Windows doesn't come with them, so
  on a PC where no other program had installed them the engine never came up.
  The MSI now puts them beside the engine. Releases before this one have the
  same gap.
- **Setup no longer says Ollama is ready when it isn't there.** Ollama was
  always listed as ready, because it has no key to check. The engine now asks
  it: setup, the home screen, and Settings say when Ollama isn't answering or
  doesn't have the model, and what to install. A summary or an ask that fails
  for either reason says the same, in place of a connection error. The check
  runs only while Ollama is the LLM in use, and takes most of a second when
  it isn't there.
- **The copilot card can be kept out of screen shares, if you ask.** The tray
  menu has a **Hide copilot from screen shares** item, off by default. It
  takes effect at once, with no restart, and is written to `overlay.toml`
  (`hide_from_screen_share = true` under `[overlay]`) for the next launch,
  leaving the rest of that file as you wrote it. This reverses the earlier "no
  concealment" rule: the card is still visible in a share unless you turn this
  on, and it is hidden from captures only, not from anyone looking at your
  screen.
- **A new look for the dashboard: slate and glass.** The green pad gives way
  to a cool slate in the dark theme and a cool white in the light one. The
  meeting list, the audio player, dialogs, and the controls are now glass,
  floating over a soft wall of colour. The page you read stays solid, so
  nothing shows through the text. The player floats over the foot of the
  transcript, and Transcript, Summary, and Asks are one segmented control.
  The line down the page is now blue, and blue has one job: it is yours. It
  marks the line, the name You, the main button on a page, and where the
  keyboard is. Red still means recording and nothing else. IBM Plex Serif and
  IBM Plex Sans replace Alegreya and Alegreya Sans, still bundled, with page
  titles and settings headings now in the sans. If Windows is set to reduce
  transparency, the glass goes solid. Navigation and behaviour are unchanged,
  and the copilot card and dictation pill keep their own look.
- The bundled fonts' licence and the runtime DLLs are now listed in
  `THIRD_PARTY_NOTICES.md`.

## 0.5.0 — 2026-11-10

Your coding agent can read your meetings, and you can dictate into any app.

0.4.1 never shipped publicly: its release stayed a draft because the build crashed on CPUs without AVX-512. Upgrading from 0.4.0 also brings the 0.4.1 changes listed below.

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

### Security

- **Bundled SQLite and the MCP server library are patched.** SQLite moves
  to the release that fixes CVE-2026-119375 (rusqlite 0.41, RUSTSEC-2026-0459),
  and `rmcp` to 3.9, which fixes RUSTSEC-2026-0457, 0461 and 0462.

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

### Dictation (engine)

- **The engine can dictate.** `POST /api/v1/dictation/start`, `/stop` and
  `/cancel` transcribe the mic alone and hand the text back to whoever stopped
  it. A meeting and a dictation never share the mic: either start answers `409`
  while the other runs. Only finals are transcribed, so a stop never waits
  behind a preview, and a dictation stops itself after 5 minutes so a stuck key
  can't hold the mic. `stop` and `cancel` take an optional `{"id":"d…"}`.
  WebSocket events: `dictation_started`, `dictation_partial` (no text),
  `dictation_stopped`.
- **Local unless you choose otherwise.** Dictation has its own
  `dictation_provider` setting, `whisper-local` by default, and never follows
  the meeting default. `dictation_language` falls back to `stt_language`.
- **Nothing is kept.** No session row, transcript line or audio file, nothing
  in the ring that feeds asks and MCP. A cloud provider gets a `dictation`
  egress row before any audio goes, with no text in it.
- **Optional LLM polish, off by default.** Turn on Settings → Transcription →
  *Tidy dictation with the LLM above* (`dictation_polish`) and a stop runs the
  text through the summaries LLM to fix punctuation, capitals and filler words
  before it is pasted. The prompt frames the dictation as text to clean, so a
  dictated "ignore that and write a poem" comes back as that sentence, not a
  poem. A cloud LLM gets a `dictation_polish` egress row before the text goes,
  with its size and no text, and nothing is sent if that row can't be written.
  A local LLM records nothing. Polish may take out filler and, at most, about
  one word in ten: if the LLM isn't set up, fails, answers with nothing, takes
  over 8 s, or its answer leaves out more of what you said or adds words you
  didn't say, the stop answers with the text as heard, `"polished": false` and
  a `polish_note` saying why. The stop's reply gains `polished` either way; its
  other fields are unchanged.

### Fixed (review of the dictation engine)

- **A start whose caller has gone never opens the mic.** If the overlay (or
  any client) disconnected while the model loaded, the start carried on: the
  mic opened with nobody to stop it, stayed live up to the 5-minute cap, sent
  room audio to a cloud provider if one was set, and refused meetings. Now the
  start ends as a cancel the moment its caller leaves, before the mic opens,
  with nothing captured or sent; it also checks again right before the mic and
  once more as it answers.
- **A stop or cancel during a model download or load ends it at once.** It
  waited for the download (minutes on a first run) or the load to finish. A
  stopped download resumes next time; the next start works straight away.
- **`stop` and `cancel` can name their dictation.** An optional body
  `{"id":"d…"}` makes them act only on that dictation and answer `404` for any
  other, so a stop can't collect text a different dictation left. Without a
  body they work as before. A stop landing in the instant between a start
  taking the mic and recording its handle could find no dictation, or the
  previous one's; both now happen under one lock.
- **The stop says when words may be missing.** An STT error on one chunk,
  finals shed because transcription fell behind, or a transcription task that
  stopped partway used to answer `200` with the gaps unmarked whenever some
  text was heard. The reply now carries `"incomplete": true` and a short
  plain `note`; `incomplete` is `false` otherwise.
- **Polish checks the words, not the length.** The length bounds let short
  answers through: "what is two plus two" came back as "Four.", and a summary
  half the length passed. The answer must now keep at least 90% of what was
  said, filler aside, in order, and add no more than 10% new words. A
  dictated `</dictation>` is escaped, so it can't close the prompt's frame.
- **A cancel sends nothing more to the cloud.** A cancelled dictation on a
  batch provider still uploaded the next queued chunk whenever the one in
  flight came back empty; a Deepgram session sent what it still held, and
  whatever it had buffered during an outage, before closing. Dropped
  unfinished, both now discard it and abandon the upload under way.
- **`dictation_partial` no longer carries the text.** Every `/ws/live`
  subscriber gets it, the dashboard included, and nothing used the text. It
  now says only that a final landed: `{"type":"dictation_partial","id":"d…"}`.
- **The last word isn't clipped.** The mic stays open 250 ms after a stop, for
  audio still in the capture buffers, but never past the 5-minute cap.
- **A panic mid-dictation closes the mic.** The capture stream stayed open
  until the next start. A start's settings read and a stop's check of
  `dictation_polish` also moved off the async runtime, and the Privacy page
  names the `dictation` ledger kind ("Dictation audio").

### Dictation (overlay)

- **Hold `Ctrl+Shift+D`, talk, let go: the text lands where your cursor was.**
  Set `[dictation] mode = "toggle"` in `overlay.toml` to press once to start
  and again to paste, and `[hotkeys] dictate` to pick other keys. The press
  goes through the same global-shortcut plugin as the other hotkeys; there is
  no keyboard hook. `Esc` cancels, and is only taken while a dictation runs.
- **It pastes into the field you started in, or nowhere.** The overlay
  remembers the exact window and the focused field at the press. If either has
  changed, closed, or runs at a higher integrity level, nothing is typed
  anywhere and no window is pulled to the front: the text is left on the
  clipboard and the pill says so. Ctrl+V waits until you've let go of Ctrl,
  Shift, Alt and Win.
- **A paste counts only when the app reads it.** The text is offered on the
  clipboard and handed over when the target app asks for it. Your own clipboard
  comes back half a second after that, unless you copied something meanwhile,
  which is kept. If the app never reads it within 3 s, as with a read-only
  field, the text stays on the clipboard and the pill says so, rather than your
  old clipboard silently replacing it. A few apps read without saying who they
  are, Windows Terminal among them: they get the text, but the paste is marked
  unconfirmed and your old clipboard is left alone. Dictated text is marked so
  Windows leaves it out of clipboard history (Win+V) and cloud clipboard sync,
  the clipboard fallback included.
- **Nothing can get stuck.** Every step of a dictation has a deadline, and `Esc`
  is released as soon as a dictation ends, or 30 s into transcribing.
- **A small pill shows what's happening**: listening, transcribing, pasted,
  or what went wrong in plain words. It never takes focus from the app you're
  dictating into. Tray → *Dictation setup* checks the hotkey, the engine, and
  which provider will hear you (local `whisper-local` is the default).

### Your data

- **Meetings can expire.** Settings → Your data can delete meetings after N
  days, with their transcripts, summaries, retained asks, and audio. Off by
  default, permanent, and there is no trash. It runs at every start and once a
  day, never touches a meeting in progress, and refuses a `retention_days` that
  isn't a whole number of days rather than appearing to accept it. A system clock
  more than 3 days past the last sweep pauses deletion until that reading has
  held for 7 days, recorded in the `retention_clock` setting, so a wrong date
  can't mass-delete; a machine left off that long pauses it too. The protection
  has limits and a delete can't be undone: a clock wrong for over 7 days still
  deletes, and a jump under 3 days can expire meetings up to 3 days early.
  Before the first sweep the newest meeting stands in for the last sweep, so an
  upgrade whose newest meeting is over 3 days old deletes nothing for 7 days,
  and the log line says it is measuring from that meeting.
- **Backups.** *Back up database*, or `POST /api/v1/backup`, writes a consistent
  copy to a `backups` folder beside the database while recording and reads carry
  on. It holds the database only, not retained audio, and retention leaves it
  alone, so a backup keeps meetings you delete later.
- **Retained asks can be read and removed.** With `copilot.retain_context` on,
  asks went into the database with no way to see or delete them. A meeting now
  shows its asks under an Asks tab, and `GET /api/v1/asks` and
  `DELETE /api/v1/asks/{id}` do the same over the API.
- **Deleting a meeting takes everything with it.** It used to leave retained
  asks behind, screen text included, and a summary could land after its session
  had gone. Asks now go with the meeting, the upgrade clears what earlier deletes
  left, and freed database pages are zeroed instead of holding the old text. The
  text also leaves the rolling window, so `/ask` and `auricle.live_transcript`
  can't return it.
- **A delete can't leave audio behind.** Rows went first and audio was removed
  best-effort, so a failure reported success and left the WAVs for good. Audio
  now goes first: if it can't be removed, `DELETE /sessions/{id}` returns `500`
  and keeps the meeting, and a retry works.
- **The `asks` table now always exists.** The 0.3.0 notes below say it is never
  created with retention off. Deleting a meeting relies on it now, so it is
  created empty on upgrade. No row is written unless `copilot.retain_context` is
  on.
- **Retained audio lines up with the transcript.** Capture delivers nothing
  during system silence, so a WAV drifted off transcript time and seeking landed
  in the wrong place. Files now follow the session timeline, padded with silence,
  and are flushed every 5 s, so a hard kill leaves them playable up to the last
  flush. A channel's file stops with an `error` event when its padding would pass
  12 h in total, because resuming without it would misalign playback, or at a 36 h
  track length. The transcript carries on.
- **Migrations are all or nothing, and a newer database is refused.** A kill
  mid-upgrade leaves the database at the version it started from, and a database
  written by a newer build is refused rather than risked.

### Recording

- **A lost device is reported during the meeting.** `device_lost` was only ever
  sent after the session had ended, so a dead microphone stayed quiet until you
  stopped. Capture streams are now checked every second and the dashboard says
  so at once, once per channel. The session keeps running on the other channel.
  Notices from a stream that is still running no longer read as a lost device.
- **Sessions record what they lost.** `meta.dropped` gives audio lost to capture
  overflow and chunks shed by the provider's queue, per channel, so a hole in the
  transcript can be told apart from silence after the live warnings are gone.
- **Far-side speech off laptop speakers appears twice less often.** The mic hears
  the speakers, so "Them" came back as "You". A mic line of five or more words
  that starts 0 to 500 ms after a matching loopback line is now dropped, and
  counted in `meta.dropped.bleed_finals`. The window is narrow, so some bleed gets
  through, such as chunks of long speech and the first lines after loopback
  silence, and the thresholds have not been measured on real bleed. See
  Limitations in the README. `[stt] drop_mic_bleed` defaults to true; set it to
  false with a monitor mix, where a match can delete your own words.
- **Finals end in the likeliest gap between words.** They were cut at a hard 5 s,
  wherever that landed. A final now ends at the quietest VAD frame of its last
  second, so finals run 4–5 s.
- **Live partials no longer stop partway through a meeting.** Partials were held
  back until earlier finals came home, but a chunk that transcribed to nothing
  never reported back, so two coughs were enough to end partials for the rest of
  the session. Every chunk now reports when it is handled, whether or not it
  produced text, and Deepgram's incremental feed is never held back.
- **Languages other than English.** `[stt] language`, `auricle record --language`
  and Settings → Transcription take `auto` or a tag like `de` or `pt-BR`. The new
  `base` and `small` models cover any language. The `.en` models stay
  English-only, and a session that asks them for anything else says so. A
  malformed value is refused when it is saved, because it ends up in a provider's
  URL. In Chinese and Japanese, overlap words may repeat at chunk seams and the
  bleed filter won't fire, because both count words by spaces.
- **Devices with more than two channels use the front pair.** Averaging every
  channel diluted stereo on a surround endpoint. The other channels are now used
  only while the front pair is silent.
- **A dropped line's partial goes with it.** When a final is dropped as speaker
  bleed, the row showing its partial used to linger until the next line on that
  channel. A new `partial_cleared` event removes it, on the important lane so lag
  can't shed it. A final that transcribes to nothing still leaves its partial
  until the next one.
- **Deepgram outages are bounded.** Audio held for a reconnect is capped at 60
  chunks per channel. Anything shed past that is counted and announced.
- **Timeouts say they are timeouts.** They used to read "error sending request".
- **A start survives its client.** Reloading the dashboard during a long
  first-run model download used to abandon a half-built session holding the slot.
  The start now finishes on its own. A stop that lands first ends it with `409`
  and no row, and a start that fails after its client left says why on the
  WebSocket.
- **One daemon per data root.** A second `auricle serve` on another port got as
  far as crash recovery, which closed the running daemon's live session. It is
  refused now, before touching anything.
- **`POST /api/v1/shutdown`** stops the active session cleanly, then exits, with
  bounded waits for the stop and for the auto-title. Closing the console now does
  the same stop, and the app does it for an engine it started when Windows logs
  off or shuts down, within a 4 s budget. A stop that misses the deadline is left
  for crash recovery. Once a shutdown is under way `POST /sessions` returns `503`,
  and `/health` now reports the daemon's `pid`.
- **Stock phrases invented on silence are dropped,** on local and batch Whisper
  alike. Whisper writes "Thank you." on a breath, and people really say that, so
  text alone never drops anything: a phrase goes only when the model's no-speech
  probability is high, or, for a provider that gives none, the audio's energy is
  low.
- **Repetition loops are cut to one copy,** on local and batch Whisper alike.
  Whisper sometimes repeats a word or phrase until it runs out of tokens, and
  that used to reach the transcript. A finished line now keeps one copy of a
  word repeated 10 or more times, or of a phrase repeated 6 or more, including
  Chinese, Japanese and Korean written without spaces. Shorter runs are left
  alone because people say them ("no no no", "ha ha ha ha"), and so are runs
  inside a word ("hahaha", "1000000"). Ported from voicebox (MIT; see
  THIRD_PARTY_NOTICES.md).
- **Deepgram reconnects without losing speech.** Up to 10 s of audio no final has
  covered yet is re-sent after a reconnect, so that audio can reach Deepgram
  twice. Auth errors stop the retries instead of looping. Audio that goes
  unanswered is replayed once on a new connection, then given up on with an
  error.
- **Deepgram's last words arrive without more speech.** The server can sit on a
  short line with no answer at all, so Auricle now asks for it 1.5 s after the
  last audio, and such a final can land about 1.5 s after speech ends.
- **Model downloads resume** from where they stopped.

### Copilot

- **The overlay moves.** Drag it by its header and it stays there until the app
  restarts, moving only if growth would run it off the screen.
- **New chat.** A `+` in the header starts over. A new conversation now clears
  the engine's follow-up history too; before, the old conversation came back with
  the next follow-up.
- **A screen toggle.** A button in the header keeps the next asks, quick assist
  included, from reading your screen. It is on at every launch.
- **Follow-ups stay in their meeting.** History was keyed on the session id in the
  request, which the overlay never sends, so one meeting's Q&A reached the next
  one's prompt. It is keyed on the session the ask resolved to now. An ask
  abandoned before it finished leaves no history and no retained row, and
  dismissing the overlay abandons the one in progress.
- **A dead engine comes back.** If an engine the app started dies, the app says
  so and restarts it, up to five tries in a row, and gives up after five crashes
  in an hour until you relaunch it. Crash notices come at most every 10 minutes,
  so a crash loop can't keep taking focus. A recording that was running is closed
  as interrupted, not resumed. An engine that stays alive but stops answering is
  never killed: after 2 minutes the app says so. An `auricle serve` you started
  yourself is left alone. The engine's stderr now lands in `engine.log`, the
  previous run's in `engine.log.old`, and `engine.pid` records which engine is the
  app's, all under `%LOCALAPPDATA%\auricle`.
- **Updates wait for the recording, and stop only the app's own engine.** A
  running `auricle.exe` stays locked against the installer, and the relaunched
  overlay would attach to the old version. Check for updates now refuses during a
  recording, and refuses if its own engine is alive but not answering. An engine
  you started yourself is never stopped: the app tells you how to stop it. The
  overlay warns before handing off to the installer, because nothing can report a
  failure after that point.
- **Quit doesn't end a recording by surprise.** A recording the app doesn't own
  keeps running. Its own recording needs a second Quit within 10 seconds, which
  stops it and exits.
- **The app says what is wrong.** The tray tooltip shows recording and offline,
  and the update nudge and hotkey warnings bring the overlay up rather than
  waiting in a hidden window.
- **Cut-off answers say so.** A summary or answer that hits the model's output
  limit ends with a truncation notice, and a stream that ends early or errors
  partway now fails instead of passing for a finished answer.
- **A long summary keeps what finished.** One failed portion used to lose the
  whole summary, which on a slow local model can be hours of work. The finished
  portions are now kept, and the summary ends with `[Incomplete: ...]` saying what
  is missing. Only a failure on the first portion still fails the request.
- **Local models get longer to answer.** A call to Ollama, or to any endpoint on
  this machine, may stay silent for up to an hour, against 10 minutes for a cloud
  provider, because a long prompt on a CPU can take over 10 minutes to start.

### Dashboard

- **A new design, built around one line.** The dashboard now takes after a
  stenographer's pad, the one object made for writing down what people say: pale
  green paper with a rule down the page. That rule runs through the product. In
  a meeting it divides Them from You and carries the tape of who spoke when; live,
  it grows as people talk and breaks where a device is lost; on the Privacy page
  it is the edge of your machine, and each ledger row sits inside it or crosses
  it. Them reads in a flush serif and You in an indented sans, so the voices
  separate without relying on colour. Red now means recording and nothing else:
  it used to double as the colour for Them. Geist gives way to Alegreya and
  Alegreya Sans, still bundled, and the Egress tab is now called Privacy.
- **Search finds the moment.** It lists matching lines with their timestamps,
  marks them on the line, and jumps to the moment, through a new
  `GET /api/v1/search`. It used to say which meetings matched and leave you to
  reread them.
- **Playback follows along.** The transcript tracks the audio and marks the turn
  being played, until you scroll away, and clicking the line seeks.
- **First-run downloads show real progress.** A new `model_download_progress`
  event reports megabytes done, so waiting to record no longer means guessing
  between ten seconds and five minutes.
- **Keyboard and screen readers.** Controls take keyboard focus and carry
  labels, status changes are announced, and the session map is one tab stop that
  arrow keys step through.
- **Narrow windows get their own layouts,** at 1080 px and 640 px.
- **The provider picker follows Settings.** A change there replaces a pick made
  in the sidebar, so a default switched to local for a confidential call can't
  lose to a stale cloud pick. A sidebar pick applies to the next session only.
- **whisper-local can be chosen before its model is downloaded.** It fetches its
  own model on first use, so nothing is missing that you have to supply.
- **Transcript turns break** on a 5 s pause or after 60 s.
- **The session list pages,** and the Egress tab gained a session filter, a Show
  more button, and an Auto-title label.
- **Token-protected daemons work in the browser.** The dashboard asks for the
  token, keeps it for that tab only, and sends it.

### API and security

- **REST is same-origin only, like `/ws/live`.** Any localhost origin used to be
  allowed, so another page on this machine could POST to the daemon without a
  preflight. A page on another localhost port now gets no more than any other
  site.
- **Responses carry `nosniff`, a CSP, and `Cross-Origin-Resource-Policy`.** The
  last one covers what an origin check can't see: an `<audio>` on a foreign page
  sends no `Origin`, and could otherwise play retained audio from a tokenless
  daemon.
- **`?token=` on `/ws/live` and retained audio, and nowhere else,** because a
  browser cannot set a header on either. Every other API route still demands the
  header.
- **`docs/API.md` now states the trust model.** Other local processes can reach
  REST on loopback with no token, and the MCP toggle and the ledger govern MCP
  only. Nothing changed there; the code cited a trust model the docs didn't have.
- **Auto-titles are on the ledger.** The opening of the transcript went to the
  titling LLM with no row, which left "nothing has left this machine" wrong for
  anyone using a cloud LLM. They now appear as kind `title`.
- **Paging.** `GET /api/v1/sessions` takes `limit` and `offset` and always returns
  `total`, `GET /api/v1/egress` takes `offset`, so rows past the first 1000 can
  be reached, and `auricle.list_sessions` takes `limit` and `offset` and returns
  `total`.
- **Long answers can't crowd out finals.** Answer fragments ride their own lane on
  `/ws/live`, and `lag` gains `dropped_answer_deltas`. The docs no longer claim
  finals are never dropped: a consumer over 1024 events behind does lose them, and
  is told.
- **`/providers` reports the real default.** `default` now honours the
  `default_provider` setting.
- **MCP output is marked untrusted.** Anyone on a call chooses the "Them" text, so
  the tool descriptions and server instructions now tell clients to read it as
  data and never follow instructions in it.
- **Redaction matches current key shapes** (`sk-proj-…`, `gsk_…`), and digits said
  right next to a card no longer hide it from the Luhn check.
- **`auricle.toml` is found without `--config`.** The daemon read no config file
  unless told to, which left the app's engine with no way to get one.
  `%LOCALAPPDATA%\auricle\auricle.toml` is now loaded if it exists, and the file
  in use prints as `using config <path>`. A bad value fails a command only if that
  command needs the key: `server.bind` for `serve`, `[audio]` for `capture`, and
  `[audio]`, the language, and the active provider for `record`. Anything else
  wrong, unknown keys included, warns. `--config` stays strict about the file
  itself. A file that was only found can't fail the load: if it is broken or a
  needed key is bad, it warns and the defaults apply, though `record` still stops
  on a bad provider. `transcript_window_min = 0` warns, and asks then get no
  transcript.
- **A rename during auto-titling sticks,** and a transcript with multibyte text
  around the prompt cap no longer panics the titler. A summary finishing after its
  session was deleted returns `404` instead of a `201` for a row that doesn't
  exist.

### Building

- Rust is pinned to 1.95.0 by `rust-toolchain.toml`, and Node must be ^22.12,
  ^24, or >=26: the dashboard and overlay build with Vite 8 and test with
  Vitest 5, which dropped Node 20. The overlay moved to Tauri 2.12.
- Releases run in a `release` environment and must pass a gate before anything
  is signed: the tag must match the version in every file that carries it, tests
  must pass, and the build flags are checked.
- `cargo build --features vulkan` opts local Whisper into the GPU, with
  `use_gpu = true` under `[stt.whisper-local]`. It needs the Vulkan SDK, is off by
  default, and no release ships it yet: this path has not been compiled or run.
- Ollama's context size can't be set per request through its OpenAI-compatible
  endpoint. Set `OLLAMA_CONTEXT_LENGTH` on the Ollama server if long summaries
  get truncated.

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
