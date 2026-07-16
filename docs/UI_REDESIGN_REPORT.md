# UI Redesign Report

The web client evolved from the Phase 5 utilitarian tab layout into a
meeting-notes application layout: persistent sidebar navigation plus a
document-style reading pane. Pipeline/capture/stt/llm crates untouched;
all server changes live in `auricle-server`.

## Layout

- **Sidebar (collapsible):** wordmark, search over titles *and* transcript
  content (`GET /sessions?q=`, SQLite LIKE with escaped wildcards),
  session list (title, date, duration; active highlighted; hover
  rename/delete affordances), pinned **Start/Stop Recording** with a
  provider quick-pick, Settings/About below.
- **Main pane:** header with inline-editable title, date/duration, export;
  "Transcript" | "AI Summary" tabs (summary tab hosts the template +
  provider pickers and rendered cards); live sessions render in the same
  layout with REC badge, elapsed ticker, VU meters, and latency in the
  header, partials keeping their dim/italic treatment.
- **Transcript document:** timestamp gutter (subtle mono `mm:ss`,
  clickable), consecutive same-channel segments merged into speaker-turn
  paragraphs, 70ch measure, 15px/1.75 reading face; the virtualizer now
  virtualizes turns.

## New API surface (documented in API.md)

`GET /sessions?q=` search · `PATCH /sessions/:id` rename ·
`DELETE /sessions/:id` (409 while recording; removes retained audio) ·
`GET /sessions/:id/audio/:channel` WAV serving · `session_updated` WS
event.

## Audio playback

Sessions recorded with `retain_raw_audio` get a floating playback bar:
play/pause, scrubber, current/total time. The mic and loopback WAVs share
one session timeline, so both play in sync — that is the meeting audio,
both sides at once. Clicking a turn's timestamp seeks (and starts)
playback. Sessions without retained audio show no bar.
Re-transcription from these WAVs is deliberately not built
(`// FUTURE` in api.rs).

## Auto-titles (user-requested during review)

The timestamp-wall sidebar problem is solved at the source: sessions
started without a title are auto-titled after stop — 3–6 words from the
transcript via the configured LLM (new `llm_provider` setting; `ollama`
default) with a first-words-of-transcript offline fallback, announced via
`session_updated`. User renames permanently opt a session out
(`meta.auto_title` cleared). Verified live: Groq produced
"Engineering Sync Meeting Notes" for a real session seconds after stop;
the fallback path produced "To the weekly engineering sync" with the LLM
unreachable. Full wiremock integration test covers the flow.

## The re-render property survived grouping

Partials still re-render exactly one row: turn grouping lives in a second
normalized layer (`turns`/`turnOrder`) that keeps object identity when a
partial's text grows — only `rows[id]` changes, and each segment span is
individually subscribed. A vitest asserts `turnOrder`/`turns` are
**reference-identical** across partial updates and partial→final flips.

## Themes

Dark stays default. Light theme via CSS-variable overrides
(`[data-theme='light']`), toggled in Settings, persisted to the settings
store + localStorage (instant boot paint). Monospace remains for
timestamps/metadata; transcript body uses the system UI reading stack.
The record button is the single loud element (red); everything else keeps
the one restrained accent.

## Round-1 critique (screenshots in docs/screenshots/redesign/)

| # | Finding | Outcome |
|---|---|---|
| 1 | Sidebar was a wall of identical "Session {timestamp}" titles | superseded by auto-titles (the root fix) |
| 2 | Title/meta date redundancy in list items | same — summary titles make the meta line informative |
| 3 | Light-theme hover states nearly invisible | fixed: light `--panel-2` contrast raised |
| 4 | Text column sat slightly left of center (gutter offset) | fixed: gutter narrowed, balancing padding added |
| 5 | Meter/baseline alignment, summary card width | re-inspected: non-issues, left alone |

Round-2 screenshot loop was waived by the maintainer; fixes were verified
by inspection and the standard gates (fmt, clippy `-D warnings`,
cargo test incl. new store/endpoint/autotitle tests, vitest 26/26,
`npm run build` embedded into the release exe).

## Visual identity pass (design mock import)

The maintainer supplied a design mock (`Auricle.dc.html`) which was
implemented as the final visual identity: near-black radial-washed canvas,
glassy gradient sidebar with an active-rail session list, red accent
(#ef4444/#c0362f) with glow, Geist / Geist Mono typography, pill-style
connection status, chip metadata, iconized controls, and a speaker-column
transcript layout (speaker + timestamp stacked left, prose right — the
timestamp remains the click-to-seek control). Structure and behavior were
untouched: store, virtualizer, single-row partial re-renders, audio bar,
live view, and the light theme (derived with adapted tones — the mock is
dark-only) all carry over. One deliberate deviation: the mock loads Geist
from Google Fonts; the implementation bundles the fonts via @fontsource so
the single binary stays fully offline. Final screenshots:
`docs/screenshots/redesign/dc_transcript_dark.png`,
`dc_summary_dark.png`.
