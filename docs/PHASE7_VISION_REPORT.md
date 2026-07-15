# Phase 7 Vision Report — Screen Capture + OCR

Empirical results from the capture-first probe, per the Global Rules: a
scratch binary (windows crate: enumerate → Windows.Graphics.Capture →
PNG → Windows.Media.Ocr) was built and run against real windows on the
dev machine *before* any `auricle-vision` crate code was written.

**Machine:** Windows 11 Home 10.0.26200 · single 1920×1080 monitor,
96 DPI (100 % scaling), SDR (no HDR display attached — HDR quirks could
not be probed and are honestly out of evidence) · `windows` crate 0.62.2
(`windows-future` 0.3.2) · OCR language pack: `en-US` only.

---

## Probe targets (all real windows, single frame each)

| Target | Window | Frame + copy | OCR | Lines | Total |
|---|---|---|---|---|---|
| Browser article | Wikipedia in Edge, 931×1005 | 335 + 12 ms | 117 ms | 36 | **510 ms** |
| Code editor | Cursor (this repo), 1920×1032 | 320 + 14 ms | 228 ms | 99 | **621 ms** |
| Terminal | Windows Terminal (README), 1115×628 | 272 + 12 ms | 66 ms | 24 | **383 ms** |
| Video playing | Big Buck Bunny in Edge, 1010×693 | 263–321 + ~12 ms | 12–28 ms | 1 | **336–415 ms** |

"Frame" time is dominated by per-call setup: D3D11 device creation +
capture item + frame pool + `StartCapture` + first-frame poll runs
**~260–335 ms cold, every call**. The naive per-call probe therefore
misses the <500 ms budget on a dense 1080p window (editor: 621 ms).
Consequence baked into the crate: **cache the D3D11 device and OCR
engine across calls** — only the item/pool/session are per-capture.
(Crate-measured numbers with the cached device are in the last section.)

## OCR quality per window type (honest reading)

- **Browser article (best case):** body text near-perfect and genuinely
  useful. Small chrome text degrades (`Soeech recoanition — Wikioedia`
  title bar), superscript citations mangle (`[16]` → `116]`).
- **Code editor:** recognizably useful but identifiers corrupt at small
  font sizes: `auricle` → `auride`, `llm` → `Ilm`/`11m`, `mic_raw` →
  `mlc raw`. Fine for "what is the user looking at", not for copying
  code out of a screenshot.
- **Terminal (16 px console text):** good. Symbol confusion is the
  weakness: table pipes `|` → `I`, hyphens → em-dashes, `laggy` →
  `taggy`, `(` → `C`.
- **Video:** OCR correctly returns (almost) nothing — only the title
  bar. No hallucinated text from imagery; cost collapses to ~12 ms.
- The **window title bar is part of the captured frame** and shows up
  as an OCR line. Left in deliberately: the title is honest context and
  already appears in `ScreenContext.window_title`.

## Reading-order defect observed in the wild

