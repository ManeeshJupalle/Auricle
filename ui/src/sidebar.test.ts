import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, appendPage, PagedLoads } from './api';
import { canPick, firstUseNote, serverDefault, startBody } from './components/Sidebar';
import type { ProviderInfo } from './types';

const provider = (id: string, ready: boolean, isDefault = false): ProviderInfo => ({
  id,
  kind: id === 'whisper-local' ? 'local' : 'streaming',
  ready,
  detail: '',
  default: isDefault,
});

describe('provider picker', () => {
  it('sends no stt_provider unless the picker was used', () => {
    expect(startBody(null)).toEqual({});
    expect(startBody('deepgram')).toEqual({ stt_provider: 'deepgram' });
  });

  it('shows the settings default over the config default, as the engine resolves it', () => {
    const list = [provider('whisper-local', true), provider('deepgram', true, true)];
    expect(serverDefault(list, { default_provider: 'whisper-local' })).toBe('whisper-local');
  });

  it('falls back to the config default when no setting exists', () => {
    const list = [provider('whisper-local', true), provider('deepgram', true, true)];
    expect(serverDefault(list, {})).toBe('deepgram');
  });

  it('never swaps a not-ready default for a ready cloud provider', () => {
    // whisper-local is "not ready" until its model downloads on first use.
    const list = [provider('whisper-local', false, true), provider('deepgram', true)];
    expect(serverDefault(list, {})).toBe('whisper-local');
  });

  it('lets a fresh install choose local before the model exists', () => {
    expect(canPick(provider('whisper-local', false))).toBe(true);
    expect(firstUseNote(provider('whisper-local', false))).toBe(true);
  });

  it('keeps a cloud provider with no key unavailable', () => {
    expect(canPick(provider('deepgram', false))).toBe(false);
    expect(firstUseNote(provider('deepgram', false))).toBe(false);
  });

  it('says nothing about a download once the model is there', () => {
    expect(canPick(provider('whisper-local', true))).toBe(true);
    expect(firstUseNote(provider('whisper-local', true))).toBe(false);
  });
});

describe('session list paging', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('asks for one page and reads the total', async () => {
    const fetchMock = vi.fn(async (_url: string) =>
      Response.json({ sessions: [{ id: 'a' }], total: 120 }),
    );
    vi.stubGlobal('fetch', fetchMock);

    const page = await api.sessionPage(50, 100);

    expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/sessions?limit=50&offset=100');
    expect(page.total).toBe(120);
    expect(page.sessions).toHaveLength(1);
  });

  it('keeps the unpaged call for callers that want every session', async () => {
    const fetchMock = vi.fn(async (_url: string) => Response.json({ sessions: [] }));
    vi.stubGlobal('fetch', fetchMock);

    await api.sessions();
    await api.sessions('budget');

    expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/sessions');
    expect(fetchMock.mock.calls[1][0]).toBe('/api/v1/sessions?q=budget');
  });

  it('does not repeat a row when a new session shifts the list down', () => {
    const shown = [{ id: 'c' }, { id: 'b' }];
    expect(appendPage(shown, [{ id: 'b' }, { id: 'a' }]).map((s) => s.id)).toEqual(['c', 'b', 'a']);
  });
});

