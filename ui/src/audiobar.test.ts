import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import { AudioBar, leadTrack, playPlan } from './components/AudioBar';
import { fmtTs } from './components/TurnDoc';

const track = (duration: number, currentTime = 0) => ({ duration, currentTime });

describe('fmtTs', () => {
  it('counts minutes and seconds in the first hour', () => {
    expect(fmtTs(0)).toBe('00:00');
    expect(fmtTs(59 * 60_000 + 59_000)).toBe('59:59');
  });

  it('adds the hour after that, so nobody has to divide by sixty', () => {
    expect(fmtTs(3_600_000)).toBe('1:00:00');
    expect(fmtTs(2 * 3_600_000 + 5 * 60_000 + 3_000)).toBe('2:05:03');
  });
});

describe('leadTrack', () => {
  it('follows the longer track, whichever one it is', () => {
    const short = track(60);
    const long = track(90);
    expect(leadTrack([short, long])).toBe(long);
    expect(leadTrack([long, short])).toBe(long);
  });

  it('ignores a track whose duration is not known yet', () => {
    const unknown = track(NaN);
    const loaded = track(90);
    expect(leadTrack([unknown, loaded])).toBe(loaded);
  });
});

describe('playPlan', () => {
  it('starts every track mid-recording', () => {
    const tracks = [track(60, 30), track(90, 30)];
    expect(playPlan(tracks)).toEqual({ rewind: false, start: tracks });
  });

  it('leaves a track past its end silent instead of rewinding it to 0', () => {
    const short = track(60, 60);
    const long = track(90, 75);
    expect(playPlan([short, long])).toEqual({ rewind: false, start: [long] });
  });

  it('starts over on every track once all of them have finished', () => {
    const tracks = [track(60, 60), track(90, 90)];
    expect(playPlan(tracks)).toEqual({ rewind: true, start: tracks });
  });

  it('still tries a track that has not loaded, so its failure can surface', () => {
    const unknown = track(NaN);
    const finished = track(90, 90);
    expect(playPlan([unknown, finished])).toEqual({ rewind: false, start: [unknown] });
  });
});

describe('audio bar markup', () => {
  it('names its icon button and slider, and keeps a status region mounted', () => {
    const html = renderToStaticMarkup(createElement(AudioBar, { sessionId: 's1' }));
    expect(html).toContain('aria-label="Play"');
    expect(html).toContain('aria-label="Playback position"');
    expect(html).toContain('role="status"');
  });
});
