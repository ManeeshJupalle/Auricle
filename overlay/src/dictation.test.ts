import { describe, expect, it } from 'vitest';
import { checkLines, type DictationCheck } from './dictation';

const healthy: DictationCheck = {
  hotkey: 'Ctrl+Shift+D',
  mode: 'hold',
  registered: true,
  hotkey_error: null,
  engine_up: true,
  provider: { id: 'whisper-local', local: true, ready: true, detail: null, is_default: true },
};

describe('dictation setup check', () => {
  it('reports a working local setup as all good, and says local is the default', () => {
    const lines = checkLines(healthy);
    expect(lines.every((l) => l.ok)).toBe(true);
    expect(lines[0].text).toBe('Ctrl+Shift+D is ready: hold it while you talk, let go to paste.');
    expect(lines[2].text).toContain('runs on this machine');
    expect(lines[2].text).toContain('is the default');
  });

  it('describes toggle mode', () => {
    const lines = checkLines({ ...healthy, mode: 'toggle' });
    expect(lines[0].text).toContain('press it again to paste');
  });

  it('says plainly when the hotkey could not be registered', () => {
    const lines = checkLines({
      ...healthy,
      registered: false,
      hotkey_error: 'HotKey already registered',
    });
    expect(lines[0].ok).toBe(false);
    expect(lines[0].text).toContain('HotKey already registered');
    expect(lines[0].text).toContain('overlay.toml');
  });

  it('flags a down engine and leaves the provider for later', () => {
    const lines = checkLines({ ...healthy, engine_up: false, provider: null });
    expect(lines[1].ok).toBe(false);
    expect(lines[2].ok).toBe(false);
    expect(lines[2].text).toContain('is the default');
  });

  it('warns that a cloud provider sends the voice off the machine', () => {
    const lines = checkLines({
      ...healthy,
      provider: { id: 'deepgram', local: false, ready: true, detail: null, is_default: false },
    });
    expect(lines[2].ok).toBe(false);
    expect(lines[2].text).toContain('leaves this machine');
    expect(lines[2].text).toContain('dictation_provider');
  });

  it('passes on why a provider is not ready', () => {
    const lines = checkLines({
      ...healthy,
      provider: {
        id: 'whisper-local',
        local: true,
        ready: false,
        detail: 'model small.en missing',
        is_default: true,
      },
    });
    expect(lines[2].ok).toBe(false);
    expect(lines[2].text).toContain('model small.en missing');
  });
});
