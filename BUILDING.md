# Building Auricle (Windows)

## Prerequisites

| Tool | Why | Install |
|---|---|---|
| Rust (stable, 1.85+) | everything | rustup.rs |
| Visual Studio Build Tools 2022 (C++ workload) | MSVC linker + whisper.cpp | VS installer |
| CMake | whisper.cpp build | bundled with VS Build Tools, or `winget install Kitware.CMake` |
| **libclang** | bindgen for whisper-rs | `pip install libclang` **or** `winget install LLVM.LLVM` |
| Node.js 20+ | building the web UI | nodejs.org |

### The libclang finding (read this if the build fails on whisper-rs-sys)

`whisper-rs-sys` runs bindgen at build time, and **its bundled fallback
bindings do not compile on Windows** — they were generated on Linux and
carry glibc struct-layout assertions that fail under MSVC (verified; see
`docs/PHASE2_PIPELINE_REPORT.md`). bindgen also *panics* rather than
erroring when libclang is missing, so the crate's documented
`WHISPER_DONT_GENERATE_BINDINGS` escape hatch is unreachable. libclang is
therefore a hard requirement.

`.cargo/config.toml` pins two `[env]` entries for this machine class:

```toml
[env]
CMAKE = '<path to cmake.exe>'          # VS-bundled CMake, not on PATH by default
LIBCLANG_PATH = '<dir containing libclang.dll>'
```

Adjust both to your machine (a `pip install libclang` wheel's `native/` dir
works; so does `C:\Program Files\LLVM\bin`). Cargo's `[env]` does **not**
override variables already set in your environment, so CI or your shell can
always take precedence by exporting them.

Two more empirical Windows notes:

- Build from a **short path** (e.g. `D:\Auricle`). MSBuild's FileTracker
  fails with `FTK1011` when the whisper.cpp build runs under a deep/temp
  path.
- whisper.cpp is compiled `opt-level = 3` even in dev profiles
  (`[profile.dev.package.whisper-rs-sys]` in the workspace manifest) —
  unoptimized whisper is unusably slow for the live pipeline.

## Build

```
cd ui && npm ci && npm run build && cd ..   # web UI → crates/auricle-server/ui-dist
cargo build --release                        # one self-contained binary
.\target\release\auricle.exe --help
```

The UI build must run **before** the release build: rust-embed snapshots
`crates/auricle-server/ui-dist` at compile time. After that, the exe runs
anywhere with no Node.

## Develop

- `cargo test` — full workspace suite (fixture-driven; no network, no keys).
- `cd ui && npm test` — vitest for the client store/reconnect logic.
- UI iteration without Rust rebuilds: `auricle serve` in one terminal,
  `cd ui && npm run dev` in another (Vite proxies `/api` and `/ws` to
  :4820).
- Whisper end-to-end test (needs the base.en model, ~10 s CPU):
  `cargo test -p auricle-stt --test whisper_e2e -- --ignored`.
- Latency benchmarks: `cargo run --release -p auricle-latency-bench --
  --stt whisper-local --model base.en` (see `benches/RESULTS.md`).

## Feature flags

- `auricle-stt/cloud` (default on): deepgram + groq-whisper +
  openai-compat providers. Disable for a local-only build.
- Vulkan GPU inference for whisper: roadmap (whisper-rs `vulkan` feature is
  wired upstream; untested here).

## Overlay (the copilot shell, `overlay/`)

Tauri v2 app in its own cargo workspace; needs the WebView2 runtime
(preinstalled on Windows 11) plus the same Rust/Node toolchain.

```
cargo build --release                                    # engine first (the MSI bundles it)
copy target\release\auricle.exe overlay\src-tauri\binaries\auricle-x86_64-pc-windows-msvc.exe
cd overlay && npm ci && npx tauri build                  # MSI in overlay\src-tauri\target\release\bundle\msi\
```

Empirical warning: a plain `cargo build --release` in `overlay/src-tauri`
does **not** produce a production overlay — without the `custom-protocol`
feature the binary still points at the dev server URL and renders a blank
(or, worse, someone else's) page. Use `npx tauri build`, or pass
`--features custom-protocol` explicitly.

Dev iteration: `npx tauri dev` (vite on :1420 with hot reload). Tests:
`npm test` (vitest) and `cargo test` in `overlay/src-tauri`.

### MSI install scope

The MSI **installs per-user by default** (no elevation,
`%LOCALAPPDATA%\Programs\Auricle Copilot`) via a dual-purpose WiX package
(`overlay/src-tauri/wix/main.wxs` — tauri-bundler's stock template with
`ALLUSERS=2` + `MSIINSTALLPERUSER=1` baked in; that property block is the
only local change). For a machine-wide install (requires elevation):

```
msiexec /i "Auricle Copilot_x.y.z_x64_en-US.msi" ALLUSERS=1 MSIINSTALLPERUSER=""
```
