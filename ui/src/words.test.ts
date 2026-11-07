import { describe, expect, it } from 'vitest';
import { hasMatch, markParts } from './components/Marked';
import { pickLabel } from './components/Sidebar';
import { byMeeting } from './views/Search';
import type { ProviderInfo, SearchMatch } from './types';
import {
  agoWord,
  daysAgo,
  downloadLine,
  providerName,
  providerPlace,
  providerWhere,
  templateName,
  withoutRepeatedTitle,
} from './words';

describe('providerName', () => {
  it('names the providers people know', () => {
    expect(providerName('whisper-local')).toBe('Whisper');
    expect(providerName('openai-compat')).toBe('OpenAI-compatible');
  });

  it('shows an id it does not know as it is', () => {
    expect(providerName('my-own-llm')).toBe('my-own-llm');
  });
});

describe('providerPlace', () => {
  it('knows which side of the line each provider is on', () => {
    expect(providerPlace('whisper-local')).toBe('local');
    expect(providerPlace('deepgram')).toBe('cloud');
  });

  it('makes no claim about a provider it does not know', () => {
    expect(providerPlace('my-own-llm')).toBeUndefined();
    expect(providerWhere('my-own-llm')).toBe('my-own-llm');
  });

  it('says it in words too', () => {
    expect(providerWhere('deepgram')).toBe('Deepgram, in the cloud');
    expect(providerWhere('whisper-local')).toBe('Whisper, on this machine');
  });
});

describe('downloadLine', () => {
  it('counts what has arrived', () => {
    expect(downloadLine({ doneMb: 62.4, sizeMb: 142 }).replace(/\u00a0/g, ' ')).toBe(
      'Downloading the speech model, 62 of 142 MB',
    );
  });

  it('starts at zero, before the first progress event', () => {
    expect(downloadLine({ doneMb: 0, sizeMb: 142 }).replace(/\u00a0/g, ' ')).toContain(
      '0 of 142 MB',
    );
  });

  it('keeps the figures together when the line wraps', () => {
    expect(downloadLine({ doneMb: 62, sizeMb: 142 })).toContain('62\u00a0of\u00a0142\u00a0MB');
  });

  it('turns into checking once every byte is in', () => {
    expect(downloadLine({ doneMb: 142, sizeMb: 142 })).toBe('Checking the speech model');
  });
});

describe('withoutRepeatedTitle', () => {
  const title = 'Q3 retention review with the growth team';

  it('drops an opening heading the page already shows', () => {
    expect(withoutRepeatedTitle('# Q3 retention review\n\nDecided.', title)).toBe('Decided.');
    expect(withoutRepeatedTitle('## Action items\n- one', 'Action items')).toBe('- one');
  });

  it('keeps a heading that says something new', () => {
    const summary = '# Renewal risk at Northwind\n\nDecided.';
    expect(withoutRepeatedTitle(summary, title, 'Minutes')).toBe(summary);
  });

  it('leaves a summary with no heading alone', () => {
    expect(withoutRepeatedTitle('Decided.', title)).toBe('Decided.');
  });
});

describe('templateName', () => {
  it('turns a file name into a label', () => {
    expect(templateName('minutes')).toBe('Minutes');
    expect(templateName('action-items')).toBe('Action items');
  });
});

describe('agoWord', () => {
  const at = (y: number, m: number, d: number, h: number) =>
    new Date(y, m, d, h).getTime() / 1000;
  const oneAm = new Date(2026, 10, 7, 1);

  it('counts calendar days, so last night is yesterday at 1 am', () => {
    expect(daysAgo(at(2026, 10, 6, 23), oneAm)).toBe(1);
    expect(agoWord(at(2026, 10, 6, 23), oneAm)).toBe('yesterday');
  });

  it('agrees with the sidebar about today and older days', () => {
    expect(agoWord(at(2026, 10, 7, 0), oneAm)).toBe('today');
    expect(agoWord(at(2026, 10, 3, 12), oneAm)).toBe('4 days ago');
  });
});

describe('markParts', () => {
  const marked = (text: string, q: string) =>
    markParts(text, q)
      .filter((p) => p.hit)
      .map((p) => p.text);

  it('finds every occurrence, whatever the case, and keeps the original case', () => {
    expect(marked('Northwind renews. NORTHWIND has left.', 'northwind')).toEqual([
      'Northwind',
      'NORTHWIND',
    ]);
  });

  it('matches the whole phrase literally, as the server does', () => {
    expect(marked('week six email', 'six email')).toEqual(['six email']);
    expect(marked('week email six', 'six email')).toEqual([]);
    expect(marked('a.b and axb', 'a.b')).toEqual(['a.b']);
  });

  it('gives back the text unchanged', () => {
    const text = 'The Northwind renewal is in six weeks';
    expect(
      markParts(text, 'northwind')
        .map((p) => p.text)
        .join(''),
    ).toBe(text);
  });

  it('marks nothing for an empty query', () => {
    expect(markParts('anything', '  ')).toEqual([{ text: 'anything', hit: false }]);
    expect(hasMatch('anything', '  ')).toBe(false);
  });
});

describe('byMeeting', () => {
  const match = (session_id: string, t_start_ms: number): SearchMatch => ({
    session_id,
    session_title: session_id,
    started_at: 0,
    channel: 1,
    speaker: 'Them',
    t_start_ms,
    t_end_ms: t_start_ms + 1,
    text: '',
  });

  it('keeps the order the server sent', () => {
    const groups = byMeeting([match('newer', 5), match('newer', 9), match('older', 1)]);

    expect(groups.map((g) => g[0].session_id)).toEqual(['newer', 'older']);
    expect(groups[0].map((m) => m.t_start_ms)).toEqual([5, 9]);
  });
});

describe('pickLabel', () => {
  const provider = (over: Partial<ProviderInfo>): ProviderInfo => ({
    id: 'whisper-local',
    kind: 'local',
    ready: true,
    detail: '',
    default: true,
    ...over,
  });

  it('says where the audio goes, so the choice is honest before Start', () => {
    expect(pickLabel(provider({}))).toBe('Whisper on this machine');
    expect(pickLabel(provider({ id: 'deepgram', kind: 'cloud-streaming' }))).toBe(
      'Deepgram in the cloud',
    );
  });

  it('says when it cannot run yet', () => {
    expect(pickLabel(provider({ ready: false }))).toBe('Whisper on this machine, downloads first');
    expect(pickLabel(provider({ id: 'openai-compat', kind: 'cloud-batch', ready: false }))).toBe(
      'OpenAI-compatible in the cloud, not ready',
    );
  });
});
