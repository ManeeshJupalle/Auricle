# Contributing to Auricle

Thanks for considering it. Ground rules, in the spirit the codebase was
built:

## The prime directive: payload-first

Never write ingestion code for an external API or audio device from
documentation alone. Capture real output first — a real device enumeration,
a real WebSocket frame dump, a real JSON response — into `fixtures/`, derive
your types from that, and note any doc-vs-reality discrepancies in the PR.
The `docs/PHASE*_REPORT.md` files show the pattern (and record the
surprises already found: gappy WASAPI loopback, Deepgram's fed-audio clock,
Ollama's `reasoning` field, and friends).

## Hard rules

- **Real-time audio callbacks are allocation-free and lock-free.** They
  write to rtrb ring buffers, count drops in atomics, and do nothing else.
  No logging, no mutexes, no allocation in the callback path — reviewers
  will check.
- **STT/LLM inference never blocks capture.** Workers own bounded queues;
  when overloaded, shed partials, never transcript.
- **Keys via env vars only.** Never persisted, never logged, never in
  config files.
- **Partials are never persisted.** Only finals reach SQLite.

## Practicalities

- Before pushing: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, and `cd ui && npm test` — all clean. CI enforces the same.
- Windows is the only supported capture platform right now; keep
  platform-specific code inside `auricle-capture`.
- New dependencies need a stated reason in the PR (the workspace manifest
  documents existing ones).
- Performance claims go through `benches/` — put numbers in
  `benches/RESULTS.md` with your hardware, including the misses.
- Build setup (incl. the Windows libclang requirement): see BUILDING.md.

## Scope

The API is the product; UI features beyond the reference client's scope,
meeting-bot functionality, and ML diarization are out of scope for v1
(roadmap items live in the README's Limitations section).