Windows OCR returns lines in its own order. On the real Wikipedia
capture, the bullet `• 1962` (x=35, y=561) was returned *after* its own
continuation text `— IBM's 16-word "Shoebox"…` (x=91, y=557): two OCR
lines from one visual line, emitted right-before-left. This is exactly
what the spec'd flattening fixes: group lines into bands by vertical
overlap, sort bands top-to-bottom, sort left-to-right within a band.
The crate's band grouping uses the line-height midpoint rule
(same band ⇔ vertical centers within half the taller line's height) and
is unit-tested on this exact real-world shape.

## Doc-vs-reality corrections

1. **`windows-future` 0.3 renamed the blocking await.** Every example in
   circulation calls `.get()` on `IAsyncOperation`; in the version
   resolved today the method is **`.join()`** (`get.rs` became
   `join.rs`). `BOOL` also moved to `windows::core`.
2. **`GraphicsCaptureAccess::RequestAccessAsync` silently requires the
   `Security_Authorization_AppCapabilityAccess` cargo feature** — the
   method vanishes (E0599, "associated item not found") without it,
   because its *return type* lives in that namespace. Nothing points
   from the error to the missing feature.
3. **No permission prompt, ever.** For an unpackaged desktop process,
   HWND-based `CreateForWindow` capture needs no consent UI, and the
   `Borderless` capability is auto-granted (`AppCapabilityAccessStatus`
   = 4 / Allowed, no prompt). `SetIsBorderRequired(false)` then succeeds,
   so the Windows 11 yellow capture border can be suppressed for the
   on-demand single frame. (This is the OS capture-indicator border on
   *our own* capture — unrelated to, and not, window concealment.)
4. **A minimized window "captures" forever without an error.**
   `CreateForWindow` and `StartCapture` both succeed on an iconic
   window, but a frame never arrives; the eventual poll-timeout surfaces
   as a nonsense `HRESULT(0x00000000)`. The crate must pre-check
   `IsIconic` and return the typed `MinimizedWindow` error instead of
   ever entering capture.
5. **A destroyed/invalid HWND fails cleanly:** `CreateForWindow` →
   `0x80070057` (E_INVALIDARG), message "Could not capture the given
   window." Mapped to the typed window-gone error.
6. **Cloaked windows capture *successfully* and yield blank frames.**
   Suspended UWP windows (`DWMWA_CLOAKED` ≠ 0) delivered a frame with
   zero OCR lines. Cloak state must be treated as "not capturable" when
   selecting the active window, or a peek can return convincing
   emptiness.
7. **`GetForegroundWindow` is not always a user window.** Immediately
   after launching a process from a background shell it returned an
   untitled `explorer.exe` shell window (the capture OCR'd taskbar
   tooltip text). Active-window selection must skip untitled, shell,
   cloaked, and tool windows and fall down the Z-order.
8. **WGC captures occluded windows — but only content that was ever
   painted.** A fully-covered, already-rendered Edge window OCR'd all 36
   lines perfectly. A *freshly opened* fully-covered Edge window never
   painted at all (Chromium occlusion detection ⇒
   `visibilityState: hidden` ⇒ first paint and `autoplay` both
   deferred), so its capture was a uniform default-background frame.
   Freshness of background-window content is app-dependent; the
   foreground window — the product path — is always current.
9. **`OcrEngine::MaxImageDimension` is 10 000 px** on this build, not
   the ~2 600 figure floating around older docs — no downscale path is
   needed for any plausible window. Guarded anyway with a typed error.
10. **`SoftwareBitmap::CreateCopyFromSurfaceAsync` accepts the capture
    frame's surface directly** — no manual staging-texture map/copy —
    and `OcrEngine` consumes the resulting premultiplied-BGRA8 bitmap
    as-is. (The probe's PNG path converted to straight alpha for the
    encoder; OCR needed no conversion.)
11. **DPI:** at 96 DPI the capture item size is the window rect minus
    the invisible resize borders (945×1012 rect → 931×1005 item); OCR
    coordinates are in captured-pixel space. Per-monitor-V2 awareness is
    set in the probe; no mixed-DPI monitor was available to probe, so
    scaled-monitor behavior is recorded as unverified rather than
    assumed.
12. **Console windows belong to `WindowsTerminal.exe`** on stock
    Windows 11 — `app_name` for a terminal peek reports the terminal
    host, not the shell inside it.
13. **The COM apartment dies with its last initialized thread — and
    takes the cached WinRT objects with it.** `auricle peek` initially
    warmed the device/engine on a helper thread; that thread exited
    before the capture and the main thread's use of the cached objects
    was a hard access violation (0xC0000005, before any error could
    surface). The daemon has the same shape: tokio reaps idle
    `spawn_blocking` threads between peeks. Fix: `CoIncrementMTAUsage`
    once per process (cookie deliberately leaked) pins the MTA for the
    process lifetime; per-thread `RoInitialize` remains as
    belt-and-braces. Verified by the previously-crashing peek flow.
14. **Foreground focus cannot be stolen, only earned.** During
    self-verification, `WScript.Shell.AppActivate` (returns success!)
    and `ShowWindow(SW_MINIMIZE/SW_RESTORE)` from a background process
    did NOT change the foreground window on this Windows 11 build; only
    newly launched processes reliably took foreground. Harmless for
    peek (the countdown exists so a human focuses the target), but a
    PHASE-9 watch-item: the overlay's summon path must count on the
    hotkey press itself for focus, not on programmatic activation.
15. **A window that has existed only while fully occluded OCRs as
    near-empty, not as an error.** A GitHub page opened behind the
    user's maximized windows returned 34 chars (title bar only) after
    30 s of "loading" — Chromium had never painted it (finding 8). No
    capture-level signal distinguishes this from a genuinely blank
    window; documented as an honest limitation of background-window
    capture rather than papered over.

## Design decisions locked for the crate

- Cache `ID3D11Device` (+ derived WinRT device) and the `OcrEngine` in
  `WindowsOcrReader`; create item/pool/session per capture and close
  them immediately after the single frame (on-demand rule).
- Pre-flight typed errors in order: window gone → minimized → cloaked;
  then map `CreateForWindow` E_INVALIDARG to window-gone and a
  first-frame timeout (2 s) to capture-failed.
- `TryCreateFromUserProfileLanguages()` returning null/error ⇒ typed
  `OcrLanguageMissing` error (en-US present here, so exercised only via
  the error-mapping unit tests).
- Active-window selection: `GetForegroundWindow`, skipped if excluded
  HWND / untitled / cloaked / tool window / our own process; then walk
  `GetTopWindow`→`GW_HWNDNEXT` Z-order for the first eligible window.
- Reading-order flattening as spec'd (bands by vertical-center overlap,
  then x), joined with newlines.
