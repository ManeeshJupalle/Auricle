import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import {
  buildMapTurns,
  fmtScale,
  layoutMap,
  MIN_BLOCK_PCT,
  nextMapIndex,
  scaleMarks,
  SessionMap,
  talkShares,
  turnAt,
  type MapBlock,
  type MapTurn,
} from './components/SessionMap';
import { buildTurns, TURN_MAX_MS, type Row, type Turn } from './store';

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

  it('gives a one-sided session a navigable block per stretch, not one bar', () => {
    const rows: Record<string, Row> = {};
    const order: string[] = [];
    const hours = 2 * 3_600_000;
    for (let t = 0; t < hours; t += 10_000) {
      const id = `r${t}`;
      rows[id] = row(id, 'loopback', t, t + 10_000);
      order.push(id);
    }

    const { turnOrder, turns } = buildTurns(order, rows);
    const map = buildMapTurns(turnOrder, turns, rows);

    expect(map).toHaveLength(hours / TURN_MAX_MS);
    // Contiguous speech stays contiguous: no gaps open up between the blocks.
    expect(map.slice(1).every((t, i) => t.startMs === map[i].endMs)).toBe(true);
    expect(map[map.length - 1].endMs).toBe(hours);
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

function turn(index: number, channel: string, startMs: number, endMs: number): MapTurn {
  return { index, channel, speaker: channel === 'mic' ? 'You' : 'Them', startMs, endMs };
}

/** A two-voice exchange of short turns, like a long fast-moving call. */
function denseSession(turnCount: number, stepMs: number): MapTurn[] {
  return Array.from({ length: turnCount }, (_, i) =>
    turn(i, i % 2 === 0 ? 'loopback' : 'mic', i * stepMs, i * stepMs + stepMs - 200),
  );
}

function expectNoOverlapInEitherLane(blocks: MapBlock[]) {
  for (const lane of ['loopback', 'mic']) {
    const inLane = blocks.filter((b) => b.turn.channel === lane);
    inLane.slice(1).forEach((b, i) => {
      expect(b.leftPct).toBeGreaterThanOrEqual(inLane[i].leftPct + inLane[i].widthPct - 1e-9);
    });
  }
}

describe('layoutMap', () => {
  it('leaves blocks that fit alone', () => {
    const turns = [turn(0, 'loopback', 0, 10_000), turn(1, 'mic', 10_000, 20_000)];

    expect(layoutMap(turns, 20_000)).toEqual([
      { turn: turns[0], turns: 1, leftPct: 0, widthPct: 50 },
      { turn: turns[1], turns: 1, leftPct: 50, widthPct: 50 },
    ]);
  });

  it('merges turns too close to draw apart, so nothing overlaps', () => {
    const turns = denseSession(400, 1000);
    const blocks = layoutMap(turns, 400_000);

    expectNoOverlapInEitherLane(blocks);
    expect(blocks.length).toBeLessThan(turns.length);
    expect(blocks.reduce((n, b) => n + b.turns, 0)).toBe(turns.length);
  });

  it('caps the block count however many turns there are', () => {
    const blocks = layoutMap(denseSession(20_000, 1000), 20_000_000);

    expect(blocks.length).toBeLessThanOrEqual(2 * (100 / MIN_BLOCK_PCT));
    expectNoOverlapInEitherLane(blocks);
  });

  it('never merges across the two voices', () => {
    const blocks = layoutMap([turn(0, 'loopback', 0, 100), turn(1, 'mic', 50, 150)], 1_000_000);

    expect(blocks.map((b) => b.turn.channel)).toEqual(['loopback', 'mic']);
  });

  it('jumps a merged block to its first turn', () => {
    const turns = [turn(0, 'mic', 1000, 1100), turn(1, 'mic', 1200, 1300)];

    const [block] = layoutMap(turns, 1_000_000);

    expect(block.turn).toBe(turns[0]);
    expect(block.turns).toBe(2);
  });

  it('gives every block the minimum width and keeps it on the track', () => {
    const turns = [turn(0, 'mic', 0, 10), turn(1, 'mic', 500_000, 500_010), turn(2, 'mic', 999_990, 1_000_000)];

    const blocks = layoutMap(turns, 1_000_000);

    expect(blocks).toHaveLength(3);
    for (const b of blocks) {
      expect(b.widthPct).toBeCloseTo(MIN_BLOCK_PCT);
      expect(b.leftPct + b.widthPct).toBeLessThanOrEqual(100 + 1e-9);
    }
  });

  it('keeps contiguous speech as separate blocks', () => {
    const hours = 2 * 3_600_000;
    const turns = Array.from({ length: hours / TURN_MAX_MS }, (_, i) =>
      turn(i, 'loopback', i * TURN_MAX_MS, (i + 1) * TURN_MAX_MS),
    );

    expect(layoutMap(turns, hours)).toHaveLength(turns.length);
  });
});

describe('talkShares', () => {
  it('fills a block that is one solid turn', () => {
    const turns = [turn(0, 'loopback', 0, 500_000), turn(1, 'mic', 500_000, 1_000_000)];

    expect(talkShares(layoutMap(turns, 1_000_000), turns, 1_000_000)).toEqual([1, 1]);
  });

  it('thins a block that is mostly silence between short turns', () => {
    // Ten one-second turns, close enough to merge into one block of 50 seconds.
    const turns = Array.from({ length: 10 }, (_, i) => turn(i, 'mic', i * 4000, i * 4000 + 1000));

    const blocks = layoutMap(turns, 10_000_000);
    const [share] = talkShares(blocks, turns, 10_000_000);

    expect(blocks).toHaveLength(1);
    expect(share).toBeLessThan(0.3);
  });

  it('keeps a talker and a listener apart in a long meeting', () => {
    const hours = 3 * 3_600_000;
    const turns = Array.from({ length: 1000 }, (_, i) => [
      turn(i * 2, 'loopback', i * 10_000, i * 10_000 + 9000),
      turn(i * 2 + 1, 'mic', i * 10_000 + 9000, i * 10_000 + 9500),
    ]).flat();

    const blocks = layoutMap(turns, hours);
    const shares = talkShares(blocks, turns, hours);
    const mean = (lane: string) => {
      const inLane = shares.filter((_, i) => blocks[i].turn.channel === lane);
      return inLane.reduce((a, b) => a + b, 0) / inLane.length;
    };

    expect(mean('loopback')).toBeGreaterThan(mean('mic') * 3);
  });

  it('never claims more than the whole block', () => {
    const blocks = layoutMap(denseSession(400, 1000), 400_000);

    for (const share of talkShares(blocks, denseSession(400, 1000), 400_000)) {
      expect(share).toBeLessThanOrEqual(1);
    }
  });
});

describe('turnAt', () => {
  const turns = [turn(0, 'loopback', 1000, 2000), turn(1, 'mic', 5000, 6000)];

  it('finds the turn being spoken', () => {
    expect(turnAt(turns, 1500)).toBe(0);
    expect(turnAt(turns, 5000)).toBe(1);
  });

  it('holds on the last turn through a silence', () => {
    expect(turnAt(turns, 3000)).toBe(0);
    expect(turnAt(turns, 9000)).toBe(1);
  });

  it('finds nothing before the first word', () => {
    expect(turnAt(turns, 500)).toBe(-1);
  });
});

describe('scaleMarks', () => {
  const minutes = (m: number) => m * 60_000;

  it('leaves at most five marks between the ends, however long the meeting', () => {
    for (const length of [3, 12, 47, 90, 186, 600]) {
      expect(scaleMarks(minutes(length)).length).toBeLessThanOrEqual(5);
    }
  });

  it('lands on round minutes', () => {
    expect(scaleMarks(minutes(47))).toEqual([10, 20, 30, 40].map(minutes));
  });

  it('keeps a mark clear of the end, whose own label sits there', () => {
    expect(scaleMarks(minutes(41))).toEqual([10, 20, 30].map(minutes));
  });

  it('has nothing to mark in a meeting shorter than its smallest step', () => {
    expect(scaleMarks(40_000)).toEqual([]);
  });
});

describe('fmtScale', () => {
  it('reads as a person would say it', () => {
    expect(fmtScale(40_000)).toBe('40 s');
    expect(fmtScale(47 * 60_000)).toBe('47 min');
    expect(fmtScale(120 * 60_000)).toBe('2 h');
    expect(fmtScale(186 * 60_000)).toBe('3 h 6 min');
  });
});

describe('nextMapIndex', () => {
  it('steps through time with the arrow keys', () => {
    expect(nextMapIndex('ArrowRight', 2, 5)).toBe(3);
    expect(nextMapIndex('ArrowDown', 2, 5)).toBe(3);
    expect(nextMapIndex('ArrowLeft', 2, 5)).toBe(1);
    expect(nextMapIndex('ArrowUp', 2, 5)).toBe(1);
  });

  it('stops at either end instead of wrapping', () => {
    expect(nextMapIndex('ArrowLeft', 0, 5)).toBe(0);
    expect(nextMapIndex('ArrowRight', 4, 5)).toBe(4);
  });

  it('jumps to the ends with Home and End', () => {
    expect(nextMapIndex('Home', 3, 5)).toBe(0);
    expect(nextMapIndex('End', 1, 5)).toBe(4);
  });

  it('leaves every other key alone, so Tab still leaves the map', () => {
    expect(nextMapIndex('Tab', 1, 5)).toBeNull();
    expect(nextMapIndex('Enter', 1, 5)).toBeNull();
  });
});

describe('SessionMap', () => {
  const html = (turns: MapTurn[], durationMs: number) =>
    renderToStaticMarkup(createElement(SessionMap, { turns, durationMs, onPick: () => {} }));

  it('is one tab stop however long the session is', () => {
    const markup = html(denseSession(400, 6000), 2_400_000);

    expect(markup.match(/tabindex="0"/g)).toHaveLength(1);
    expect(markup.match(/tabindex="-1"/g)?.length).toBeGreaterThan(100);
  });

  it('draws each voice on its own side of the centerline', () => {
    const markup = html([turn(0, 'loopback', 0, 100), turn(1, 'mic', 100, 200)], 200);

    expect(markup).toContain('map-turn loopback');
    expect(markup).toContain('map-turn mic');
  });

  it('says how many turns a merged block holds', () => {
    const markup = html([turn(0, 'mic', 0, 10), turn(1, 'mic', 20, 30)], 1_000_000);

    expect(markup).toContain('aria-label="Jump to You at 00:00, 2 turns"');
  });

  it('marks hits on their own side, and where a recording was cut off', () => {
    const markup = renderToStaticMarkup(
      createElement(SessionMap, {
        turns: [turn(0, 'loopback', 0, 100), turn(1, 'mic', 100, 200)],
        durationMs: 200,
        onPick: () => {},
        hits: [{ ms: 50, channel: 'loopback' }],
        cutMs: 200,
      }),
    );

    expect(markup).toContain('map-hit loopback');
    expect(markup).toContain('map-cut');
    expect(markup).toContain('Cut off');
  });

  it('breaks a lane where its device was lost', () => {
    const markup = renderToStaticMarkup(
      createElement(SessionMap, {
        turns: [turn(0, 'mic', 0, 100)],
        durationMs: 200,
        onPick: () => {},
        lost: { ms: 100, channel: 'mic' },
      }),
    );

    expect(markup).toContain('map-lost mic');
    expect(markup).toContain('Mic lost');
  });

  it('draws nothing for a session with no timeline', () => {
    expect(html([], 1000)).toBe('');
    expect(html([turn(0, 'mic', 0, 0)], 0)).toBe('');
  });
});
