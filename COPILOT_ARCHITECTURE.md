# Auricle Copilot — Architecture (v0.3)

**A screen-aware meeting copilot built on the Auricle engine.** Press a global hotkey during a meeting: Auricle captures the active window's text (local OCR), combines it with the recent live transcript, and streams an LLM answer into a small always-on-top overlay. Honest by design: the overlay is visible, capture is on-demand, and everything runs through the same local-first, provider-swappable engine.

This document extends `AURICLE_ARCHITECTURE.md`. Everything there still holds; nothing in the existing pipeline, capture, STT, or server layers changes except documented additions.

---

## 1. Positioning

### What this is
A copilot for **your own** meetings: recall what was said, understand what's on screen, draft answers, look things up — summoned deliberately, dismissed instantly.

### What this is explicitly NOT (design constraints, not just marketing)
- **No concealment.** The overlay is a normal window. No hide-from-screen-capture tricks (`SetWindowDisplayAffinity` / `WDA_EXCLUDEFROMCAPTURE` is out of scope and will not be added). If you share your screen, the overlay is visible — that's the point.
- **No continuous surveillance.** Screen capture happens only on explicit hotkey press. There is no background screenshot loop, no keylogging, no watching.
- **Privacy defaults preserved.** OCR text and questions are processed in memory; nothing screen-derived is persisted unless `copilot.retain_context = true`. Fully-local operation (Windows OCR + Ollama) is the default configuration.

These constraints go in the README verbatim.

---

## 2. What Gets Added

```
                         EXISTING ENGINE (v0.1, unchanged)
     capture → VAD → chunker → STT → assembler ──► broadcast ──► /ws/live
                                                       │
                                              ┌────────┴─────────┐
NEW (v0.3)                                    │  transcript ring │
                                              │  (last N min,    │
┌───────────────┐  hotkey   ┌──────────────┐  │  in-memory)      │
│ Overlay shell │──────────►│ /api/v1/ask  │  └────────┬─────────┘
│ (Tauri,       │           │ (assistant   │           │
│ always-on-top,│◄──────────│  service)    │◄──────────┘
│ streaming UI) │  WS/SSE   └──────┬───────┘
└───────────────┘  tokens          │ on-demand
                                   ▼
                           ┌──────────────┐
                           │ auricle-     │  Windows.Graphics.Capture
                           │ vision       │  → Windows.Media.Ocr
                           │ (screen→text)│  (native, local, fast)
                           └──────────────┘
```

Three new components; the engine is a dependency, not a construction site.

### 2.1 `auricle-vision` (new crate)

- **Capture:** active window (or a chosen monitor) via `Windows.Graphics.Capture` (WinRT, the modern non-deprecated path; the `windows` crate exposes it). Single frame on demand — no streaming session held open.
- **OCR:** `Windows.Media.Ocr` (built into Windows 10/11, no model download, ~100–300 ms typical). Output: lines with bounding boxes, flattened to reading-order text with a simple top-to-bottom, left-to-right sort within line bands.
- **Output type:** `ScreenContext { window_title, app_name, text, captured_at, ocr_ms }`.
- **Fallback/extension point:** trait `ScreenReader` so a vision-LLM path (send the PNG to a multimodal model) can be added later behind config. v0.3 ships OCR only.
- **Exclusions:** the capture excludes Auricle's own overlay window (by window handle, a correctness measure so the copilot doesn't OCR its own previous answer — not a concealment feature).

### 2.2 Assistant service (extends `auricle-server` + `auricle-llm`)