describe('a paged list with loads racing each other', () => {
  type Row = { id: string };
  const rows = (...ids: string[]): Row[] => ids.map((id) => ({ id }));

  /** A load the test settles by hand, in whatever order it wants. */
  function deferred<T>() {
    let resolve!: (value: T) => void;
    let reject!: (e: Error) => void;
    const promise = new Promise<T>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    return { promise, resolve, reject };
  }

  /** Lets a settled load's handlers run. */
  const settled = () => new Promise((done) => setTimeout(done));

  /** The list as a component holds it, driven the way Sidebar drives it. */
  function list(initial: Row[]) {
    const loads = new PagedLoads();
    const state = { shown: initial, error: null as string | null, pageReads: 0 };
    return {
      state,
      loads,
      reload(load: Promise<Row[]>) {
        loads.replace(
          load,
          (page) => {
            state.shown = page;
          },
          (e) => {
            state.error = e.message;
          },
        );
      },
      showMore(load: Promise<Row[]>) {
        loads.extend(
          () => {
            state.pageReads += 1;
            return load;
          },
          (page) => {
            state.shown = appendPage(state.shown, page);
          },
          (e) => {
            state.error = e.message;
          },
        );
      },
    };
  }

  it('cannot bring back a row a reload in flight is removing', async () => {
    const l = list(rows('a', 'b', 'c'));
    const reload = deferred<Row[]>();
    const page = deferred<Row[]>();

    l.reload(reload.promise); // b was just deleted
    l.showMore(page.promise);
    reload.resolve(rows('a', 'c'));
    await settled();
    page.resolve(rows('d'));
    await settled();

    expect(l.state.shown).toEqual(rows('a', 'c'));
    expect(l.state.pageReads).toBe(0);
  });

  it('drops a page when a reload starts after it', async () => {
    const l = list(rows('a', 'b', 'c'));
    const page = deferred<Row[]>();
    const reload = deferred<Row[]>();

    l.showMore(page.promise);
    l.reload(reload.promise);
    reload.resolve(rows('a', 'c'));
    await settled();
    page.resolve(rows('d'));
    await settled();

    expect(l.state.shown).toEqual(rows('a', 'c'));
  });

  it('drops a late page even if the reload that overtook it landed last', async () => {
    const l = list(rows('a', 'b', 'c'));
    const page = deferred<Row[]>();
    const reload = deferred<Row[]>();

    l.showMore(page.promise);
    l.reload(reload.promise);
    page.resolve(rows('d'));
    await settled();
    expect(l.state.shown).toEqual(rows('a', 'b', 'c'));

    reload.resolve(rows('a', 'c'));
    await settled();
    expect(l.state.shown).toEqual(rows('a', 'c'));
  });

  it('adds the page when nothing else moved', async () => {
    const l = list(rows('a'));
    l.showMore(Promise.resolve(rows('b')));
    await settled();
    expect(l.state.shown).toEqual(rows('a', 'b'));
  });

  it('takes the next Show more once the reload has landed', async () => {
    const l = list(rows('a', 'b'));
    l.reload(Promise.resolve(rows('a')));
    await settled();
    l.showMore(Promise.resolve(rows('c')));
    await settled();
    expect(l.state.shown).toEqual(rows('a', 'c'));
  });

  it('runs one Show more at a time', async () => {
    const l = list(rows('a'));
    const page = deferred<Row[]>();
    l.showMore(page.promise);
    l.showMore(Promise.resolve(rows('x')));
    expect(l.state.pageReads).toBe(1);

    page.resolve(rows('b'));
    await settled();
    expect(l.state.shown).toEqual(rows('a', 'b'));
  });

  it('lets only the newest reload land', async () => {
    const l = list([]);
    const older = deferred<Row[]>();
    const newer = deferred<Row[]>();

    l.reload(older.promise);
    l.reload(newer.promise);
    newer.resolve(rows('new'));
    await settled();
    older.resolve(rows('old'));
    await settled();

    expect(l.state.shown).toEqual(rows('new'));
  });

  it('keeps refusing a Show more until the newest reload lands', async () => {
    const l = list(rows('a'));
    const older = deferred<Row[]>();
    const newer = deferred<Row[]>();

    l.reload(older.promise);
    l.reload(newer.promise);
    older.resolve(rows('old'));
    await settled();
    l.showMore(Promise.resolve(rows('x')));

    expect(l.state.pageReads).toBe(0);
  });

  it('reports a failed reload and takes a Show more after it', async () => {
    const l = list(rows('a'));
    l.reload(Promise.reject(new Error('offline')));
    await settled();
    expect(l.state.error).toBe('offline');

    l.showMore(Promise.resolve(rows('b')));
    await settled();
    expect(l.state.shown).toEqual(rows('a', 'b'));
  });

  it('says nothing about a load that was overtaken and then failed', async () => {
    const l = list(rows('a'));
    const page = deferred<Row[]>();

    l.showMore(page.promise);
    l.reload(Promise.resolve(rows('a')));
    page.reject(new Error('offline'));
    await settled();

    expect(l.state.error).toBeNull();
  });

  it('drops anything in flight when the rows change by other means', async () => {
    // Deleting a stored question, or moving to another session.
    const l = list(rows('a', 'b'));
    const page = deferred<Row[]>();

    l.showMore(page.promise);
    l.loads.cancel();
    page.resolve(rows('c'));
    await settled();

    expect(l.state.shown).toEqual(rows('a', 'b'));
    l.showMore(Promise.resolve(rows('d')));
    await settled();
    expect(l.state.shown).toEqual(rows('a', 'b', 'd'));
  });
});
