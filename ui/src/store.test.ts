import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api } from './api';
import { liveStatus, TURN_GAP_MS, TURN_MAX_MS, useAuricle } from './store';
import type { Health, SegmentInfo, SessionDetail, WsEvent } from './types';
import { resync } from './ws';

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

  it('a cleared partial leaves the transcript, and only on its own channel', () => {
    apply(partial('loopback', 0, 1000, 'the far side speaking'));
    apply(partial('mic', 0, 1000, 'the far side speaking'));
    apply({ type: 'partial_cleared', session: 's1', channel: 'mic' });

    const s = useAuricle.getState();
    expect(s.order).toHaveLength(1);
    expect(s.rows[s.order[0]].channel).toBe('loopback');
    expect(s.turnOrder).toHaveLength(1);
    expect(s.partialRowByChannel.mic).toBeUndefined();
    expect(s.partialRowByChannel.loopback).toBe(s.order[0]);
  });

  it('clearing a channel with no partial changes nothing', () => {
    apply(finalEv('mic', 0, 1500, 'Already final.'));
    apply({ type: 'partial_cleared', session: 's1', channel: 'mic' });
    expect(useAuricle.getState().order).toHaveLength(1);
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

  it('markStopping shows the stop in progress until session_stopped lands', () => {
    apply({ type: 'session_started', session: 's2', title: 'T', stt_provider: 'fake' });
    useAuricle.getState().markStopping('s2');
    expect(useAuricle.getState().engineState).toBe('stopping');
    expect(useAuricle.getState().activeSession).toBe('s2');

    apply({ type: 'session_stopped', session: 's2' });
    expect(useAuricle.getState().engineState).toBe('idle');
  });

  it('a stop response arriving after session_stopped cannot wedge the button', () => {
    apply({ type: 'session_started', session: 's2', title: 'T', stt_provider: 'fake' });
    apply({ type: 'session_stopped', session: 's2' });

    useAuricle.getState().markStopping('s2');

    expect(useAuricle.getState().engineState).toBe('idle');
  });

  it('a stale stop response does not touch a newer session', () => {
    apply({ type: 'session_started', session: 's2', title: 'T', stt_provider: 'fake' });
    apply({ type: 'session_stopped', session: 's2' });
    apply({ type: 'session_started', session: 's3', title: 'T', stt_provider: 'fake' });

    useAuricle.getState().markStopping('s2');

    expect(useAuricle.getState().engineState).toBe('recording');
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
  it('closely spaced same-channel segments join one turn; channel change opens a new one', () => {
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

  it('a long pause opens a new turn with its own timestamp', () => {
    apply(finalEv('loopback', 0, 1000, 'before'));
    apply(finalEv('loopback', 1000 + TURN_GAP_MS, 7000, 'after'));
    const s = useAuricle.getState();
    expect(s.turnOrder).toHaveLength(2);
    expect(s.turns[s.turnOrder[1]].tStartMs).toBe(1000 + TURN_GAP_MS);
  });

  it('a pause just under the limit stays inside the turn', () => {
    apply(finalEv('loopback', 0, 1000, 'before'));
    apply(finalEv('loopback', 1000 + TURN_GAP_MS - 1, 7000, 'after'));
    expect(useAuricle.getState().turnOrder).toHaveLength(1);
  });

  it('an unbroken monologue is cut at the size limit', () => {
    const segMs = 10_000;
    const total = TURN_MAX_MS * 3;
    for (let t = 0; t < total; t += segMs) apply(finalEv('loopback', t, t + segMs, 'words'));
    const s = useAuricle.getState();
    expect(s.turnOrder).toHaveLength(3);
    expect(s.turnOrder.map((id) => s.turns[id].tStartMs)).toEqual([
      0,
      TURN_MAX_MS,
      TURN_MAX_MS * 2,
    ]);
  });

  it('a partial after a pause opens its turn at once, then grows in place', () => {
    apply(finalEv('loopback', 0, 1000, 'before'));
    apply(partial('loopback', 1000 + TURN_GAP_MS, 6500, 'aft'));
    const before = useAuricle.getState();
    expect(before.turnOrder).toHaveLength(2);

    apply(partial('loopback', 1000 + TURN_GAP_MS, 8000, 'after that'));

    expect(useAuricle.getState().turnOrder).toBe(before.turnOrder);
    expect(useAuricle.getState().turns).toBe(before.turns);
  });

  it('live and stored grouping agree, so a resync does not reshape the document', () => {
    const script: [number, number, number][] = [
      [1, 0, 2000],
      [1, 2500, 4000],
      [1, 4000 + TURN_GAP_MS, 12_000],
      [0, 12_500, 13_000],
      [1, 14_000, 70_000],
      [1, 71_000, 80_000],
    ];
    const groups = () => {
      const s = useAuricle.getState();
      return s.turnOrder.map((id) => s.turns[id].segIds.map((seg) => s.rows[seg].tStartMs));
    };

    for (const [channel, start, end] of script) {
      apply(finalEv(channel === 0 ? 'mic' : 'loopback', start, end, 'x'));
    }
    const live = groups();

    useAuricle.getState().loadFinals(
      script.map(([channel, start, end]) => ({
        channel,
        speaker: channel === 0 ? 'You' : 'Them',
        t_start_ms: start,
        t_end_ms: end,
        text: 'x',
        provider: 'x',
      })),
    );
    expect(groups()).toEqual(live);
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

describe('reconnect resync', () => {
  const health = (active: string | null): Health => ({
    status: 'ok',
    version: '0',
    state: active ? 'recording' : 'idle',
    active_session: active,
  });
  const detail = (transcript: SegmentInfo[] = []): SessionDetail => ({
    id: 's7',
    title: 'Standup',
    started_at: 1_790_000_000,
    ended_at: null,
    stt_provider: 'deepgram',
    meta: {},
    transcript,
    summaries: [],
  });
  const seg = (tStart: number, tEnd: number, text: string): SegmentInfo => ({
    channel: 1,
    speaker: 'Them',
    t_start_ms: tStart,
    t_end_ms: tEnd,
    text,
    provider: 'x',
  });

  /** Hold the detail fetch open, as a slow daemon would, until released. */
  const holdDetail = () => {
    let release!: (d: SessionDetail) => void;
    const pending = new Promise<SessionDetail>((resolve) => {
      release = resolve;
    });
    vi.spyOn(api, 'health').mockResolvedValue(health('s7'));
    vi.spyOn(api, 'session').mockReturnValue(pending);
    return release;
  };

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('F5 while recording restores the title, provider, and start time', async () => {
    vi.spyOn(api, 'health').mockResolvedValue(health('s7'));
    vi.spyOn(api, 'session').mockResolvedValue(detail([seg(0, 1000, 'one')]));

    await resync();

    const s = useAuricle.getState();
    expect(s.engineState).toBe('recording');
    expect(s.activeSession).toBe('s7');
    expect(s.sessionTitle).toBe('Standup');
    expect(s.sessionProvider).toBe('deepgram');
    expect(s.liveStartedAtMs).toBe(1_790_000_000_000);
    expect(s.order).toHaveLength(1);
  });

  it('bumps sessionsVersion so the sidebar drops a session that died meanwhile', async () => {
    useAuricle.setState({ engineState: 'recording', activeSession: 's7' });
    const v0 = useAuricle.getState().sessionsVersion;
    vi.spyOn(api, 'health').mockResolvedValue(health(null));

    await resync();

    const s = useAuricle.getState();
    expect(s.engineState).toBe('idle');
    expect(s.activeSession).toBeNull();
    expect(s.sessionsVersion).toBe(v0 + 1);
  });

  it('a final landing during the fetch survives the older snapshot', async () => {
    const release = holdDetail();
    const done = resync();
    apply(finalEv('loopback', 2000, 3000, 'arrived mid-fetch'));
    // Buffered, not lost and not yet visible.
    expect(useAuricle.getState().order).toHaveLength(0);

    release(detail([seg(0, 1000, 'persisted')]));
    await done;

    const s = useAuricle.getState();
    expect(s.order.map((id) => s.rows[id].text)).toEqual(['persisted', 'arrived mid-fetch']);
  });

  it('a final in both the snapshot and the buffer stays one row', async () => {
    const release = holdDetail();
    const done = resync();
    apply(finalEv('loopback', 0, 1000, 'once'));

    release(detail([seg(0, 1000, 'once')]));
    await done;

    expect(useAuricle.getState().order).toHaveLength(1);
  });

  it('a partial buffered ahead of its final does not duplicate the snapshot row', async () => {
    const release = holdDetail();
    const done = resync();
    apply(partial('loopback', 0, 500, 'on'));
    apply(finalEv('loopback', 0, 1000, 'once'));

    release(detail([seg(0, 1000, 'once')]));
    await done;

    const s = useAuricle.getState();
    expect(s.order).toHaveLength(1);
    expect(s.partialRowByChannel['loopback']).toBeUndefined();
  });

  it('a session_stopped during the fetch beats the snapshot', async () => {
    const release = holdDetail();
    const done = resync();
    apply({ type: 'session_stopped', session: 's7' });

    release(detail());
    await done;

    expect(useAuricle.getState().engineState).toBe('idle');
    expect(useAuricle.getState().activeSession).toBeNull();
  });

  it('a failed fetch still delivers what it buffered', async () => {
    vi.spyOn(api, 'health').mockRejectedValue(new Error('down'));
    const done = resync();
    apply(finalEv('mic', 0, 1000, 'kept'));

    await done;

    expect(useAuricle.getState().order).toHaveLength(1);
  });

  it('a resync superseded by a newer connection applies nothing', async () => {
    const release = holdDetail();
    let current = true;
    const stale = resync(() => current);
    current = false;

    release(detail([seg(0, 1000, 'stale snapshot')]));
    await stale;

    expect(useAuricle.getState().order).toHaveLength(0);
    useAuricle.getState().endResync(null);
  });

  it('a different session does not inherit the previous one\'s banners', async () => {
    useAuricle.setState({ activeSession: 's6', deviceLost: 'mic: unplugged', lastError: 'old' });
    vi.spyOn(api, 'health').mockResolvedValue(health('s7'));
    vi.spyOn(api, 'session').mockResolvedValue(detail());

    await resync();

    expect(useAuricle.getState().deviceLost).toBeNull();
    expect(useAuricle.getState().lastError).toBeNull();
  });
});

describe('settings changes', () => {
  it('bump the version the sidebar watches and drop a summary provider pick', () => {
    useAuricle.setState({ summaryProvider: 'groq', summaryTemplate: 'actions' });
    const v0 = useAuricle.getState().settingsVersion;

    useAuricle.getState().settingsChanged();

    const s = useAuricle.getState();
    expect(s.settingsVersion).toBe(v0 + 1);
    expect(s.summaryProvider).toBeNull();
    // The template is not a privacy decision, so it stays.
    expect(s.summaryTemplate).toBe('actions');
  });
});

describe('screen reader status', () => {
  const status = () => liveStatus(useAuricle.getState());

  beforeEach(() => {
    useAuricle.setState({ conn: 'open' });
  });

  it('follows the recording lifecycle', () => {
    expect(status()).toBe('Not recording.');
    apply({ type: 'session_started', session: 's1', title: 'T', stt_provider: 'x' });
    expect(status()).toBe('Recording.');
    apply({ type: 'session_stopped', session: 's1' });
    expect(status()).toBe('Not recording.');
  });

  it('stays silent on transcript and meter traffic', () => {
    apply({ type: 'session_started', session: 's1', title: 'T', stt_provider: 'x' });
    const before = status();
    apply(partial('mic', 0, 500, 'secret words'));
    apply(partial('mic', 0, 900, 'secret words go on'));
    apply(finalEv('mic', 0, 1000, 'secret words went on'));
    apply({ type: 'vu', session: 's1', channel: 'mic', rms: 0.4 });
    expect(status()).toBe(before);
  });

  it('reports a lost connection, but not the first connect', () => {
    useAuricle.setState({ conn: 'connecting' });
    expect(status()).toBe('Not recording.');
    useAuricle.setState({ conn: 'reconnecting' });
    expect(status()).toContain('reconnecting');
  });

  it('carries device and engine errors while the session is active', () => {
    apply({ type: 'session_started', session: 's1', title: 'T', stt_provider: 'x' });
    apply({ type: 'device_lost', session: 's1', channel: 'mic', message: 'unplugged' });
    apply({ type: 'error', session: 's1', message: 'provider hiccup' });
    expect(status()).toContain('unplugged');
    expect(status()).toContain('provider hiccup');
  });

  it('does not keep repeating a finished session\'s errors', () => {
    apply({ type: 'session_started', session: 's1', title: 'T', stt_provider: 'x' });
    apply({ type: 'error', session: 's1', message: 'provider hiccup' });
    apply({ type: 'session_stopped', session: 's1' });
    expect(status()).toBe('Not recording.');
  });

  it('passes the model download notice through', () => {
    apply({ type: 'model_download_started', session: 's3', model: 'base.en', size_mb: 141 });
    expect(status()).toContain('base.en');
  });

  it('stays quiet through download progress, or it would talk over the meeting', () => {
    apply({ type: 'model_download_started', session: 's3', model: 'base.en', size_mb: 141 });
    const before = status();
    apply({ type: 'model_download_progress', session: 's3', model: 'base.en', done_mb: 62, size_mb: 141 });
    expect(status()).toBe(before);
  });

  it('says once when the download turns into checking', () => {
    apply({ type: 'model_download_started', session: 's3', model: 'base.en', size_mb: 141 });
    apply({ type: 'model_download_progress', session: 's3', model: 'base.en', done_mb: 141, size_mb: 141 });
    expect(status()).toContain('Checking');
  });
});

describe('model download progress', () => {
  const download = () => useAuricle.getState().download;

  it('starts at nothing done, before the first progress event arrives', () => {
    apply({ type: 'model_download_started', session: 's3', model: 'base.en', size_mb: 142 });
    expect(download()).toEqual({ doneMb: 0, sizeMb: 142 });
  });

  it('follows the progress events', () => {
    apply({ type: 'model_download_started', session: 's3', model: 'base.en', size_mb: 142 });
    apply({ type: 'model_download_progress', session: 's3', model: 'base.en', done_mb: 62, size_mb: 142 });
    expect(download()).toEqual({ doneMb: 62, sizeMb: 142 });
  });

  it('picks up a download it joined part way through', () => {
    apply({ type: 'model_download_progress', session: 's3', model: 'base.en', done_mb: 62, size_mb: 142 });
    expect(useAuricle.getState().notice).toContain('base.en');
    expect(download()).toEqual({ doneMb: 62, sizeMb: 142 });
  });

  it('clears once the session starts or the start is over', () => {
    apply({ type: 'model_download_started', session: 's3', model: 'base.en', size_mb: 142 });
    apply({ type: 'session_started', session: 's3', title: 'T', stt_provider: 'x' });
    expect(download()).toBeNull();

    apply({ type: 'model_download_started', session: 's4', model: 'base.en', size_mb: 142 });
    apply({ type: 'session_stopped', session: 's4' });
    expect(download()).toBeNull();
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

  it('model download notice appears and clears once the session starts', () => {
    apply({ type: 'model_download_started', session: 's3', model: 'base.en', size_mb: 141 });
    const notice = useAuricle.getState().notice;
    expect(notice).toContain('base.en');
    expect(notice).toContain('141');
    apply({ type: 'session_started', session: 's3', title: 'T', stt_provider: 'whisper-local' });
    expect(useAuricle.getState().notice).toBeNull();
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
