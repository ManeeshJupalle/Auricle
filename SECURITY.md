# Security policy

Auricle records meetings, reads the screen on request, and pastes text into
other apps, so a security bug here can expose exactly what people chose to keep
on their own machine. Reports are taken seriously and handled in private until
a fix ships.

## Reporting a vulnerability

Please report privately through GitHub:

1. Open the repository's **Security** tab and choose **Report a vulnerability**
   ([direct link](https://github.com/ManeeshJupalle/Auricle/security/advisories/new)).
2. Describe the issue, the version or commit, and the steps to reproduce it.
   A proof of concept against a test meeting helps; please don't include
   recordings or transcripts of real people.

Please don't open a public issue, discussion, or pull request for a
vulnerability before a fix is released. If the **Report a vulnerability**
button isn't available, open an issue that says only that you have a security
report and need a private channel, with no details, and one will be arranged.
There is no email address for reports.

What to expect: an acknowledgement, an assessment of whether it's in scope,
and a fix or a reasoned answer. When a fix ships, the advisory is published
with credit to you unless you'd rather not be named.

## Supported versions

Fixes land on `main` and ship in the next release; older releases don't get
separate patches. The app's built-in updater offers each new release.

## In scope

The trust model these are judged against is written down in
[docs/API.md](docs/API.md#trust-model): the daemon trusts the machine it runs
on and does not trust the browser.

- **The localhost API** (REST, `/ws/live`, SSE, and `/mcp` on
  `127.0.0.1:4820`). For example: a web page in your browser that can read a
  transcript, play retained audio, start a recording, or change a setting; a way
  around the same-origin check, the `Host` check that stops DNS rebinding, or
  the `nosniff`, CSP, and `Cross-Origin-Resource-Policy` headers; a request
  that serves files outside the engine's sessions directory.
- **Token mode** (a non-loopback bind with a bearer token). For example: any
  API route reachable without the token, a `?token=` accepted on routes other
  than `/ws/live` and retained audio, or a comparison that leaks the token
  through timing.
- **The egress ledger.** For example: user data (audio, a prompt, a summary, an
  auto-title, dictation audio or polish text) that the daemon sends to a cloud
  provider with no ledger row, a row that names the wrong destination or
  meeting, or an MCP read filed as "stayed local". The ledger covers the daemon
  only: `auricle record` writing no rows is documented, not a vulnerability.
- **The paste layer** (dictation in the overlay). For example: dictated text
  typed into a window other than the one the dictation started in, or into a
  higher-integrity window; text that lands in Windows clipboard history or
  cloud clipboard sync; a clipboard restored over something the user copied
  since; dictated words reaching a meeting transcript, an ask, or an MCP agent.

Also in scope: the overlay's webview and its CSP, the updater's signature
check, and anything that lets a meeting's "Them" side (whose words anyone on
the call controls) make an LLM, an MCP client, or the app do something it
shouldn't.

## Out of scope

- Other processes running as you on the same machine. They can use the whole
  API on loopback by design, and the database and retained audio are ordinary
  files under `%LOCALAPPDATA%\auricle` (see the trust model).
- What a cloud provider or an MCP client does with data after you chose to send
  it there.
- The unsigned installer's SmartScreen prompt (a known gap, in
  [RELEASING.md](RELEASING.md)).
- Reports from automated scanners with no demonstrated impact.
