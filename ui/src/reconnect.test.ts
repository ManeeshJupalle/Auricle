import { describe, expect, it } from 'vitest';
import { ReconnectPolicy } from './ws';

describe('reconnect backoff policy', () => {
  it('doubles from 1s and caps at 10s', () => {
    const p = new ReconnectPolicy();
    expect(p.nextDelay()).toBe(1000);
    expect(p.nextDelay()).toBe(2000);
    expect(p.nextDelay()).toBe(4000);
    expect(p.nextDelay()).toBe(8000);
    expect(p.nextDelay()).toBe(10_000);
    expect(p.nextDelay()).toBe(10_000);
  });

  it('reset returns to the base delay (successful reconnect)', () => {
    const p = new ReconnectPolicy();
    p.nextDelay();
    p.nextDelay();
    p.reset();
    expect(p.nextDelay()).toBe(1000);
  });

  it('honors custom base and cap', () => {
    const p = new ReconnectPolicy(500, 2000);
    expect(p.nextDelay()).toBe(500);
    expect(p.nextDelay()).toBe(1000);
    expect(p.nextDelay()).toBe(2000);
    expect(p.nextDelay()).toBe(2000);
  });
});
