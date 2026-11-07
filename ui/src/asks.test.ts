import { afterEach, describe, expect, it, vi } from 'vitest';
import { api } from './api';
import { confirmDeleteAsk, screenText } from './components/AskList';

describe('stored asks', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const stubFetch = (respond: () => Response) => {
    const fetchMock = vi.fn(async (_url: string, _init?: RequestInit) => respond());
    vi.stubGlobal('fetch', fetchMock);
    return fetchMock;
  };

  it('lists one session, with paging', async () => {
    const fetchMock = stubFetch(() => Response.json({ asks: [] }));

    await api.asks('s 1', 100, 200);

    expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/asks?session=s+1&limit=100&offset=200');
  });

  it('takes the 204 from a delete as success, with no body to parse', async () => {
    const fetchMock = stubFetch(() => new Response(null, { status: 204 }));

    await expect(api.deleteAsk(7)).resolves.toBeUndefined();

    expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/asks/7');
    expect(fetchMock.mock.calls[0][1]?.method).toBe('DELETE');
  });

  it('surfaces a failed delete', async () => {
    stubFetch(() => Response.json({ error: 'ask not found' }, { status: 404 }));

    await expect(api.deleteAsk(7)).rejects.toThrow('ask not found');
  });

  it('deletes nothing until the user confirms', async () => {
    const fetchMock = stubFetch(() => new Response(null, { status: 204 }));

    expect(await confirmDeleteAsk(7, () => false)).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();

    expect(await confirmDeleteAsk(7, () => true)).toBe(true);
    expect(fetchMock).toHaveBeenCalledOnce();
  });
});

describe('screenText', () => {
  it('shows the window and text out of the stored JSON', () => {
    const stored = JSON.stringify({
      window_title: 'Roadmap',
      app_name: 'chrome',
      text: 'Q3 goals',
      captured_at: 1,
      ocr_ms: 2,
    });
    expect(screenText(stored)).toBe('Roadmap\n\nQ3 goals');
  });

  it('has nothing to show for an ask without screen text', () => {
    expect(screenText(null)).toBeNull();
  });

  it('shows anything else as stored rather than hide it', () => {
    expect(screenText('not json')).toBe('not json');
    expect(screenText('null')).toBe('null');
    expect(screenText('{}')).toBe('{}');
  });
});
