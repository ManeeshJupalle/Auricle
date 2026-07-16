# Phase 9 Overlay Report — Tauri v2 Shell

The visible product moment: a global hotkey summons a frameless,
always-on-top glass card that captures context through the public API
and streams the answer. Everything below was verified live on the dev
machine on 2026-07-15. Round 1 was built and verified, then paused for
the maintainer's look review; the review came back "blessed with
changes" and the round-2 section at the end records every change and
its verification. Screenshots for both rounds are in
`docs/screenshots/overlay/` (`r1_*` / `r2_*`).

**Environment:** Windows 11 Home 10.0.26200, now at **125 % display
scaling** (Phase 7 recorded 100 % — the machine changed underneath us,
which found real bugs in the verification tooling, below) · Node
v24.11.1 · Tauri 2.11.5 / tauri-plugin-global-shortcut 2.3.2 /
WebView2 150 · overlay exe 11.8 MB.

---

## What was built

`overlay/` — Tauri v2 + React/TS, its own cargo workspace (**zero path
dependencies on engine crates**; the overlay speaks only the public
HTTP API). Engine UI design tokens (glass gradients, red accent, Geist
fonts via @fontsource) reused verbatim; both themes carried over.

- **Window:** 420 px logical, frameless, transparent, always-on-top,
  pre-created hidden, docked to a configurable corner (default
  bottom-right, 12 px inside the monitor **work area**). A normal
  window in every OS sense: taskbar button, alt-tab entry, zero
  concealment APIs (diff grepped — the only match was a doc comment,
  reworded so the gate greps clean).
- **The window is resized to the rendered card** (ResizeObserver →
  `resize_overlay`): a fixed-size transparent window would silently
  swallow clicks in its invisible regions. Click-through when idle
  (blur → `set_ignore_cursor_events(true)`), interactive again on
  summon or alt-tab focus.
- **Hotkeys** via the global-shortcut plugin only: summon
  (Ctrl+Shift+Space) and quick-assist (Ctrl+Shift+A), rebindable in
  `%LOCALAPPDATA%\auricle\overlay.toml`; parse/registration failures
  surface as a visible warning banner in the overlay. **Esc is
  deliberately window-level, not global** — a global Esc would swallow
  Esc from every app on the system; dismiss-by-Esc therefore works
  when the overlay has focus, which is the only time Esc-to-dismiss
  makes sense.
- **Ask flow:** question input (Enter to send) or quick-assist's
  templated question; SSE events stream into a pure reducer
  (unit-tested state machine); mandatory context chips derive from the
  engine's `ask_started` echo — never from what the overlay merely
  requested; streaming markdown answer (the engine UI's no-innerHTML
  renderer, copied); Copy button; follow-up input carries
  `follow_up: true` once history exists.
- **HWND exclusion:** the Rust side passes the overlay's own HWND as
  `exclude_hwnd` on every ask (the only engine-facing plumbing, and it
  already existed since Phase 8).
- **Tray:** start/stop recording (public REST), open dashboard,
  summon, quit. Action failures summon the overlay with a toast.
- **Engine child management:** health-probe attach, else spawn
  `auricle serve` (env override → exe-adjacent sidecar → PATH;
  resolution order unit-tested). The child is deliberately not
  lifetime-tied to the overlay (no job object): quit and crashes leave
  a recording engine untouched.

## Architecture finding: the webview cannot call the engine

The engine's Phase-6 same-origin hardening rejects browser requests
whose Origin isn't same-origin/loopback. The Tauri webview's origin is
`http://tauri.localhost` — **correctly rejected (403)**. So all engine
traffic (health, ask SSE, tray REST) goes through the overlay's Rust
side with reqwest, and SSE events reach the webview over a Tauri
channel. The engine's security model held against its own first-party
client; no engine change was needed or made.

## Doc-vs-reality findings

1. **A plain `cargo build --release` of a Tauri v2 app is not a
   production build.** Without the `custom-protocol` feature the binary
   still points at `devUrl` — it rendered a transparent void (nothing
   listening on :1420), and later, something worse: **another dev
   server happened to be running on this machine's port 1420, and the
   overlay silently rendered that app's UI** (its `invoke` calls failed
   against our backend with "Command … not found"). `tauri build`
   enables the feature automatically; the feature flag is now explicit
   in Cargo.toml with a warning comment.
2. **125 % DPI broke the verification tooling, not the overlay.**
   DPI-unaware PowerShell gets virtualized window rects
   (420 px "wide" for a 525-physical-px window), so screenshots clipped
   the right 20 % and screen-region captures landed on the wrong
   desktop area entirely. The overlay itself positioned and rendered
   correctly at 125 % throughout. `PrintWindow` (even with
   `PW_RENDERFULLCONTENT`) renders a transparent WebView2 window as
   black from a DPI-aware process; the working recipe is DPI-aware
   `CopyFromScreen` of the physical rect while the overlay is topmost.
