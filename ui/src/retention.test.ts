import { afterEach, describe, expect, it, vi } from 'vitest';
import { api } from './api';
import { parseRetentionDays, retentionDeletesMore } from './views/Settings';

describe('retention days', () => {
  it('accepts whole days from zero up', () => {
    expect(parseRetentionDays('0')).toBe(0);
    expect(parseRetentionDays(' 90 ')).toBe(90);
  });

  it('refuses anything that is not a whole number of days', () => {
    for (const text of ['', ' ', '-1', '1.5', '1e3', 'ninety', '99999999999999999999']) {
      expect(parseRetentionDays(text)).toBeNull();
    }
  });

  it('asks first when a change would delete more than today', () => {
    expect(retentionDeletesMore(30, 0)).toBe(true);
    expect(retentionDeletesMore(30, 90)).toBe(true);
  });

  it('does not ask when a change keeps more', () => {
    expect(retentionDeletesMore(0, 30)).toBe(false);
    expect(retentionDeletesMore(90, 30)).toBe(false);
    expect(retentionDeletesMore(30, 30)).toBe(false);
  });
});

describe('database backup', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('posts with no body and returns the path the daemon wrote', async () => {
    const fetchMock = vi.fn(async (_url: string, _init?: RequestInit) =>
      Response.json({ path: 'C:\\data\\backups\\auricle-1.db' }, { status: 201 }),
    );
    vi.stubGlobal('fetch', fetchMock);

    await expect(api.backup()).resolves.toEqual({ path: 'C:\\data\\backups\\auricle-1.db' });

    expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/backup');
    expect(fetchMock.mock.calls[0][1]?.method).toBe('POST');
    expect(fetchMock.mock.calls[0][1]?.body).toBeUndefined();
  });
});
