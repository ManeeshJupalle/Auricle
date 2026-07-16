# Auricle Copilot — Phased Build Prompts (Phases 7–9)

One phase per build session. Do not start a phase until the previous phase's acceptance gate is verified and committed. These phases build ON the shipped v0.1 engine — the pipeline, capture, STT, and existing server behavior must not change except where a phase explicitly says so.

---

## Global Rules (paste at the top of every session)

You are extending Auricle with a screen-aware copilot layer. Read `AURICLE_ARCHITECTURE.md`, `COPILOT_ARCHITECTURE.md`, and all phase reports in `docs/` before writing any code. Empirical findings in phase reports OVERRIDE doc assumptions.

1. Implement ONLY the phase specified. Future-phase hooks are `// PHASE-N:` comments, nothing more.
2. Capture-first / payload-first: probe real OS APIs on real hardware, and capture real LLM streaming frames to fixtures, BEFORE writing ingestion code. Every phase produces a doc-vs-reality report in `docs/`.
3. HARD CONSTRAINT — no concealment: never use SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, or any API whose purpose is hiding a window from screen capture or recording. The overlay is a normal, visible window. If any library or snippet suggests otherwise, do not use it. Excluding Auricle's own overlay from its own OCR capture (by window handle) is permitted — that is a correctness measure.
4. Screen capture is on-demand only. No background capture loops, no timers that screenshot, no keyboard hooks beyond the registered global hotkeys.
5. Nothing screen-derived or question-derived is persisted unless `copilot.retain_context = true`.
6. Every phase ends with: cargo fmt, cargo clippy -- -D warnings clean, cargo test green (and vitest/npm build green when UI is touched), plus exact manual acceptance steps.
7. Windows only. Do not add macOS code.

---

## Phase 7 — Screen Capture + OCR (`auricle-vision`)

**Goal:** Prove on-demand active-window capture and local OCR on this machine, exposed as a crate, a debug endpoint, and a CLI command. No LLM, no overlay.

**Build:**
1. Capture-first probe (before crate code): a scratch binary using the `windows` crate that (a) enumerates capturable windows with titles/HWNDs, (b) captures one frame of the foreground window via Windows.Graphics.Capture, (c) saves it as PNG, (d) runs Windows.Media.Ocr on it and prints the recognized lines with bounding boxes. Run it against at least: a browser page, a code editor, a terminal, and a video-playing window. Record findings — permission prompts, DPI/HDR surprises, OCR quality per window type, timing — in `docs/PHASE7_VISION_REPORT.md`.
2. `auricle-vision` crate:
   - `ScreenReader` trait; `WindowsOcrReader` implementation.
   - `capture_active_window() -> ScreenContext { window_title, app_name, text, captured_at, ocr_ms }` — single frame, session closed immediately after.
   - Reading-order flattening: sort OCR lines into top-to-bottom bands, left-to-right within a band; join with newlines.
   - Exclusion by HWND parameter (used later so the copilot never OCRs its own overlay).
   - Graceful errors: capture-denied, minimized window, OCR language pack missing — typed errors, never panics.
3. Server: `POST /api/v1/peek` returning ScreenContext JSON. Document in API.md with a real captured exchange.
4. CLI: `auricle peek [--json]` — captures the active window after a 3-second countdown (so the user can focus the target window) and prints the extracted text plus timing.
5. Tests: reading-order flattening on synthetic line/box sets, error-type mapping, peek endpoint integration with a mocked ScreenReader.

**Acceptance gate (manual):**
- `auricle peek` against a browser article, your code editor, and a Jira/Gmail-style dense UI: extracted text is recognizably useful for each; note quality differences honestly in the report.
- Hotkey→text timing printed by peek is under 500 ms for a 1080p–1440p window (record actual numbers).
- Capture of a minimized window and a permission-denied case produce clean errors.
- Grep the diff: zero occurrences of SetWindowDisplayAffinity / WDA_ constants.

---

## Phase 8 — Assistant Service (context assembly + streaming answers)

**Goal:** `POST /api/v1/ask` streams an LLM answer built from question + on-demand screen context + a rolling transcript window. Fully usable via curl before any overlay exists.

