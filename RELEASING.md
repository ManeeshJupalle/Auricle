# Releasing Auricle Copilot

The overlay ships with an auto-updater. This is how a release is cut and how
the two kinds of signing fit together.

## In-app updates

The overlay checks GitHub for a newer signed release:

- **Silently on launch** — if one is waiting, it shows a toast ("Update
  vX is available — Tray → Check for updates to install"). It never
  installs on its own.
- **On demand** — the tray's **Check for updates** item downloads, verifies,
  installs, and relaunches.

Config lives in `overlay/src-tauri/tauri.conf.json` under `plugins.updater`:
the `endpoints` point at `releases/latest/download/latest.json`, and `pubkey`
is the updater public key. Every update payload is verified against that key
before it runs, so a tampered artifact is rejected.

## Two signatures, don't conflate them

1. **Updater signature** — proves an update payload is the one we published.
   Uses a Tauri/minisign keypair; the public half is committed in
   `tauri.conf.json`, the private half is a CI secret. Already set up.
2. **Authenticode code signing** — makes Windows/SmartScreen trust the
   installer. Needs a certificate. **This is the one remaining step** (see
   below). Until it's in place, installs show the usual "unknown publisher"
   SmartScreen prompt.

## One-time setup

**Updater keypair** (done for v0.4.0+ — regenerate only if the key is lost):

```
cd overlay && npm run tauri signer generate -- -w updater.key --ci
```

(`--ci` = no password; passing `--password ""` through npm/PowerShell fails.)
Base64 of nothing extra: the generated `updater.key.pub` file content goes
into `tauri.conf.json → plugins.updater.pubkey` verbatim (it is already
base64). Keep `updater.key` OUT of the repo (gitignored) and back it up —
GitHub secrets cannot be read back, so that file is the only copy.

Then add two repository secrets (Settings → Secrets → Actions):

- `TAURI_SIGNING_PRIVATE_KEY` — contents of `updater.key`
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — the password chosen above (unset is fine for a `--ci` key)

Upload the key with a raw byte redirect (`gh secret set TAURI_SIGNING_PRIVATE_KEY < updater.key`
from bash/Git Bash) — piping through PowerShell 5.1 prepends a BOM and CI
fails with "Invalid symbol 239, offset 0".

Keep the private key out of git; if it is ever lost, updates for existing
installs break (they only trust the matching public key).

**Code signing (the remaining step).** Recommended: **Azure Trusted
Signing** — subscription-priced, no hardware token, builds SmartScreen
reputation over time. Once the account and certificate profile exist:

1. Add `bundle.windows.signCommand` in `tauri.conf.json` (or wire
   `TAURI_WINDOWS_SIGN_COMMAND`) to call the Trusted Signing client against
   the built artifacts.
2. Add the account's credentials as repo secrets and uncomment the signing
   env in `.github/workflows/release.yml`.

An EV certificate on a hardware token is the alternative — instant
zero-warning trust, higher cost, and token handling in CI.

## Cutting a release

1. Bump the version in all of these, update `CHANGELOG.md`, and fold
   user-facing changes into `README.md`:

   - `Cargo.toml` (`[workspace.package]` and the `auricle-*` entries in
     `[workspace.dependencies]`)
   - `overlay/src-tauri/Cargo.toml`
   - `overlay/src-tauri/tauri.conf.json`
   - `ui/package.json` and `overlay/package.json`

   Then refresh the lockfiles: `cargo build` at the root and in
   `overlay/src-tauri`, and `npm install --package-lock-only` in `ui/` and
   `overlay/`.
2. Tag and push:

   ```
   git tag v0.4.0 && git push origin v0.4.0
   ```

3. `.github/workflows/release.yml` waits for approval on the `release`
   environment, then runs the release gate below. It builds the dashboard, the
   engine sidecar, and the overlay; signs the updater artifacts; generates
   `latest.json`; and opens a **draft** release with the MSI + `latest.json`
   attached.
4. Smoke-test the MSI, then **publish** the draft. Publishing is what makes it
   the "latest" release the updater endpoint resolves to — existing installs
   pick it up on their next check.

To re-run a release by hand, start the workflow from the tag (Actions → Release
→ Run workflow → "Use workflow from" → **Tags**). From a branch it fails.

## The release gate

The release job stops before anything is signed if any of these fail:

- **Tag and versions.** The run must be from a tag, and the tag must equal
  `v` + the version in all five files listed above.
- **Tests.** Both vitest suites and `cargo test`.
- **ggml flags.** The job sets `GGML_NATIVE=OFF`, then reads whisper-rs-sys's
  `CMakeCache.txt` and requires `GGML_NATIVE` and `GGML_AVX512` to be `OFF`.
  ggml has no runtime CPU dispatch, so a native build bakes in whatever the
  runner's CPU supports: the v0.4.1 draft came out with AVX-512 code that
  crashes on CPUs without it. Off selects ggml's portable AVX2 set. The job
  deliberately has no build cache, because whisper-rs-sys's build script doesn't
  rerun when `GGML_*` changes, so cached objects could outlive a flag change.
- **Smoke test.** `auricle.exe --version` has to run.

## The `release` environment

The release job runs in a GitHub environment named `release`, because it is what
touches the updater signing key. The workflow can only name the environment;
protecting it is a repo setting, and until that is done the job runs unattended.
GitHub creates the environment on the first run if it doesn't exist.

Set this once, under Settings → Environments → `release`:

- **Required reviewers** — add whoever may approve a release.
- **Deployment branches and tags** — restrict to the tag pattern `v*`.

The two `TAURI_SIGNING_*` secrets can stay repository secrets. Moving them into
this environment is stronger: only jobs that passed the reviewers can read them.

## Publishing to crates.io

Manual (CI does not do this). Needs a token with the `publish-update`
scope (`cargo login`), and a fresh `ui/ && npm run build` first so the
server crate packages the current dashboard. Publish in dependency order —
`cargo publish` waits for the index between crates:

```
for c in auricle-core auricle-capture auricle-llm auricle-vision \
         auricle-pipeline auricle-stt; do cargo publish -p $c || break; done
cargo publish -p auricle-server --allow-dirty   # ui-dist is gitignored by design
cargo publish -p auricle-cli
```
