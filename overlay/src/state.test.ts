import { describe, expect, it } from 'vitest';
import {
  initialState,
  reduce,
  toastMs,
  QUICK_ASSIST_QUESTION,
  type OverlayEvent,
  type OverlayState,
} from './state';

function run(events: OverlayEvent[], from: OverlayState = initialState): OverlayState {
  return events.reduce(reduce, from);
}

const started = (provider: string | null = 'groq', gen = 1): OverlayEvent => ({
  type: 'ASK_STARTED',
  askId: 'a1',
  chips: [],
  provider,
  gen,
});

describe('overlay state machine', () => {
  it('summons from hidden to ready and dismisses back', () => {
    const shown = reduce(initialState, { type: 'SUMMON' });
    expect(shown.phase).toBe('ready');
    expect(reduce(shown, { type: 'DISMISS' }).phase).toBe('hidden');
  });

  it('runs a full ask lifecycle with provider and elapsed time', () => {
    const s = run([
      { type: 'SUMMON' },
      { type: 'SUBMIT', question: 'what changed?', at: 1_000, gen: 1 },
      {
        type: 'ASK_STARTED',
        askId: 'a1',
        chips: [{ icon: 'screen', label: 'Jira' }],
        provider: 'groq',
        gen: 1,
      },
      { type: 'DELTA', text: 'The chunker ', gen: 1 },
      { type: 'DELTA', text: 'overlap.', gen: 1 },
      { type: 'DONE', usage: { total_tokens: 42 }, at: 2_350, gen: 1 },
    ]);
    expect(s.phase).toBe('answered');
    expect(s.answer).toBe('The chunker overlap.');
    expect(s.chips).toEqual([{ icon: 'screen', label: 'Jira' }]);
    expect(s.usage).toEqual({ total_tokens: 42 });
    expect(s.provider).toBe('groq');
    expect(s.elapsedMs).toBe(1_350);
    expect(s.quick).toBe(false);
    expect(s.hasHistory).toBe(true);
  });

  it('ignores submits while busy and empty submits', () => {
    const asking = run([{ type: 'SUMMON' }, { type: 'SUBMIT', question: 'q1', at: 0, gen: 1 }]);
    expect(asking.phase).toBe('asking');
    expect(reduce(asking, { type: 'SUBMIT', question: 'q2', at: 1, gen: 2 })).toBe(asking);
    const streaming = reduce(asking, started());
    expect(reduce(streaming, { type: 'SUBMIT', question: 'q3', at: 2, gen: 3 })).toBe(streaming);
    const ready = reduce(initialState, { type: 'SUMMON' });
    expect(reduce(ready, { type: 'SUBMIT', question: '   ', at: 3, gen: 4 })).toBe(ready);
  });

  it('quick-assist starts a marked ask from hidden with the templated question', () => {
    const s = reduce(initialState, { type: 'QUICK_ASSIST', at: 500, gen: 1 });
    expect(s.phase).toBe('asking');
    expect(s.question).toBe(QUICK_ASSIST_QUESTION);
    expect(s.quick).toBe(true);
    expect(s.startedAt).toBe(500);
    // A second press while busy is a no-op.
    expect(reduce(s, { type: 'QUICK_ASSIST', at: 600, gen: 2 })).toBe(s);
  });

  it('a typed ask after a quick one clears the quick marker', () => {
    const quick = run([
      { type: 'QUICK_ASSIST', at: 0, gen: 1 },
      started(),
      { type: 'DELTA', text: 'x', gen: 1 },
      { type: 'DONE', usage: null, at: 100, gen: 1 },
    ]);
    expect(quick.quick).toBe(true);
    const typed = reduce(quick, { type: 'SUBMIT', question: 'follow up', at: 200, gen: 2 });
    expect(typed.quick).toBe(false);
  });

  it('a new ask clears the previous answer, chips, provider, and timing', () => {
    const answered = run([
      { type: 'QUICK_ASSIST', at: 0, gen: 1 },
      {
        type: 'ASK_STARTED',
        askId: 'a1',
        chips: [{ icon: 'transcript', label: 't' }],
        provider: 'ollama',
        gen: 1,
      },
      { type: 'DELTA', text: 'old answer', gen: 1 },
      { type: 'DONE', usage: null, at: 900, gen: 1 },
    ]);
    expect(answered.elapsedMs).toBe(900);
    const again = reduce(answered, { type: 'SUBMIT', question: 'follow up', at: 1_000, gen: 2 });
    expect(again.phase).toBe('asking');
    expect(again.answer).toBe('');
    expect(again.chips).toEqual([]);
    expect(again.provider).toBeNull();
    expect(again.elapsedMs).toBeNull();
    expect(again.startedAt).toBe(1_000);
    expect(again.hasHistory, 'history survives for follow_up').toBe(true);
  });

  it('errors surface and dismiss clears the view', () => {
    const s = run([
      { type: 'SUMMON' },
      { type: 'SUBMIT', question: 'q', at: 0, gen: 1 },
      { type: 'ERROR', message: 'llm groq returned HTTP 429', gen: 1 },
    ]);
    expect(s.phase).toBe('error');
    expect(s.error).toContain('429');
    expect(reduce(s, { type: 'DISMISS' }).phase).toBe('hidden');
  });

  it('dismissing mid-stream drops later deltas (view abandoned)', () => {
    const streaming = run([
      { type: 'QUICK_ASSIST', at: 0, gen: 1 },
      started(),
      { type: 'DELTA', text: 'partial ', gen: 1 },
    ]);
    const hidden = reduce(streaming, { type: 'DISMISS' });
    expect(hidden.phase).toBe('hidden');
    const after = reduce(hidden, { type: 'DELTA', text: 'late', gen: 1 });
    expect(after.answer, 'late delta dropped').toBe('partial ');
    // Re-summon shows what was there; the ask is not resumed.
    expect(reduce(after, { type: 'SUMMON' }).phase).toBe('ready');
  });

  it('summon while visible is a no-op (keeps the answer on screen)', () => {
    const answered = run([
      { type: 'QUICK_ASSIST', at: 0, gen: 1 },
      started(),
      { type: 'DELTA', text: 'answer', gen: 1 },
      { type: 'DONE', usage: null, at: 1, gen: 1 },
    ]);
    expect(reduce(answered, { type: 'SUMMON' })).toBe(answered);
  });

  it('accepts a first delta before ask_started (non-streaming fallback path)', () => {
    const asking = reduce(initialState, { type: 'QUICK_ASSIST', at: 0, gen: 1 });
    const s = reduce(asking, { type: 'DELTA', text: 'whole answer at once', gen: 1 });
    expect(s.phase).toBe('streaming');
    expect(s.answer).toBe('whole answer at once');
    // A late ask_started still reports what was captured.
    const chips = [{ icon: 'screen' as const, label: 'Jira' }];
    const after = reduce(s, { type: 'ASK_STARTED', askId: 'a1', chips, provider: 'groq', gen: 1 });
    expect(after.chips).toEqual(chips);
    expect(after.answer).toBe('whole answer at once');
  });
});