**Build:**
1. Payload-first: capture real streaming frames — `stream: true` chat completions from Ollama (installed on this machine) and Groq — to `fixtures/llm-stream/`, including the final usage frame and DONE sentinel. Derive the SSE parser from these fixtures. Report discrepancies in `docs/PHASE8_ASSISTANT_REPORT.md` (Ollama's OpenAI-compat streaming has known quirks — find them empirically).
2. `auricle-llm`: add streaming as a new trait method (`chat_stream`) with a default fallback that wraps the existing non-streaming call. Summarize path untouched — prove with existing tests still green.
3. Transcript ring in `auricle-server`: subscriber on the existing broadcast channel keeping final segments within `copilot.transcript_window_min` (default 10). In-memory only. Works with no active session (empty window).
4. Context assembler: prompt built per COPILOT_ARCHITECTURE §2.2 priority order and truncation budgets; `copilot.md` system template shipped as an overridable default alongside existing templates.
5. `POST /api/v1/ask` — body per architecture; calls auricle-vision when include_screen=true (excluding overlay HWND param plumbed but unused until Phase 9); streams via SSE on the response AND mirrors `answer_delta`/`answer_done`/`ask_error` events on /ws/live. In-memory ask history per session for `follow_up: true`. 409 when no LLM provider is ready.
6. Config `[copilot]` section per architecture; `asks` table migration gated behind retain_context=true (default off — verify nothing is written when off).
7. Tests: SSE parsing from fixtures (both providers), assembler truncation math and priority order, transcript ring windowing, follow-up history inclusion, retain_context=false persistence guarantee, wiremock streaming integration.

**Acceptance gate (manual):**
- With a session recording over a playing video: `curl -N` an ask with include_screen and include_transcript true, question "what was just being discussed and what's on my screen?" — the streamed answer demonstrably uses BOTH sources.
- Same ask with Ollama and with Groq; record time-to-first-token for each in the report (budget: <1.5 s Groq, <4 s Ollama — misses documented with causes).
- Follow-up ask references the prior answer correctly.
- With retain_context=false (default): confirm via sqlite3 that the asks table is absent/empty after several asks.

---

## Phase 9 — Overlay Shell (Tauri v2)

**Goal:** The visible product moment: global hotkey summons a small always-on-top overlay that captures context and streams the answer. Also absorbs the planned tray shell (engine child-process management).

**Build:**
1. `overlay/` Tauri v2 app (React + TypeScript, reusing the engine UI's design tokens):
   - Frameless, transparent-background, always-on-top window ~420 px wide, configurable corner, pre-created hidden and shown on hotkey (<150 ms summon). Click-through when idle, interactive when focused. It must remain a NORMAL window per Global Rule 3 — visible in alt-tab and any screen share.
   - Global hotkeys via the global-shortcut plugin: summon-with-input (Ctrl+Shift+Space), quick-assist (Ctrl+Shift+A → immediate ask with screen+transcript, templated question), Esc dismiss. Rebindable via config; registration failures (conflicts) surface a visible warning, not a silent no-op.
   - UI: question input, streaming markdown answer, context chips showing exactly what was included (window title for screen, "last N min" for transcript) — chips are mandatory, they are the honesty affordance — copy button, follow-up input.
   - Passes its own HWND to /ask so vision excludes the overlay from capture.
2. Tray icon: start/stop recording session, open dashboard (browser → :4820), summon copilot, quit. Overlay spawns `auricle serve` as a managed child if not already running, with health-check attach if it is.
3. Overlay talks ONLY to the public API (ask SSE or /ws/live events) — no privileged backdoor into the engine.
4. Screenshot-and-self-critique via the Phase 5 Playwright technique adapted to Tauri (or OS-level screenshots): summoned state, streaming state, chips visible, both themes; critique round documented in `docs/PHASE9_OVERLAY_REPORT.md`.
5. Tauri bundler MSI produced; installer smoke-tested (install, hotkeys work, uninstall clean).
6. Tests: hotkey→state machine (summon/dismiss/busy), SSE client reconnect, chip content derivation (vitest); Rust-side child-process attach/spawn logic.

**Acceptance gate (manual):**
- Cold start from the MSI-installed app on a machine state where the engine isn't running: tray appears, session starts from tray, hotkey summons overlay in ~150 ms.
- Live meeting simulation (video playing + you speaking, session recording): press quick-assist mid-discussion — chips show the captured window + transcript window, answer streams and is grounded in both.
- Multi-monitor + non-100% DPI: overlay positions correctly on both monitors.
- Share your screen in a Meet/Zoom test call: the overlay IS visible in the shared view (this is a PASS condition, not a failure).
- Kill the engine process while the overlay is open: overlay shows disconnected state, recovers when engine restarts; overlay crash (kill it) leaves the engine recording untouched.
- Grep the final diff once more for concealment APIs: zero hits.