3. **The taskbar button can minimize the overlay** (it is a normal
   window — that's the point), and a minimized always-on-top window is
   simply gone from view; summon now calls `unminimize()` first.
4. **Windows Terminal focus follows my scripts, not the hotkey
   press**: repeated foreground churn from automation made "which
   window is active" racy on a live desktop. On real (human) usage the
   foreground window at hotkey time is the meeting/content window,
   which is exactly what the capture grabs — verified by the chips.
5. Tauri v2's `Monitor::work_area()` and `window.hwnd()` both exist
   and behave as documented; positioning math needed no Win32 calls.

## Live verification results

| Check | Result |
|---|---|
| Cold start (no engine): overlay spawns `auricle serve` | healthy in **2.8 s** (dev exe) / **2.8 s** (MSI-installed sidecar at `%LOCALAPPDATA%\Programs\Auricle Copilot\auricle.exe`) |
| Summon hotkey → window visible | **91–122 ms** across runs (budget < 150 ms) |
| Focus | foreground = overlay after every summon (RegisterHotKey grants activation; Phase 7 finding 14 holds) |
| Esc dismiss | visible=False within 300 ms |
| Quick-assist during a real session (Deepgram, live TTS) | answer grounded in BOTH sources; chips showed the true captured window + "transcript: last 10 min · N segments" |
| HWND exclusion | overlay is foreground at capture time, yet chips/answers always name the window *behind* it (the browser/editor in use) — never "Auricle Copilot"; the overlay never OCR'd its own answer |
| Engine killed under the overlay | amber "engine offline" badge (health poll backoff 1-2-4-8-10 s); badge clears after the engine returns; mid-stream ask ends with the truncated answer preserved |
| Overlay killed mid-recording | engine state `recording` before and after; a sentence spoken *after* the kill was transcribed and persisted |
| retain_context=false | after all overlay asks, the engine DB still has no `asks` table |
| MSI | 17 MB, installs per-user with `MSIINSTALLPERUSER=1 ALLUSERS=2` (default scope is per-machine → UAC); hotkey works from the installed app; uninstall removes the install dir clean |
| Gates | overlay: fmt ✓ clippy -D warnings ✓ cargo test 11/11 ✓ vitest 28/28 ✓ · engine workspace: fmt ✓ tests 25 suites all green (untouched) |

## Round-1 screenshot critique (fixes NOT yet applied — awaiting the maintainer's look review)

Screenshots (all real states, captured on the live desktop):
`r1_summoned_dark.png`, `r1_answered_dark.png`, `r1_streaming_dark.png`,
`r1_engine_offline_dark.png`, `r1_engine_recovered_dark.png`,
`r1_summoned_light.png`, `r1_answered_light.png`.

| # | Finding | Proposed round-2 fix |
|---|---|---|
| 1 | **Copy button disappears after dismiss → re-summon.** The actions row keys on phase `answered`; re-summon lands in `ready` with the answer still displayed, so Copy (and the token count) vanish while the text sits right there. | show actions whenever an answer is displayed and no ask is busy |
| 2 | **Between `ask_started` and the first token the box is a bare caret.** Fine for Groq (sub-second); for a local model this is many seconds of unexplained blinking. | keep the caret, add a dim "waiting for the model…" line while `answer == ''` |
| 3 | **Chip titles truncate at 34 chars**, which amputates browser titles ("…- Jira - G…"). | raise to ~48 chars; the chip row already wraps |
| 4 | **Esc hint appears only in the follow-up placeholder**, not the first-ask placeholder. | mention Esc in both |
| 5 | **The 4 px transparent margin mostly clips the card's drop shadow**, so the card edge reads slightly flat against busy backgrounds. | either accept (the border carries the edge) or widen the margin to 8–10 px — wider transparent borders are dead-click zones while focused, so this is a taste call |
| 6 | Light theme: correct and clean; the red focus ring reads a touch louder than in dark. | possibly soften `--accent-soft` in light; taste call |

Deliberately kept: the streaming caret's pulse (matches the engine's
REC treatment), chips styled as the engine's metadata pills, the
compact 107-px ready state (summon should feel weightless), and the
window growing downward-anchored from its corner.

One honest gap: no pretty mid-stream screenshot. Groq streams a full
answer in ~1–2 s, faster than the screenshot tooling's process spawn;
the only mid-stream captures landed in the DPI-clipped round (text
visibly growing, right edge cut). The streaming path itself is
exercised by those captures, the reducer tests, and the caret is
visible in `r1_pending/streaming` variants from the slow-model round.

