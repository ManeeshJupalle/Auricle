# Contributing to Auricle

Thanks for considering it. Bug reports, fixes, docs, and benchmarks from
hardware other than ours are all welcome. Auricle runs on Windows only today,
so building and testing the engine needs a Windows machine; the dashboard's
tests and the docs don't.

## Your first contribution

Pick an issue labelled
[good first issue](https://github.com/ManeeshJupalle/Auricle/labels/good%20first%20issue),
say in a comment that you're taking it, and open a draft pull request early.
Questions in the issue are welcome.

### Dashboard only (no Rust toolchain)

The dashboard is a Vite/React app in `ui/`. Its type check and tests need only
Node (^20.19 or >=22.12):

```
cd ui
npm ci
npx tsc --noEmit     # type check
npm test             # vitest
npm run build        # tsc + vite build, into crates/auricle-server/ui-dist
```

To see your change running, `npm run dev` serves the UI on :5173 and proxies
`/api` and `/ws` to an engine on `127.0.0.1:4820`. That engine has to come from
the same commit as the UI, because the API on `main` moves ahead of the last
release: build it as below and run `cargo run -p auricle-cli -- serve` in
another terminal.

### Overlay frontend only

The copilot overlay's web side is in `overlay/` and also needs only Node:

```
cd overlay
npm ci
npm test
```

### The engine

Follow [BUILDING.md](BUILDING.md) for the prerequisites: Rust (the version in
`rust-toolchain.toml` installs itself), Visual Studio Build Tools with the C++
workload, CMake, and libclang, which whisper.cpp needs and which is the usual
first stumbling block. Then, from a short path such as `D:\Auricle`:

```
cd ui && npm ci && npm run build && cd ..   # the UI is embedded at compile time, so it goes first
cargo build
cargo test                                   # fixture-driven: no network, no keys
```

The first build compiles whisper.cpp and takes a while; after that it's quick.
The overlay's Rust side (`overlay/src-tauri`) is its own cargo workspace; see
the Overlay section of BUILDING.md.

### Docs

README.md, docs/API.md, BUILDING.md, and this file are fair game. Every claim
in them should match what the code does today, so if you find one that
doesn't, an issue or a fix is a real contribution.

## Before you push

CI runs these on Windows; run the ones for the parts you touched, from the
repo root in Git Bash:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
(cd ui && npm test)
(cd overlay && npm test)
(cd overlay/src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test)
```

The overlay's Rust checks need `overlay/dist` (`npm run build` in `overlay/`)
and a file at `overlay/src-tauri/binaries/auricle-x86_64-pc-windows-msvc.exe`;
an empty placeholder is enough, which is what CI uses.

## The prime directive: payload-first

Never write ingestion code for an external API or audio device from
documentation alone. Capture real output first (a real device enumeration,
a real WebSocket frame dump, a real JSON response) into `fixtures/`, derive
your types from that, and note any doc-vs-reality discrepancies in the PR.
The `docs/PHASE*_REPORT.md` files show the pattern (and record the
surprises already found: gappy WASAPI loopback, Deepgram's fed-audio clock,
Ollama's `reasoning` field, and friends). Fixtures must not contain real
people's speech or private data; synthetic speech and made-up text are fine.

## Hard rules

- **Real-time audio callbacks are allocation-free and lock-free.** They
  write to rtrb ring buffers, count drops in atomics, and do nothing else.
  No logging, no mutexes, no allocation in the callback path; reviewers
  will check.
- **STT/LLM inference never blocks capture.** Workers own bounded queues;
  when overloaded, shed partials, never transcript.
- **Keys via env vars or Windows Credential Manager only.** Never logged,
  never in config files, never in the database.
- **Partials are never persisted.** Only finals reach SQLite.
- **Nothing leaves the machine by surprise.** Anything that sends user data
  to a cloud service is opt-in, and in the daemon writes an egress ledger row
  first.
- **No real meetings in issues, PRs, tests, or screenshots.** Make one up.

## Practicalities

- Windows is the only supported capture platform right now; keep
  platform-specific code inside `auricle-capture`.
- New dependencies need a stated reason in the PR (the workspace manifest
  documents existing ones).
- Performance claims go through `benches/`: put numbers in
  `benches/RESULTS.md` with your hardware, including the misses.
- User-facing changes go in `CHANGELOG.md`, and in the README or
  `docs/API.md` where they apply.
- Security problems: report them privately, as [SECURITY.md](SECURITY.md)
  describes, not in an issue.
- Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## Scope

The API is the product; UI features beyond the reference client's scope,
meeting-bot functionality, and ML diarization are out of scope for v1. See the
README's Roadmap and Limitations sections for what's planned and what's known.
