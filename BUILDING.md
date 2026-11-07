# Building Auricle (Windows)

## Prerequisites

| Tool | Why | Install |
|---|---|---|
| Rust (pinned in `rust-toolchain.toml`) | everything | rustup.rs; rustup fetches the pinned version on first use |
| Visual Studio Build Tools 2022 (C++ workload) | MSVC linker + whisper.cpp | VS installer |
| CMake | whisper.cpp build | bundled with VS Build Tools, or `winget install Kitware.CMake` |
| **libclang** | bindgen for whisper-rs | `pip install libclang` **or** `winget install LLVM.LLVM` |
| Node.js ^20.19 or >=22.12 | building the web UI (Vite's requirement) | nodejs.org |

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
- `vulkan` (off by default; on `auricle-cli` and `auricle-stt`): whisper
  inference on the GPU, through whisper.cpp's Vulkan backend.

  ```
  cargo build --release -p auricle-cli --features vulkan
  ```

  Build time needs the [Vulkan SDK](https://vulkan.lunarg.com/): `VULKAN_SDK`
  must be set (the SDK installer does this; `whisper-rs-sys` panics without
  it and links `vulkan-1` from `%VULKAN_SDK%\Lib`), and CMake must find the
  SDK's `glslc` to compile the shaders. Run time needs a GPU with a Vulkan
  driver.

  The feature only makes the GPU available. Inference stays on the CPU until
  `auricle.toml` says otherwise; on a build without the feature this setting
  warns and stays on the CPU:

  ```toml
  [stt.whisper-local]
  use_gpu = true
  ```

  **This path has never been compiled or run.** The wiring is in place and
  default builds are unaffected, but expect the first Vulkan build to need
  fixes, and measure it before trusting it in a meeting.

## Overlay (the copilot shell, `overlay/`)

Tauri v2 app in its own cargo workspace; needs the WebView2 runtime
(preinstalled on Windows 11) plus the same Rust/Node toolchain.

```
set GGML_NATIVE=OFF                                      # PowerShell: $env:GGML_NATIVE = 'OFF'
cargo clean --release -p whisper-rs-sys                  # see below
cargo build --release                                    # engine first (the MSI bundles it)
mkdir overlay\src-tauri\binaries                         # gitignored, so absent on a fresh clone
copy target\release\auricle.exe overlay\src-tauri\binaries\auricle-x86_64-pc-windows-msvc.exe
cd overlay && npm ci && npx tauri build                  # MSI in overlay\src-tauri\target\release\bundle\msi\
```

An MSI is for other machines, so its engine must not be built for this one.
ggml has no runtime CPU dispatch and `GGML_NATIVE` defaults on, so without
the variable the engine uses whatever this CPU has and can crash on one that
lacks it. Off selects ggml's portable AVX2 set. Cargo doesn't track the
variable, so a build left in `target\release` would be reused with its old
flags: that is what the `cargo clean` is for. `release.yml` sets the same
variable, then checks that it reached CMake.

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
