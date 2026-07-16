import { describe, expect, it } from 'vitest';
import { deriveChips } from './chips';

describe('context chip derivation (the honesty affordance)', () => {
  it('derives both chips from a full ask_started', () => {
    const chips = deriveChips(
      {
        screen: { window_title: 'Sprint 12 Board - Jira', app_name: 'chrome' },
        transcript_segments: 14,
      },
      10,
    );
    expect(chips).toEqual([
      { icon: 'screen', label: 'chrome — Sprint 12 Board - Jira' },
      { icon: 'transcript', label: 'transcript: last 10 min · 14 segments' },
    ]);
  });

  it('no screen captured means no screen chip — never a fabricated one', () => {
    const chips = deriveChips({ screen: null, transcript_segments: 3 }, 10);
    expect(chips).toHaveLength(1);
    expect(chips[0].icon).toBe('transcript');
  });

  it('transcript not requested means no transcript chip', () => {
    const chips = deriveChips(
      { screen: { window_title: 'Notes', app_name: 'notepad' }, transcript_segments: null },
      10,
    );
    expect(chips).toHaveLength(1);
    expect(chips[0].icon).toBe('screen');
  });

  it('an empty transcript window says so instead of hiding', () => {
    const chips = deriveChips({ screen: null, transcript_segments: 0 }, 10);
    expect(chips[0].label).toBe('transcript: empty window');
  });

  it('long window titles truncate with an ellipsis', () => {
    const chips = deriveChips(
      {
        screen: {
          window_title: 'A very long browser tab title that keeps going and going forever',
          app_name: 'msedge',
        },
        transcript_segments: null,
      },
      10,
    );
    expect(chips[0].label.length).toBeLessThanOrEqual('msedge — '.length + 34);
    expect(chips[0].label.endsWith('…')).toBe(true);
  });

  it('skips the app prefix when the title already names it', () => {
    const chips = deriveChips(
      { screen: { window_title: 'Task Manager', app_name: 'Taskmgr' }, transcript_segments: null },
      10,
    );
    // "Task Manager" doesn't contain "Taskmgr" — prefix kept.
    expect(chips[0].label).toBe('Taskmgr — Task Manager');
    const chips2 = deriveChips(
      { screen: { window_title: 'Almanac', app_name: 'almanac' }, transcript_segments: null },
      10,
    );
    expect(chips2[0].label).toBe('Almanac');
  });

  it('respects a non-default window length on the label', () => {
    const chips = deriveChips({ screen: null, transcript_segments: 5 }, 25);
    expect(chips[0].label).toBe('transcript: last 25 min · 5 segments');
  });
});
