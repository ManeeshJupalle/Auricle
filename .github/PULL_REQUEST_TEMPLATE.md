## What and why

<!-- What this changes, and the problem it solves. Link the issue: "Fixes #123". -->

## How it was checked

<!-- The commands you ran and what they printed. Delete the lines that don't apply. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] `cd ui && npm test` (and `npx tsc --noEmit`)
- [ ] `cd overlay && npm test`, and `cargo test` in `overlay/src-tauri`
- [ ] Tried by hand: <!-- what you clicked or ran -->

## Checklist

- [ ] No real meeting audio, transcripts, or screenshots in the diff, the tests, or this description.
- [ ] Anything that sends user data off the machine is opt-in and writes an egress ledger row.
- [ ] New dependencies are named here, with the reason.
- [ ] User-facing changes are in `CHANGELOG.md` (and `README.md` / `docs/API.md` where they apply).