- No `SetWindowDisplayAffinity`, no `WDA_*` anywhere — verified by
  grepping the diff (acceptance gate).

## Crate-measured results (warm path)

`auricle peek` pre-creates the device + OCR engine during its 3-second
countdown, so the printed number is the true capture→text cost — the
same steady state the daemon reaches after its first peek. All runs
below are real windows on the dev machine, timings printed by the
shipped code:

| Target | Via | capture | OCR | capture→text |
|---|---|---|---|---|
| Browser article (Wikipedia, Edge ~1266×953) | `auricle peek` | 84 ms | 91 ms | **175 ms** |
| Code editor (VS Code, 1920×1048, dense) | `auricle peek` | 89 ms | 209 ms | **298 ms** |
| Same editor, second run | `auricle peek` | 94 ms | 168 ms | 262 ms |
| Dense UI (Task Manager, 1940×1040) | example harness | — | 297 ms | ~390 ms est. warm¹ |
| VS Code via daemon | `POST /api/v1/peek` | — | 199 ms | 200 OK |

¹ The Task Manager run was a cold single-shot process (1 217 ms total
including device init + process start); warm capture is ~90 ms on this
machine, hence the estimate. It is the only estimated number in the
table. Foreground focus cannot be forced from a background test process
(finding 14), so the dense-UI window was captured by HWND through the
`capture_hwnd` example — the identical capture+OCR path minus the
foreground pick.

**Budget verdict: <500 ms holds on the warm path for every probed
window type at 1080p** — worst case ~300 ms (dense editor). The naive
cold path (~500–620 ms, first table) is paid at most once per process,
during peek's countdown or the daemon's first peek.

Error paths verified against real windows (`capture_hwnd` example):
a minimized File Explorer window → `the target window is minimized`
(0 ms, pre-flight, no capture attempted); a destroyed HWND → `the
target window no longer exists`. The OCR-language-missing path cannot
be exercised on this machine (en-US installed) and is covered by the
endpoint error-mapping test.