describe('superseded asks', () => {
  // Esc mid-answer, re-summon, ask again: the old stream is still landing.
  const secondAsk = run([
    { type: 'SUMMON' },
    { type: 'SUBMIT', question: 'first', at: 0, gen: 1 },
    started(),
    { type: 'DELTA', text: 'first answer', gen: 1 },
    { type: 'DISMISS' },
    { type: 'SUMMON' },
    { type: 'SUBMIT', question: 'second', at: 10, gen: 2 },
  ]);

  it('old deltas never reach the new answer', () => {
    const s = run(
      [
        { type: 'DELTA', text: 'stale', gen: 1 },
        started('groq', 2),
        { type: 'DELTA', text: 'stale too', gen: 1 },
        { type: 'DELTA', text: 'fresh', gen: 2 },
      ],
      secondAsk,
    );
    expect(s.answer).toBe('fresh');
  });

  it('the old answer_done does not end the new ask', () => {
    const s = run(
      [
        { type: 'DONE', usage: { total_tokens: 1 }, at: 20, gen: 1 },
        { type: 'DELTA', text: 'fresh', gen: 2 },
      ],
      secondAsk,
    );
    expect(s.phase).toBe('streaming');
    expect(s.usage).toBeNull();
    expect(s.answer).toBe('fresh');
  });

  it('an old ask_started cannot claim the new ask', () => {
    const s = reduce(secondAsk, {
      type: 'ASK_STARTED',
      askId: 'old',
      chips: [{ icon: 'screen', label: 'old window' }],
      provider: 'ollama',
      gen: 1,
    });
    expect(s).toBe(secondAsk);
  });

  it('an old failure does not turn the new ask into an error', () => {
    expect(reduce(secondAsk, { type: 'ERROR', message: 'stream broke', gen: 1 })).toBe(secondAsk);
  });

  it('a failure from a dismissed ask stays silent after re-summon', () => {
    const reopened = run([
      { type: 'SUMMON' },
      { type: 'SUBMIT', question: 'q', at: 0, gen: 1 },
      { type: 'DISMISS' },
      { type: 'SUMMON' },
    ]);
    expect(reduce(reopened, { type: 'ERROR', message: 'late', gen: 1 })).toBe(reopened);
  });

  it('the new run still accepts a delta ahead of its own ask_started', () => {
    const s = reduce(secondAsk, { type: 'DELTA', text: 'whole answer', gen: 2 });
    expect(s.phase).toBe('streaming');
    expect(s.answer).toBe('whole answer');
  });
});

describe('new chat', () => {
  const answered = run([
    { type: 'SUMMON' },
    { type: 'SUBMIT', question: 'q', at: 0, gen: 1 },
    {
      type: 'ASK_STARTED',
      askId: 'a1',
      chips: [{ icon: 'screen', label: 'Jira' }],
      provider: 'groq',
      gen: 1,
    },
    { type: 'DELTA', text: 'answer', gen: 1 },
    { type: 'DONE', usage: { total_tokens: 9 }, at: 5, gen: 1 },
  ]);

  it('clears the conversation and the follow-up flag', () => {
    expect(answered.hasHistory).toBe(true);
    expect(reduce(answered, { type: 'NEW_CHAT' })).toEqual({ ...initialState, phase: 'ready' });
  });

  it('clears an error too', () => {
    const failed = reduce(answered, { type: 'SUBMIT', question: 'again', at: 6, gen: 2 });
    const s = reduce(reduce(failed, { type: 'ERROR', message: 'x', gen: 2 }), { type: 'NEW_CHAT' });
    expect(s.phase).toBe('ready');
    expect(s.error).toBeNull();
  });

  it('is refused while an ask runs', () => {
    const asking = reduce(answered, { type: 'SUBMIT', question: 'again', at: 6, gen: 2 });
    expect(reduce(asking, { type: 'NEW_CHAT' })).toBe(asking);
  });
});

describe('toastMs', () => {
  it('keeps a short message up for the usual time', () => {
    expect(toastMs('Downloading update 0.5.0…')).toBe(6000);
  });

  it('gives instructions long enough to be read through', () => {
    expect(toastMs('x'.repeat(280))).toBeGreaterThanOrEqual(19_000);
  });
});