## Deviations / notes

- Hotkey config lives in the overlay's own
  `%LOCALAPPDATA%\auricle\overlay.toml` (defaults match the
  architecture). The engine reads no config file by default, so there
  was no shared `auricle.toml` to piggyback on; the `[copilot]`
  hotkey keys in engine config remain honored if a user passes
  `--config` to their own daemon — the overlay simply doesn't parse
  the engine's file it can't locate.
- Quit leaves the engine running by design (a recording must survive
  the shell; stopping the engine is a tray/dashboard decision).

---

# Round 2 — maintainer review fixes (all applied and re-verified)

The look review blessed the direction with seven changes. Before →
after, each verified in the `r2_*` screenshots:

| # | Review item | What changed | Verified in |
|---|---|---|---|
| 1 | Card read "recording/alert" at rest: red focus ring in both themes, red header dot always | New `--focus` token (calm blue derived from the transcript's "You" tone) drives the input focus ring in both themes. The header dot is neutral gray at rest and turns red ONLY while `/health` reports `state: recording` — it is now an honest recording indicator, polled on the existing health cadence | `r2_summoned_dark` (neutral dot, blue ring) vs `r2_thinking_dark`/`r2_answered_dark` (red dot while the session records) vs `r2_engine_offline_dark` (neutral again after the session ended) |
| 1a | Copy vanished after dismiss → re-summon | Actions row keys on "an answer is displayed and nothing is busy", not on phase `answered` | `r2_engine_offline_dark` — that card is a re-summoned `ready` state and still shows Copy + provider·time |
| 1b | Bare caret during the first-token wait | Distinct thinking state: caret + "`{provider}` is thinking…" once `ask_started` lands and until the first token | `r2_thinking_dark` ("ollama is thinking…") |
| 1c | Chip titles amputated at 34 chars | Budget raised to 48; typical browser titles now survive whole (locked by a vitest case) | `r2_*` chips |
| 1d | Esc hint only on the follow-up placeholder | Both placeholders read "(Enter · Esc)" | `r2_summoned_*` |
| 1e | Drop shadow clipped by the 4 px margin | Margin widened to 10 px with a compact shadow sized to fit it (window resize accounts for +20) | all `r2_*` — the card visibly floats |
| 2 | Desktop content ghosted through the card | All card surfaces are fully opaque (`rgb()` gradients, no alpha); transparency exists only in the shadow gutter outside the card | the `r1_*` cards are translucent (the page behind tints the surface) vs the opaque `r2_*` cards; any strip of desktop at a shot's edge is the outside gutter, by design |
| 3 | Answer panel ≈ card background in dark | Answer panel lightened (rgb 30/34/43 → 24/27/35 gradient) + `--edge-strong` border | `r2_answered_dark` |
| 4 | Light theme: dark chrome around a light card | Opaque light surfaces + lighter/softer light-theme shadow + slightly stronger light edge; what remains dark at a screenshot's rim is the see-through gutter showing the desktop, not chrome | `r2_summoned_light`, `r2_answered_light` |
| 5 | Raw token count | Actions row shows "`{provider} · {elapsed}s`" (submit → answer_done, timed through event payloads so the reducer stays pure); token count moved to the row's tooltip | `r2_answered_dark` / `r2_answered_light` ("groq · 1.2s") vs the `r1_*` raw token counts |
| 6 | No distinct quick-assist state | "⚡ Quick assist — what's happening right now?" marker renders above the chips for hotkey-initiated asks (cleared by the next typed ask); thinking state covered under 1b | `r2_thinking_dark`, `r2_answered_dark`, `r2_answered_light` |
| 7 | MSI required elevation by default | Custom WiX template (tauri-bundler 2.x stock template + `ALLUSERS=2`/`MSIINSTALLPERUSER=1` in the Property table — the canonical dual-purpose pattern; only that block changed). Plain `msiexec /i <msi> /quiet` now installs per-user with **no elevation** (verified: exit 0 into `%LOCALAPPDATA%\Programs`, uninstall clean); per-machine remains available via `ALLUSERS=1 MSIINSTALLPERUSER=""`, documented in BUILDING.md |

Round-2 gates: vitest 29/29 (one new chip-truncation case; state/ask
tests extended for provider, elapsed timing, and the quick flag) ·
overlay cargo test 11/11, fmt + clippy `-D warnings` clean · engine
workspace clippy clean and untouched · concealment grep over the
overlay sources and the WiX template: zero hits · MSI rebuilt with the
per-user default and smoke-tested (install → per-user dir → uninstall
clean).
