import { describe, expect, it } from 'vitest';
import {
  initialState,
  reduce,
  QUICK_ASSIST_QUESTION,
  type OverlayEvent,
  type OverlayState,
} from './state';

function run(events: OverlayEvent[], from: OverlayState = initialState): OverlayState {
  return events.reduce(reduce, from);
}

const started = (provider: string | null = 'groq'): OverlayEvent => ({
  type: 'ASK_STARTED',
  askId: 'a1',
  chips: [],
  provider,
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
      { type: 'SUBMIT', question: 'what changed?', at: 1_000 },
      {
        type: 'ASK_STARTED',
        askId: 'a1',
        chips: [{ icon: 'screen', label: 'Jira' }],
        provider: 'groq',
      },
      { type: 'DELTA', text: 'The chunker ' },
      { type: 'DELTA', text: 'overlap.' },
      { type: 'DONE', usage: { total_tokens: 42 }, at: 2_350 },
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
    const asking = run([{ type: 'SUMMON' }, { type: 'SUBMIT', question: 'q1', at: 0 }]);
    expect(asking.phase).toBe('asking');
    expect(reduce(asking, { type: 'SUBMIT', question: 'q2', at: 1 })).toBe(asking);
    const streaming = reduce(asking, started());
    expect(reduce(streaming, { type: 'SUBMIT', question: 'q3', at: 2 })).toBe(streaming);
    const ready = reduce(initialState, { type: 'SUMMON' });
    expect(reduce(ready, { type: 'SUBMIT', question: '   ', at: 3 })).toBe(ready);
  });

  it('quick-assist starts a marked ask from hidden with the templated question', () => {
    const s = reduce(initialState, { type: 'QUICK_ASSIST', at: 500 });
    expect(s.phase).toBe('asking');
    expect(s.question).toBe(QUICK_ASSIST_QUESTION);
    expect(s.quick).toBe(true);
    expect(s.startedAt).toBe(500);
    // A second press while busy is a no-op.
    expect(reduce(s, { type: 'QUICK_ASSIST', at: 600 })).toBe(s);
  });

  it('a typed ask after a quick one clears the quick marker', () => {
    const quick = run([
      { type: 'QUICK_ASSIST', at: 0 },
      started(),
      { type: 'DELTA', text: 'x' },
      { type: 'DONE', usage: null, at: 100 },
    ]);
    expect(quick.quick).toBe(true);
    const typed = reduce(quick, { type: 'SUBMIT', question: 'follow up', at: 200 });
    expect(typed.quick).toBe(false);
  });

  it('a new ask clears the previous answer, chips, provider, and timing', () => {
    const answered = run([
      { type: 'QUICK_ASSIST', at: 0 },
      { type: 'ASK_STARTED', askId: 'a1', chips: [{ icon: 'transcript', label: 't' }], provider: 'ollama' },
      { type: 'DELTA', text: 'old answer' },
      { type: 'DONE', usage: null, at: 900 },
    ]);
    expect(answered.elapsedMs).toBe(900);
    const again = reduce(answered, { type: 'SUBMIT', question: 'follow up', at: 1_000 });
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
      { type: 'SUBMIT', question: 'q', at: 0 },
      { type: 'ERROR', message: 'llm groq returned HTTP 429' },
    ]);
    expect(s.phase).toBe('error');
    expect(s.error).toContain('429');
    expect(reduce(s, { type: 'DISMISS' }).phase).toBe('hidden');
  });

  it('dismissing mid-stream drops later deltas (view abandoned)', () => {
    const streaming = run([
      { type: 'QUICK_ASSIST', at: 0 },
      started(),
      { type: 'DELTA', text: 'partial ' },
    ]);
    const hidden = reduce(streaming, { type: 'DISMISS' });
    expect(hidden.phase).toBe('hidden');
    const after = reduce(hidden, { type: 'DELTA', text: 'late' });
    expect(after.answer, 'late delta dropped').toBe('partial ');
    // Re-summon shows what was there; the ask is not resumed.
    expect(reduce(after, { type: 'SUMMON' }).phase).toBe('ready');
  });

  it('summon while visible is a no-op (keeps the answer on screen)', () => {
    const answered = run([
      { type: 'QUICK_ASSIST', at: 0 },
      started(),
      { type: 'DELTA', text: 'answer' },
      { type: 'DONE', usage: null, at: 1 },
    ]);
    expect(reduce(answered, { type: 'SUMMON' })).toBe(answered);
  });

  it('accepts a first delta before ask_started (non-streaming fallback path)', () => {
    const asking = reduce(initialState, { type: 'QUICK_ASSIST', at: 0 });
    const s = reduce(asking, { type: 'DELTA', text: 'whole answer at once' });
    expect(s.phase).toBe('streaming');
    expect(s.answer).toBe('whole answer at once');
  });
});
