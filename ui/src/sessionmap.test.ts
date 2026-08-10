import { describe, expect, it } from 'vitest';
import { buildMapTurns } from './components/SessionMap';
import type { Row, Turn } from './store';

function row(id: string, channel: string, tStartMs: number, tEndMs: number): Row {
  return { id, channel, speaker: channel === 'mic' ? 'You' : 'Them', tStartMs, tEndMs, text: '', final: true };
}

describe('buildMapTurns', () => {
  it('takes each turn end from its LAST segment, not its first', () => {
    const rows: Record<string, Row> = {
      a: row('a', 'loopback', 0, 4000),
      b: row('b', 'loopback', 4200, 9000),
      c: row('c', 'mic', 9500, 12_000),
    };
    const turns: Record<string, Turn> = {
      t0: { id: 't0', channel: 'loopback', speaker: 'Them', tStartMs: 0, segIds: ['a', 'b'] },
      t1: { id: 't1', channel: 'mic', speaker: 'You', tStartMs: 9500, segIds: ['c'] },
    };

    const map = buildMapTurns(['t0', 't1'], turns, rows);

    expect(map).toEqual([
      { index: 0, channel: 'loopback', speaker: 'Them', startMs: 0, endMs: 9000 },
      { index: 1, channel: 'mic', speaker: 'You', startMs: 9500, endMs: 12_000 },
    ]);
  });

  it('falls back to the start when a segment row is missing', () => {
    // Defensive: a turn whose row was evicted must not produce NaN geometry.
    const turns: Record<string, Turn> = {
      t0: { id: 't0', channel: 'mic', speaker: 'You', tStartMs: 500, segIds: ['gone'] },
    };

    const [turn] = buildMapTurns(['t0'], turns, {});

    expect(turn.startMs).toBe(500);
    expect(turn.endMs).toBe(500);
  });

  it('preserves transcript order so an index addresses the same turn', () => {
    const rows: Record<string, Row> = { a: row('a', 'mic', 0, 1000), b: row('b', 'loopback', 2000, 3000) };
    const turns: Record<string, Turn> = {
      t0: { id: 't0', channel: 'mic', speaker: 'You', tStartMs: 0, segIds: ['a'] },
      t1: { id: 't1', channel: 'loopback', speaker: 'Them', tStartMs: 2000, segIds: ['b'] },
    };

    expect(buildMapTurns(['t0', 't1'], turns, rows).map((t) => t.index)).toEqual([0, 1]);
  });
});
