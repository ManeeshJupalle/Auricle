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

**Updater keypair** (done — regenerate only if the key is lost):

```
cd overlay && npm run tauri signer generate -- -w updater.key
```

Then add two repository secrets (Settings → Secrets → Actions):

- `TAURI_SIGNING_PRIVATE_KEY` — contents of `updater.key`
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — the password chosen above (blank is allowed)

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

1. Bump the version across the workspace and `overlay/src-tauri/tauri.conf.json`,
   update `CHANGELOG.md`, and fold user-facing changes into `README.md`.
2. Tag and push:

   ```
   git tag v0.4.0 && git push origin v0.4.0
   ```

3. `.github/workflows/release.yml` builds the dashboard, the engine sidecar,
   and the overlay; signs the updater artifacts; generates `latest.json`; and
   opens a **draft** release with the MSI + `latest.json` attached.
4. Smoke-test the MSI, then **publish** the draft. Publishing is what makes it
   the "latest" release the updater endpoint resolves to — existing installs
   pick it up on their next check.
