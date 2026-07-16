// Engine connection monitor: health polls with backoff while down,
// steady cadence while up. The schedule is a pure function (tested);
// the loop side-effects live in the caller.

/** Poll interval in ms after `consecutiveFailures` failed health checks. */
export function nextPollDelay(consecutiveFailures: number): number {
  if (consecutiveFailures <= 0) return 5_000; // healthy cadence
  // 1s → 2s → 4s → 8s, capped at 10s — same shape the engine UI uses.
  return Math.min(1_000 * 2 ** (consecutiveFailures - 1), 10_000);
}
