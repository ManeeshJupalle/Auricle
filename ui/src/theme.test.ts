import { afterEach, describe, expect, it, vi } from 'vitest';
import { api } from './api';
import { applyTheme, initTheme } from './theme';

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('theme when storage is blocked', () => {
  it('still boots, and takes the server setting', async () => {
    const dataset: Record<string, string> = {};
    vi.stubGlobal('document', { documentElement: { dataset } });
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('SecurityError: access is denied');
      },
      setItem: () => {
        throw new Error('SecurityError: access is denied');
      },
    });
    vi.spyOn(api, 'settings').mockResolvedValue({ ui_theme: 'light' });

    expect(() => initTheme()).not.toThrow();
    expect(dataset.theme).toBe('dark');
    await vi.waitFor(() => expect(dataset.theme).toBe('light'));
    expect(() => applyTheme('dark')).not.toThrow();
  });
});
