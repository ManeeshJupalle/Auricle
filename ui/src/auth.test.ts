import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api } from './api';
import { readToken, saveToken, validToken, withBearer, withToken } from './auth';
import { useAuricle } from './store';
import { liveUrl, startLiveSocket } from './ws';

const TOKEN = 'tok/en+1 2&3';

function fakeStorage(): Storage {
  const items = new Map<string, string>();
  return {
    get length() {
      return items.size;
    },
    clear: () => items.clear(),
    getItem: (k) => items.get(k) ?? null,
    key: (i) => [...items.keys()][i] ?? null,
    removeItem: (k) => void items.delete(k),
    setItem: (k, v) => void items.set(k, v),
  };
}

describe('token mode', () => {
  let session: Storage;
  let local: Storage;

  beforeEach(() => {
    session = fakeStorage();
    local = fakeStorage();
    vi.stubGlobal('sessionStorage', session);
    vi.stubGlobal('localStorage', local);
    useAuricle.setState({ tokenPrompt: null });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const stubFetch = (status = 200) => {
    const fetchMock = vi.fn(async (_url: string, _init?: RequestInit) =>
      Response.json(status === 401 ? { error: 'unauthorized' } : {}, { status }),
    );
    vi.stubGlobal('fetch', fetchMock);
    return fetchMock;
  };

  const sentAuth = (fetchMock: ReturnType<typeof stubFetch>, call = 0) =>
    new Headers(fetchMock.mock.calls[call][1]?.headers).get('Authorization');

  describe('request building', () => {
    it('adds no header and no init when there is no token', () => {
      expect(withBearer(undefined, null)).toBeUndefined();
      const init = { method: 'POST' };
      expect(withBearer(init, null)).toBe(init);
    });

    it('adds the bearer header and keeps the rest of the request', () => {
      const out = withBearer(
        { method: 'PUT', headers: { 'Content-Type': 'application/json' } },
        'abc',
      );
      const headers = new Headers(out?.headers);

      expect(out?.method).toBe('PUT');
      expect(headers.get('Authorization')).toBe('Bearer abc');
      expect(headers.get('Content-Type')).toBe('application/json');
    });

    it('leaves a URL alone with no token', () => {
      expect(withToken('/ws/live')).toBe('/ws/live');
    });

    it('encodes the token into a URL', () => {
      saveToken(TOKEN);
      expect(withToken('/ws/live')).toBe('/ws/live?token=tok%2Fen%2B1%202%263');
      expect(withToken('/a?b=1')).toBe('/a?b=1&token=tok%2Fen%2B1%202%263');
    });
  });

  describe('every API call', () => {
    it('sends the header on reads and on writes that set their own headers', async () => {
      saveToken('abc');
      const fetchMock = stubFetch();

      await api.health();
      await api.putSettings({ retention_days: 30 });
      await api.backup();
      await api.deleteAsk(1).catch(() => {});

      expect(fetchMock).toHaveBeenCalledTimes(4);
      for (let call = 0; call < 4; call += 1) {
        expect(sentAuth(fetchMock, call)).toBe('Bearer abc');
      }
      expect(new Headers(fetchMock.mock.calls[1][1]?.headers).get('Content-Type')).toBe(
        'application/json',
      );
    });

    it('sends no header on a daemon that needs none', async () => {
      const fetchMock = stubFetch();

      await api.health();

      expect(sentAuth(fetchMock)).toBeNull();
    });

    it('fetches the export with the header, since a link cannot carry it', async () => {
      saveToken('abc');
      const fetchMock = stubFetch();

      await api.exportBlob('s1');

      expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/sessions/s1/export?format=md');
      expect(sentAuth(fetchMock)).toBe('Bearer abc');
    });
  });

  describe('URLs a browser fetches by itself', () => {
    it('puts the token on the socket', () => {
      saveToken(TOKEN);
      expect(liveUrl({ protocol: 'https:', host: 'box:4820' })).toBe(
        'wss://box:4820/ws/live?token=tok%2Fen%2B1%202%263',
      );
    });

    it('keeps the socket URL bare without one', () => {
      expect(liveUrl({ protocol: 'http:', host: '127.0.0.1:4820' })).toBe(
        'ws://127.0.0.1:4820/ws/live',
      );
    });

    it('puts the token on retained audio', () => {
      saveToken(TOKEN);
      expect(api.audioUrl('s 1', 'mic')).toBe(
        '/api/v1/sessions/s%201/audio/mic?token=tok%2Fen%2B1%202%263',
      );
    });

    it('keeps it off the export link, the contract has no query token there', () => {
      saveToken(TOKEN);
      expect(api.exportUrl('s1')).toBe('/api/v1/sessions/s1/export?format=md');
    });
  });

  describe('opening the socket', () => {
    const stubSocket = () => {
      const socket = vi.fn(function () {});
      vi.stubGlobal('WebSocket', socket);
      vi.stubGlobal('location', { protocol: 'http:', host: 'box:4820' });
      return socket;
    };

    afterEach(() => {
      vi.useRealTimers();
    });

    it('holds a token-bearing socket back until the daemon answers', async () => {
      // The browser prints a failed socket's URL to the console, token and all.
      vi.useFakeTimers();
      saveToken('abc');
      const socket = stubSocket();
      vi.stubGlobal(
        'fetch',
        vi.fn(async () => {
          throw new TypeError('connection refused');
        }),
      );

      startLiveSocket();
      await vi.advanceTimersByTimeAsync(0);
      expect(socket).not.toHaveBeenCalled();

      stubFetch();
      await vi.advanceTimersByTimeAsync(1000);
      expect(socket).toHaveBeenCalledWith('ws://box:4820/ws/live?token=abc');
    });

    it('connects at once with no token to protect', () => {
      const socket = stubSocket();
      const fetchMock = stubFetch();

      startLiveSocket();

      expect(socket).toHaveBeenCalledWith('ws://box:4820/ws/live');
      expect(fetchMock).not.toHaveBeenCalled();
    });
  });

  describe('a 401', () => {
    it('asks for a token when none was sent', async () => {
      stubFetch(401);

      await expect(api.health()).rejects.toThrow('unauthorized');

      expect(useAuricle.getState().tokenPrompt).toBe('required');
    });

    it('forgets a refused token and says it was refused', async () => {
      saveToken('wrong');
      stubFetch(401);

      await api.health().catch(() => {});

      expect(readToken()).toBeNull();
      expect(useAuricle.getState().tokenPrompt).toBe('rejected');
    });

    it('keeps saying refused while the other boot requests fail too', async () => {
      saveToken('wrong');
      stubFetch(401);

      await Promise.allSettled([api.health(), api.settings()]);
      await api.providers().catch(() => {});

      expect(useAuricle.getState().tokenPrompt).toBe('rejected');
    });

    it('leaves the prompt closed on any other failure', async () => {
      saveToken('abc');
      stubFetch(500);

      await api.health().catch(() => {});

      expect(readToken()).toBe('abc');
      expect(useAuricle.getState().tokenPrompt).toBeNull();
    });
  });

  describe('where the token can end up', () => {
    it('stays in sessionStorage and out of localStorage', () => {
      saveToken('abc');

      expect(session.length).toBe(1);
      expect(local.length).toBe(0);
    });

    it('stays out of the error a failed call raises', async () => {
      saveToken('s3cret');
      stubFetch(401);

      const error = await api.health().catch((e: Error) => e);

      expect((error as Error).message).not.toContain('s3cret');
    });

    it('stays out of the console', async () => {
      const logged = (['log', 'info', 'warn', 'error', 'debug'] as const).map((level) =>
        vi.spyOn(console, level),
      );
      saveToken('s3cret');
      stubFetch(401);

      await api.health().catch(() => {});

      for (const spy of logged) expect(spy).not.toHaveBeenCalled();
    });
  });

  describe('when storage is blocked', () => {
    it('runs tokenless instead of throwing', async () => {
      vi.stubGlobal('sessionStorage', {
        getItem: () => {
          throw new Error('denied');
        },
      });
      const fetchMock = stubFetch();

      await api.health();

      expect(sentAuth(fetchMock)).toBeNull();
    });
  });

  describe('validToken', () => {
    it('takes what a header can carry', () => {
      expect(validToken('abc-DEF_123.~+/=')).toBe(true);
    });

    it('refuses what would make the request itself throw', () => {
      expect(validToken('')).toBe(false);
      expect(validToken('tök')).toBe(false);
      expect(validToken('a\nb')).toBe(false);
    });
  });
});
