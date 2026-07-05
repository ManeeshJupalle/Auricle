# Phase 6 Part A Report — Summaries, Benchmarks, Ship Preparation

Public ship steps (crates.io publish, GitHub release, demo GIF) are
deferred to a later session by design; everything below prepares them.

## LLM payload capture (payload-first)

Captured 2026-07-05 into `fixtures/llm/` (requests + verbatim responses):

- **Groq** `POST /openai/v1/chat/completions` (llama-3.3-70b-versatile):
  standard OpenAI shape plus extras — `usage` carries Groq-only timing
  fields (`queue_time`, `prompt_time`, `completion_time`), an undocumented
  `x_groq: {id, seed}` extension, `service_tier`, and `usage_breakdown:
  null`.
- **Ollama** (installed on this machine with qwen3:8b / qwen2.5:14b /
  gemma4 already pulled; its CLI auto-starts the daemon): OpenAI-compatible
  endpoint at `/v1/chat/completions`. **Key discrepancy:** for reasoning
  models (qwen3), Ollama returns a non-standard `message.reasoning` field
  carrying the chain of thought *alongside* `message.content`. A strict
  deserializer would reject the response; ours reads `content` only, and a
  fixture test locks in that the reasoning never leaks into summaries.
  `system_fingerprint` is the literal `"fp_ollama"`.
- **Anthropic native variant: skipped** — no `ANTHROPIC_API_KEY` in the
  environment, and the payload-first rule forbids implementing against
  documentation alone. The `LlmProvider` trait is the plug-in point
  (`// PHASE-NEXT` marker in `auricle-llm/src/lib.rs`).

## What was built

- **`auricle-llm`**: `LlmProvider` trait; one OpenAI-compatible client
  covering Ollama/Groq/any base_url (keys via env, memory-only); four
  default templates (`minutes`, `action-items`, `standup`, `1on1`) embedded
  at compile time and overridable/extendable by dropping `.md` files into
  `%LOCALAPPDATA%\auricle\templates` (template names are sanitized — no
  path traversal); map-reduce above ~6k estimated tokens (line-boundary
  chunking, unit-tested for coverage/boundaries/oversized lines; call-count
  verified against wiremock: 3 map + 1 reduce for a ~40k-char transcript).
- **Server**: `POST /api/v1/sessions/:id/summarize` (template + provider
  params, 404/400/502 error split, nothing persisted on failure),
  summaries persisted to the Phase-4 `summaries` table, returned in session
  detail, appended to markdown export as `## Summary — <template> (<model>)`
  sections; `/providers` now also lists LLM providers and templates.
- **UI**: Summarize enabled in session detail with template + provider
  pickers; result cards render inline (template, model, accent border) and
  refresh via detail refetch.
- **Benchmarks**: `benches/latency` — real-time-paced fixture through the
  production pipeline; results and budget misses in `benches/RESULTS.md`
  (deepgram passes the <1 s partial budget at p50 0.14 s; whisper-local
  base.en misses the 2.5 s final budget by ~0.13 s with root causes).
- **Ship prep**: README (thesis, honest limitations, factual comparison,
  benchmark table, `<!-- DEMO GIF -->` placeholder), BUILDING.md (including
  the Windows libclang finding), CONTRIBUTING.md, MIT LICENSE, CHANGELOG,
  CI workflow (windows-latest: UI build → vitest → fmt → clippy → tests,
  with runner-owned CMAKE/LIBCLANG_PATH overriding the repo config).
- **Publish readiness**: `cargo publish --workspace --dry-run` passes for
  all seven crates including verification. This required moving the
  embedded UI inside the server crate (`crates/auricle-server/ui-dist`,
  explicit `include`) — rust-embed cannot package files outside a crate
  root, so published `auricle-server`/`auricle-cli` ship the prebuilt UI
  with no Node required by `cargo install`.

## Self-verification

- Real Groq summarize via curl (captured into API.md) and **via the UI**
  (Playwright: pickers → Summarize → rendered cards), on real transcribed
  sessions; export confirmed to append summaries after the transcript.
- Template faithfulness observed live: `action-items` on a transcript with
  no action items produced exactly the mandated "No action items were
  discussed." — and `minutes` on the same session produced accurate,
  non-invented minutes (screenshot `docs/screenshots/session_summary.png`).
- Screenshot critique: layout/labels good; one nit found — summary bodies
  rendered as plain text, so markdown markers like `**` showed raw.
  **Fixed in Phase 6B** with a minimal dependency-free React markdown
  renderer (headers/lists/checklists/bold/italic/code, no innerHTML so LLM
  output cannot inject markup); screenshot re-taken.

## Remaining for the deferred ship session

1. **Demo GIF**: run `auricle serve`, record (e.g. ScreenToGif) a real
   video-call/podcast session — start in UI, live partials visible, stop,
   summarize, export — save as `docs/demo.gif`, replace the README
   `<!-- DEMO GIF -->` placeholder.
2. **Publish** (order matters; each waits for the index):
   `cargo publish -p auricle-core`, then `-p auricle-capture`,
   `-p auricle-pipeline`, `-p auricle-llm`, `-p auricle-stt`,
   `-p auricle-server` (after a fresh `npm run build`), `-p auricle-cli`.
   Or all at once on cargo ≥1.90: `cargo publish --workspace`.
3. **GitHub release**: tag `v0.1.0`, attach `target\release\auricle.exe`
   (build with fresh `ui/ npm run build` first), paste CHANGELOG section.
4. **Fresh-clone smoke test**: clean machine/checkout, follow README +
   BUILDING.md verbatim to a live transcription session; fix any snag.
5. Repo topics: `rust`, `speech-to-text`, `whisper`, `local-first`,
   `meeting-notes`, `privacy`, `real-time`. LinkedIn post.
