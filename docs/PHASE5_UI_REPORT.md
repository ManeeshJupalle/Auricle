# Phase 5 UI Report

The reference web client: Vite + React + TypeScript + zustand +
@tanstack/react-virtual, embedded into the release binary via rust-embed.
All verification below ran against the real daemon (single binary from a
clean directory) driven by Playwright with real Deepgram transcription of
system TTS audio.

**Environment:** Node v24.11.1, npm 11.17.0, Chromium 149 (Playwright),
release binary 31.3 MB (UI adds ~75 KB gzipped).

---

## Architecture notes

- **Normalized store (zustand).** The transcript is `order: string[]` +
  `rows: Record<id, Row>`; each visible row is a memoized component
  subscribed to exactly `rows[id]`. A partial event mutates one entry of
  `rows` — `order` is untouched — so exactly one row re-renders. A final
  arriving while its channel has a live partial reuses that row's id: the
  row flips styling in place (dim italic + caret → bright) without any list
  churn.
- **Two-lane tolerance.** Finals carry deterministic ids
  (`f-{channel}-{t_start_ms}`), so a final whose partials were all shed
  (lossy lane) simply appends, and a replayed final (resync race) upserts
  instead of duplicating. `lag` events surface as a "~N partials shed"
  badge; the transcript itself is unaffected.
- **Reconnect = resync.** The WS client reconnects with 1→2→4→8→10 s
  backoff (unit-tested). On every open it re-reads `/health`, and if a
  session is active it rebuilds the transcript from the persisted finals in
  `/sessions/:id` — so a killed-and-restarted daemon recovers in the open
  page without a refresh.
- **Server additions for the UI** (documented in API.md): `vu` events
  (~10 Hz per-channel RMS, lossy lane, computed in the capture drain
  threads), `latency_ms` on `final` events, rust-embed static serving with
  SPA fallback, and a settings overlay so the Settings view's choices
  (provider, model, devices, retention) actually govern subsequent
  sessions.
- **Virtualization.** `@tanstack/react-virtual` with `measureElement` for
  wrap-height rows; the list follows the live tail unless the user scrolls
  up (>80 px from bottom disengages).

## Screenshot–critique loop

Round 1 screenshots were taken mid-session (real Deepgram partials on
screen), then critiqued. Findings and fixes:

| # | Finding (round 1) | Fix (round 2, verified) |
|---|---|---|
| 1 | Settings column hugged the left edge; the right half of a 1440 px viewport was dead space | column centered (`margin: 0 auto`) |
| 2 | Idle VU tracks were nearly invisible against the panel — an empty meter read as a broken element, and the muted-mic "You" meter looked like a rendering bug | track lightened (#232a3a) + 1 px border so the housing is visible at zero level |
| 3 | The latency placeholder rendered as `— latency`, reading like a stray glyph | label-first: `latency —` / `latency 1.0s` |
| 4 | VU meters crowded the Stop button | +16 px separation |

Kept as-is deliberately: the mostly-empty transcript panel early in a live
session (it is the workspace and fills over time; the empty state before a
session explains what will happen), and the plain sessions list (four
sessions fill it acceptably; snippets are out of scope).

`docs/screenshots/` (all real states, not mockups):
- `live_partial.png` — dim italic partial with caret, mid-recognition
- `live_recording.png` — finals + growing partial, VU meters live, REC
  badge, `latency 1.0s` readout (real Deepgram numbers)
- `sessions_list.png`, `sessions_after.png` — populated list incl. an
  `interrupted` tag from the Phase 4 crash test
- `session_detail.png` — stored transcript, disabled Summarize (PHASE-6),
  Export .md
- `settings.png` — devices, provider/model, key presence dots, retention
- `reconnecting.png` — daemon killed under the open page: amber
  `reconnecting` badge, transcript retained

## Self-verification results

| Check | Result |
|---|---|
| Single binary | exe copied alone to a clean dir; UI served from embedded assets; no Node at runtime |
| Browser e2e | start → live partials/finals (deepgram) → stop → session in list → detail loads — all via Playwright clicks |
| Reconnect | daemon `taskkill`ed under the open page → `reconnecting` badge; restarted → `connected` without refresh; data loads |
| vitest | 15/15 (store partial→final transitions, live-vs-session state, resync dedup, reconnect backoff) |
| cargo | fmt clean, clippy `-D warnings` clean, all tests green |

## Build order

`ui/ npm run build` → `cargo build --release` (rust-embed snapshots
`ui/dist` at compile time). Dev iteration: `auricle serve` + `npm run dev`
(proxy config in `ui/vite.config.ts`).

Note: `vite.config.ts` pins an empty inline PostCSS config because a stray
`D:\postcss.config.js` outside the repo was being picked up by PostCSS's
upward config search on this machine.