- **Transcript ring:** in-memory buffer of final segments from the existing broadcast channel, capped at `copilot.transcript_window_min` (default 10 min). Zero DB reads on the hot path; works even when nothing is persisted yet.
- **Context assembler:** builds the prompt from, in priority order: (1) user question, (2) `ScreenContext.text` (truncated to `copilot.max_screen_chars`, default 6 000), (3) transcript window (most recent first, truncated to token budget), (4) session metadata (title, speakers). System prompt template `copilot.md` lives with the other templates, user-overridable.
- **Endpoint:** `POST /api/v1/ask` — body `{ question, include_screen: bool, include_transcript: bool, provider?, session_id? }`. Response: streamed tokens.
- **Streaming:** new WS event `answer_delta { ask_id, text }` + `answer_done { ask_id, usage }` on the existing `/ws/live` socket (the overlay already needs that socket for transcript context), plus an SSE variant on the endpoint itself for curl-ability.
- **Provider:** existing `LlmProvider` — Ollama default, Groq/OpenAI-compat selectable per ask. Streaming support added to the LLM client (it's currently request/response; SSE parsing of OpenAI-compatible `stream: true` is the addition).
- **History:** each ask + answer kept in memory per session for follow-up questions (`ask` accepts `follow_up: true` to include prior Q/A); persisted to a new `asks` table only if `copilot.retain_context = true`.

### 2.3 Overlay shell (new top-level dir `overlay/`, Tauri v2)

- **Window:** frameless, transparent background, always-on-top, ~420 px wide, docked to a configurable corner, click-through when idle. Normal window in every OS sense — visible in screen shares, alt-tab, and capture (see §1).
- **Global hotkeys** (Tauri global-shortcut plugin, defaults, all rebindable):
  - `Ctrl+Shift+Space` — summon with question input focused
  - `Ctrl+Shift+A` — "answer what's happening": capture screen + last transcript, no typed question (question defaults to the `quick_assist` template)
  - `Esc` — dismiss
- **UI:** single React view (reuses the engine UI's design tokens/theme): question input, streaming answer with markdown rendering, context chips showing exactly what was included ("🖥 screen: Chrome — Jira board", "🎙 transcript: last 10 min"), copy button, follow-up input. Context chips are the honesty affordance — the user always sees what was captured.
- **Process model:** the overlay is a pure API client. It spawns/attaches to `auricle serve` if not running (same child-process management the v0.2 tray shell plan called for — this IS the desktop shell, shipped with a reason to exist). If the overlay crashes, the engine doesn't blink.
- **Tray icon:** start/stop session, open dashboard (browser to :4820), summon copilot, quit.

---

## 3. API Additions (documented in API.md when built)

- `POST /api/v1/ask` — SSE streaming answer; 409 if no capability (no LLM provider ready)
- `POST /api/v1/peek` — capture + OCR only, returns `ScreenContext` JSON (debug/CLI/testing surface)
- WS events: `answer_delta`, `answer_done`, `ask_error`
- Config section:

```toml
[copilot]
transcript_window_min = 10
max_screen_chars = 6000
retain_context = false
hotkey_summon = "Ctrl+Shift+Space"
hotkey_quick = "Ctrl+Shift+A"
provider = "ollama"            # falls back to [llm].provider
```

New SQLite migration (only used when retain_context=true):
`asks(id, session_id NULLABLE, question, screen_context, answer, provider, created_at)`

---

## 4. Performance Budget (extends the published table)

| Metric | Target | Notes |
|---|---|---|
| Hotkey → screen text ready | < 500 ms | WGC frame + Windows OCR |
| Hotkey → first answer token (Groq) | < 1.5 s | capture + assemble + TTFT |
| Hotkey → first answer token (Ollama, local) | < 4 s | model-dependent; measured & documented honestly |
| Overlay summon → visible | < 150 ms | pre-created hidden window, show on hotkey |
| Idle overlay CPU | ~0 % | no background capture |

---

## 5. Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Windows OCR quality on dense UIs/code | Documented limitation; `peek` CLI makes quality inspectable; vision-LLM extension point for v0.4 |
| WGC permission/HDR/DPI quirks | Phase 7 is capture-first: probe real windows on real hardware before pipeline code, PHASE7 report |
| LLM streaming breaks existing LlmProvider users | Streaming added as a new trait method with a default non-streaming fallback; summarize path untouched |
| Overlay perceived as Cluely-style cheating tool | §1 constraints in README verbatim; context chips; no concealment APIs anywhere in the codebase |
| Tauri v2 global shortcut/transparency quirks on Windows | Phase 9 gate includes explicit multi-monitor + DPI checks |
| Scope creep (voice-asking, auto-suggestions) | Out of scope for v0.3; listed in §6 |

---

## 6. Out of Scope (v0.3)

Continuous/ambient suggestions • voice-activated asks • vision-LLM screen understanding • macOS • answer injection into other apps • any form of capture-hiding.

---

## 7. Ship Definition (v0.3 launch)

Engine v0.1 must already be public. v0.3 ships as: updated repo (new crates + overlay), overlay installer (Tauri bundler MSI), README section with a new demo GIF (hotkey → context chips → streaming answer over a real meeting), updated benchmark table with copilot latency rows, honest limitations. Launch line: **"The engine now has eyes."**
