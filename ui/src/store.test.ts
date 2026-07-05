import { beforeEach, describe, expect, it } from 'vitest';
import { useAuricle } from './store';
import type { WsEvent } from './types';

const apply = (ev: WsEvent) => useAuricle.getState().applyEvent(ev);

const partial = (channel: string, tStart: number, tEnd: number, text: string): WsEvent => ({
  type: 'partial',
  session: 's1',
  channel,
  speaker: channel === 'mic' ? 'You' : 'Them',
  t_start_ms: tStart,
  t_end_ms: tEnd,
  text,
});

const finalEv = (channel: string, tStart: number, tEnd: number, text: string): WsEvent => ({
  type: 'final',
  session: 's1',
  channel,
  speaker: channel === 'mic' ? 'You' : 'Them',
  t_start_ms: tStart,
  t_end_ms: tEnd,
  text,
  latency_ms: 640,
});

beforeEach(() => {
  useAuricle.getState().resetLive();
  useAuricle.setState({ engineState: 'idle', activeSession: null });
});

describe('partial → final transitions', () => {
  it('a growing partial updates the same row in place', () => {
    apply(partial('loopback', 0, 1000, 'hello'));
    const s1 = useAuricle.getState();
    expect(s1.order).toHaveLength(1);
    const id = s1.order[0];

    apply(partial('loopback', 0, 2000, 'hello world'));
    const s2 = useAuricle.getState();
    expect(s2.order).toHaveLength(1);
    expect(s2.order[0]).toBe(id);
    expect(s2.rows[id].text).toBe('hello world');
    expect(s2.rows[id].final).toBe(false);
  });

  it('a final flips the live partial row in place (same id)', () => {
    apply(partial('loopback', 0, 1000, 'hello wor'));
    const id = useAuricle.getState().order[0];

    apply(finalEv('loopback', 0, 1500, 'Hello world.'));
    const s = useAuricle.getState();
    expect(s.order).toHaveLength(1);
    expect(s.order[0]).toBe(id);
    expect(s.rows[id].final).toBe(true);
    expect(s.rows[id].text).toBe('Hello world.');
    expect(s.latencyMs).toBe(640);
  });

  it('after a flip, the next partial starts a new row', () => {
    apply(partial('loopback', 0, 1000, 'first'));
    apply(finalEv('loopback', 0, 1500, 'First.'));
    apply(partial('loopback', 2000, 2500, 'second'));
    const s = useAuricle.getState();
    expect(s.order).toHaveLength(2);
    expect(s.rows[s.order[1]].text).toBe('second');
    expect(s.rows[s.order[1]].final).toBe(false);
  });

  it('a final with no preceding partial appends (lossy partial lane)', () => {
    apply(finalEv('mic', 0, 1200, 'Dropped partials, final arrives.'));
    const s = useAuricle.getState();
    expect(s.order).toHaveLength(1);
    expect(s.rows[s.order[0]].final).toBe(true);
  });

  it('channels keep independent partial rows', () => {
    apply(partial('mic', 0, 500, 'me talking'));
    apply(partial('loopback', 100, 600, 'them talking'));
    apply(partial('mic', 0, 900, 'me talking more'));
    const s = useAuricle.getState();
    expect(s.order).toHaveLength(2);
    const micRow = Object.values(s.rows).find((r) => r.channel === 'mic')!;
    const loopRow = Object.values(s.rows).find((r) => r.channel === 'loopback')!;
    expect(micRow.text).toBe('me talking more');
    expect(loopRow.text).toBe('them talking');
  });

  it('a replayed final (resync race) does not duplicate its row', () => {
    apply(finalEv('mic', 5000, 6000, 'once'));
    apply(finalEv('mic', 5000, 6000, 'once'));
    expect(useAuricle.getState().order).toHaveLength(1);
  });
});

describe('live vs session state', () => {
  it('session_started resets the transcript and sets active state', () => {
    apply(finalEv('mic', 0, 1000, 'from an old session'));
    apply({ type: 'session_started', session: 's2', title: 'Standup', stt_provider: 'deepgram' });
    const s = useAuricle.getState();
    expect(s.order).toHaveLength(0);
    expect(s.engineState).toBe('recording');
    expect(s.activeSession).toBe('s2');
    expect(s.sessionTitle).toBe('Standup');
    expect(s.sessionProvider).toBe('deepgram');
  });

  it('session_stopped clears active state but keeps the transcript visible', () => {
    apply({ type: 'session_started', session: 's2', title: 'T', stt_provider: 'fake' });
    apply(finalEv('loopback', 0, 1000, 'the words'));
    apply({ type: 'session_stopped', session: 's2' });
    const s = useAuricle.getState();
    expect(s.engineState).toBe('idle');
    expect(s.activeSession).toBeNull();
    expect(s.order).toHaveLength(1);
    expect(s.vu).toEqual({});
  });

  it('loadFinals rebuilds the transcript from persisted segments', () => {
    apply(partial('mic', 9000, 9500, 'stale partial'));
    useAuricle.getState().loadFinals([
      { channel: 1, speaker: 'Them', t_start_ms: 0, t_end_ms: 2000, text: 'one', provider: 'x' },
      { channel: 0, speaker: 'You', t_start_ms: 2500, t_end_ms: 3000, text: 'two', provider: 'x' },
    ]);
    const s = useAuricle.getState();
    expect(s.order).toHaveLength(2);
    expect(s.rows[s.order[0]].speaker).toBe('Them');
    expect(s.rows[s.order[0]].channel).toBe('loopback');
    expect(s.rows[s.order[1]].channel).toBe('mic');
    expect(Object.values(s.rows).every((r) => r.final)).toBe(true);
  });

  it('a live final arriving after resync upserts instead of duplicating', () => {
    useAuricle.getState().loadFinals([
      { channel: 1, speaker: 'Them', t_start_ms: 0, t_end_ms: 2000, text: 'one', provider: 'x' },
    ]);
    apply(finalEv('loopback', 0, 2000, 'one'));
    expect(useAuricle.getState().order).toHaveLength(1);
  });
});

