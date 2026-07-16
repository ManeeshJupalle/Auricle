import { describe, expect, it } from 'vitest';
import { nextPollDelay } from './connection';

describe('engine connection poll schedule', () => {
  it('polls at a relaxed cadence while healthy', () => {
    expect(nextPollDelay(0)).toBe(5000);
  });

  it('backs off 1-2-4-8 capped at 10s while down, and recovers instantly', () => {
    expect(nextPollDelay(1)).toBe(1000);
    expect(nextPollDelay(2)).toBe(2000);
    expect(nextPollDelay(3)).toBe(4000);
    expect(nextPollDelay(4)).toBe(8000);
    expect(nextPollDelay(5)).toBe(10000);
    expect(nextPollDelay(20)).toBe(10000);
    // First success resets to the healthy cadence.
    expect(nextPollDelay(0)).toBe(5000);
  });
});
