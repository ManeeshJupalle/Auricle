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

describe('overlay state machine', () => {
  it('summons from hidden to ready and dismisses back', () => {
    const shown = reduce(initialState, { type: 'SUMMON' });
    expect(shown.phase).toBe('ready');
    expect(reduce(shown, { type: 'DISMISS' }).phase).toBe('hidden');
  });

  it('runs a full ask lifecycle', () => {
    const s = run([
      { type: 'SUMMON' },
      { type: 'SUBMIT', question: 'what changed?' },
      { type: 'ASK_STARTED', askId: 'a1', chips: [{ icon: 'screen', label: 'Jira' }] },
      { type: 'DELTA', text: 'The chunker ' },
      { type: 'DELTA', text: 'overlap.' },
      { type: 'DONE', usage: { total_tokens: 42 } },
    ]);
    expect(s.phase).toBe('answered');
    expect(s.answer).toBe('The chunker overlap.');
    expect(s.chips).toEqual([{ icon: 'screen', label: 'Jira' }]);
    expect(s.usage).toEqual({ total_tokens: 42 });
    expect(s.hasHistory).toBe(true);
  });

  it('ignores submits while busy and empty submits', () => {
    const asking = run([{ type: 'SUMMON' }, { type: 'SUBMIT', question: 'q1' }]);
    expect(asking.phase).toBe('asking');
    expect(reduce(asking, { type: 'SUBMIT', question: 'q2' })).toBe(asking);
    const streaming = reduce(asking, { type: 'ASK_STARTED', askId: 'a', chips: [] });
    expect(reduce(streaming, { type: 'SUBMIT', question: 'q3' })).toBe(streaming);
    const ready = reduce(initialState, { type: 'SUMMON' });
    expect(reduce(ready, { type: 'SUBMIT', question: '   ' })).toBe(ready);
  });

  it('quick-assist starts an ask from hidden with the templated question', () => {
    const s = reduce(initialState, { type: 'QUICK_ASSIST' });
    expect(s.phase).toBe('asking');
    expect(s.question).toBe(QUICK_ASSIST_QUESTION);
    // A second press while busy is a no-op.
    expect(reduce(s, { type: 'QUICK_ASSIST' })).toBe(s);
  });

  it('a new ask clears the previous answer, chips, and error', () => {
    const answered = run([
      { type: 'QUICK_ASSIST' },
      { type: 'ASK_STARTED', askId: 'a1', chips: [{ icon: 'transcript', label: 't' }] },
      { type: 'DELTA', text: 'old answer' },
      { type: 'DONE', usage: null },
    ]);
    const again = reduce(answered, { type: 'SUBMIT', question: 'follow up' });
    expect(again.phase).toBe('asking');
    expect(again.answer).toBe('');
    expect(again.chips).toEqual([]);
    expect(again.hasHistory, 'history survives for follow_up').toBe(true);
  });

  it('errors surface and dismiss clears the view', () => {
    const s = run([
      { type: 'SUMMON' },
      { type: 'SUBMIT', question: 'q' },
      { type: 'ERROR', message: 'llm groq returned HTTP 429' },
    ]);
    expect(s.phase).toBe('error');
    expect(s.error).toContain('429');
    expect(reduce(s, { type: 'DISMISS' }).phase).toBe('hidden');
  });

  it('dismissing mid-stream drops later deltas (view abandoned)', () => {
    const streaming = run([
      { type: 'QUICK_ASSIST' },
      { type: 'ASK_STARTED', askId: 'a1', chips: [] },
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
      { type: 'QUICK_ASSIST' },
      { type: 'ASK_STARTED', askId: 'a1', chips: [] },
      { type: 'DELTA', text: 'answer' },
      { type: 'DONE', usage: null },
    ]);
    expect(reduce(answered, { type: 'SUMMON' })).toBe(answered);
  });

  it('accepts a first delta before ask_started (non-streaming fallback path)', () => {
    const asking = reduce(initialState, { type: 'QUICK_ASSIST' });
    const s = reduce(asking, { type: 'DELTA', text: 'whole answer at once' });
    expect(s.phase).toBe('streaming');
    expect(s.answer).toBe('whole answer at once');
  });
});