describe('speaker-turn grouping', () => {
  it('consecutive same-channel segments join one turn; channel change opens a new one', () => {
    apply(finalEv('loopback', 0, 1000, 'first'));
    apply(finalEv('loopback', 1500, 2500, 'second'));
    apply(finalEv('mic', 3000, 3500, 'reply'));
    apply(finalEv('loopback', 4000, 5000, 'back again'));
    const s = useAuricle.getState();
    expect(s.turnOrder).toHaveLength(3);
    const t = s.turnOrder.map((id) => s.turns[id]);
    expect(t[0].segIds).toHaveLength(2);
    expect(t[0].speaker).toBe('Them');
    expect(t[1].segIds).toHaveLength(1);
    expect(t[1].speaker).toBe('You');
    expect(t[2].segIds).toHaveLength(1);
  });

  it('a growing partial re-renders exactly one row: turn identities are untouched', () => {
    apply(finalEv('loopback', 0, 1000, 'earlier'));
    apply(partial('loopback', 1500, 2000, 'grow'));
    const before = useAuricle.getState();
    const beforeTurnOrder = before.turnOrder;
    const beforeTurns = before.turns;
    const partialId = before.partialRowByChannel['loopback']!;
    const beforeOtherRow = before.rows[before.order[0]];

    apply(partial('loopback', 1500, 3000, 'growing more'));

    const after = useAuricle.getState();
    // The turn layer the virtualizer renders is IDENTICAL by reference.
    expect(after.turnOrder).toBe(beforeTurnOrder);
    expect(after.turns).toBe(beforeTurns);
    // Sibling rows keep identity; only the partial's row changed.
    expect(after.rows[after.order[0]]).toBe(beforeOtherRow);
    expect(after.rows[partialId].text).toBe('growing more');
  });

  it('a final flipping its partial keeps the turn layer identical too', () => {
    apply(partial('mic', 0, 500, 'talking'));
    const before = useAuricle.getState();
    apply(finalEv('mic', 0, 900, 'Talking.'));
    const after = useAuricle.getState();
    expect(after.turnOrder).toBe(before.turnOrder);
    expect(after.turns).toBe(before.turns);
    expect(after.rows[after.turns[after.turnOrder[0]].segIds[0]].final).toBe(true);
  });

  it('loadFinals rebuilds turns from persisted segments', () => {
    useAuricle.getState().loadFinals([
      { channel: 1, speaker: 'Them', t_start_ms: 0, t_end_ms: 2000, text: 'a', provider: 'x' },
      { channel: 1, speaker: 'Them', t_start_ms: 2100, t_end_ms: 3000, text: 'b', provider: 'x' },
      { channel: 0, speaker: 'You', t_start_ms: 3500, t_end_ms: 4000, text: 'c', provider: 'x' },
    ]);
    const s = useAuricle.getState();
    expect(s.turnOrder).toHaveLength(2);
    expect(s.turns[s.turnOrder[0]].segIds).toHaveLength(2);
    expect(s.turns[s.turnOrder[1]].speaker).toBe('You');
  });
});

describe('meters and diagnostics', () => {
  it('vu events update per-channel levels', () => {
    apply({ type: 'vu', session: 's1', channel: 'mic', rms: 0.2 });
    apply({ type: 'vu', session: 's1', channel: 'loopback', rms: 0.05 });
    const s = useAuricle.getState();
    expect(s.vu['mic']).toBeCloseTo(0.2);
    expect(s.vu['loopback']).toBeCloseTo(0.05);
  });

  it('lifecycle and retitle events bump sessionsVersion for the sidebar', () => {
    const v0 = useAuricle.getState().sessionsVersion;
    apply({ type: 'session_started', session: 's9', title: 'Untitled session', stt_provider: 'x' });
    apply({ type: 'session_stopped', session: 's9' });
    apply({ type: 'session_updated', session: 's9', title: 'Pipeline Latency Review' });
    expect(useAuricle.getState().sessionsVersion).toBe(v0 + 3);
    // The transcript itself is untouched by a retitle.
    expect(useAuricle.getState().order).toHaveLength(0);
  });

  it('device_lost, error, and lag surface without touching the transcript', () => {
    apply(finalEv('mic', 0, 1000, 'kept'));
    apply({ type: 'device_lost', session: 's1', channel: 'mic', message: 'unplugged' });
    apply({ type: 'error', session: 's1', message: 'provider hiccup' });
    apply({ type: 'lag', dropped_partials: 12 });
    const s = useAuricle.getState();
    expect(s.deviceLost).toContain('unplugged');
    expect(s.lastError).toBe('provider hiccup');
    expect(s.droppedPartials).toBe(12);
    expect(s.order).toHaveLength(1);
  });
});
