import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, appendPage } from './api';
import { EGRESS_PAGE, egressHasMore, kindLabel } from './views/Egress';

describe('egress paging', () => {
  it('offers more only when the last page came back full', () => {
    expect(egressHasMore(EGRESS_PAGE)).toBe(true);
    expect(egressHasMore(EGRESS_PAGE - 1)).toBe(false);
  });

  it('keeps growing past the 1000 rows one request can return', () => {
    const shown = Array.from({ length: 1000 }, (_, id) => ({ id }));
    const later = Array.from({ length: EGRESS_PAGE }, (_, i) => ({ id: 1000 + i }));
    expect(appendPage(shown, later)).toHaveLength(1000 + EGRESS_PAGE);
  });

  it('drops rows a newer entry pushed back onto the next page', () => {
    expect(appendPage([{ id: 3 }, { id: 2 }], [{ id: 2 }, { id: 1 }])).toEqual([
      { id: 3 },
      { id: 2 },
      { id: 1 },
    ]);
  });
});

describe('egress kinds', () => {
  it('names the auto-title kind', () => {
    expect(kindLabel('title')).toBe('Auto-title');
  });

  it('names the dictation kinds', () => {
    expect(kindLabel('dictation')).toBe('Dictation audio');
    expect(kindLabel('dictation_polish')).toBe('Dictation polish');
  });

  it('shows a kind it does not know as is', () => {
    expect(kindLabel('something_new')).toBe('something_new');
  });
});

describe('egress request', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const urlFor = async (call: () => Promise<unknown>) => {
    const fetchMock = vi.fn(async () => Response.json({ entries: [], totals: [] }));
    vi.stubGlobal('fetch', fetchMock);
    await call();
    return (fetchMock.mock.calls[0] as unknown[])[0];
  };

  it('stays bare for the callers that want the default page', async () => {
    expect(await urlFor(() => api.egress())).toBe('/api/v1/egress');
  });

  it('scopes to a session and a limit', async () => {
    expect(await urlFor(() => api.egress('s 1', 400))).toBe(
      '/api/v1/egress?session=s+1&limit=400',
    );
  });

  it('asks past the first page with an offset', async () => {
    expect(await urlFor(() => api.egress('s 1', 200, 1200))).toBe(
      '/api/v1/egress?session=s+1&limit=200&offset=1200',
    );
  });

  it('leaves a zero offset out', async () => {
    expect(await urlFor(() => api.egress(undefined, 200, 0))).toBe('/api/v1/egress?limit=200');
  });
});
