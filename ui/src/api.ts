// Named fetch on purpose: no request in this file can skip the token.
import { authFetch as fetch, withToken } from './auth';
import type {
  AskInfo,
  DeviceInfo,
  DiagnosticsInfo,
  EgressLedger,
  Health,
  ProvidersResponse,
  SearchPage,
  SessionDetail,
  SessionPage,
  SessionSummary,
  Settings,
  SummaryInfo,
} from './types';

async function check(resp: Response): Promise<Response> {
  if (!resp.ok) {
    let detail = `${resp.status}`;
    try {
      const body = (await resp.json()) as { error?: string };
      if (body.error) detail = body.error;
    } catch {
      // non-JSON error body
    }
    throw new Error(detail);
  }
  return resp;
}

async function json<T>(resp: Response): Promise<T> {
  await check(resp);
  return resp.json() as Promise<T>;
}

/** Add a later page, minus any row a newer one pushed back onto it. */
export function appendPage<T extends { id: unknown }>(shown: T[], page: T[]): T[] {
  const seen = new Set(shown.map((row) => row.id));
  return [...shown, ...page.filter((row) => !seen.has(row.id))];
}

/**
 * Keeps the loads behind a paged list from landing on each other. A load that
 * replaces the list overtakes anything in flight. One that extends it builds
 * on the rows on show, so it starts only when nothing is in flight: with a
 * reload on its way, those rows may still hold one that reload is removing.
 */
export class PagedLoads {
  private latest = 0;
  private busy = false;

  replace<T>(load: Promise<T>, land: (value: T) => void, fail: (e: Error) => void): void {
    this.latest += 1;
    const mine = this.latest;
    this.busy = true;
    const settle = (finish: () => void) => {
      if (mine !== this.latest) return;
      this.busy = false;
      finish();
    };
    load.then(
      (value) => settle(() => land(value)),
      (e: Error) => settle(() => fail(e)),
    );
  }

  /** `load` runs only if this goes ahead, so a refused Show more costs nothing. */
  extend<T>(load: () => Promise<T>, land: (value: T) => void, fail: (e: Error) => void): void {
    if (!this.busy) this.replace(load(), land, fail);
  }

  /** The rows changed some other way: nothing in flight may land on them. */
  cancel(): void {
    this.latest += 1;
    this.busy = false;
  }
}

export const api = {
  health: () => fetch('/api/v1/health').then((r) => json<Health>(r)),

  devices: () =>
    fetch('/api/v1/devices')
      .then((r) => json<{ devices: DeviceInfo[] }>(r))
      .then((b) => b.devices),

  providers: () => fetch('/api/v1/providers').then((r) => json<ProvidersResponse>(r)),

  summarize: (id: string, body: { template: string; provider: string }) =>
    fetch(`/api/v1/sessions/${encodeURIComponent(id)}/summarize`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    }).then((r) => json<SummaryInfo & { session: string; provider: string }>(r)),

  sessions: (q?: string) =>
    fetch(`/api/v1/sessions${q?.trim() ? `?q=${encodeURIComponent(q.trim())}` : ''}`)
      .then((r) => json<{ sessions: SessionSummary[] }>(r))
      .then((b) => b.sessions),

  search: (q: string, limit: number, offset: number) => {
    const params = new URLSearchParams({ q, limit: String(limit), offset: String(offset) });
    return fetch(`/api/v1/search?${params}`).then((r) => json<SearchPage>(r));
  },

  sessionPage: (limit: number, offset: number) =>
    fetch(`/api/v1/sessions?limit=${limit}&offset=${offset}`).then((r) => json<SessionPage>(r)),

  renameSession: (id: string, title: string) =>
    fetch(`/api/v1/sessions/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ title }),
    }).then((r) => json<{ id: string; title: string }>(r)),

  deleteSession: (id: string) =>
    fetch(`/api/v1/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' }).then((r) =>
      json<{ id: string; deleted: boolean }>(r),
    ),

  // In token mode the token rides in this URL, so a failed load can print it to
  // this browser's console. <audio> can't set a header, and nothing on this
  // side can stand in for one on files hours long: a blob holds the whole file
  // in memory, MediaSource refuses WAV, and a service worker needs a secure
  // context, which a token bind on plain HTTP is not. The player only mounts
  // after a bearer read of the session has worked, so what is left is a token
  // revoked while the page is open. Closing that needs the daemon to accept a
  // cookie or a short-lived ticket.
  audioUrl: (id: string, channel: 'mic' | 'loopback') =>
    withToken(`/api/v1/sessions/${encodeURIComponent(id)}/audio/${channel}`),

  session: (id: string) =>
    fetch(`/api/v1/sessions/${encodeURIComponent(id)}`).then((r) => json<SessionDetail>(r)),

  startSession: (body: { title?: string; stt_provider?: string }) =>
    fetch('/api/v1/sessions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    }).then((r) => json<{ id: string }>(r)),

  stopSession: (id: string) =>
    fetch(`/api/v1/sessions/${encodeURIComponent(id)}/stop`, { method: 'POST' }).then((r) =>
      json<{ id: string; status: string }>(r),
    ),

  settings: () => fetch('/api/v1/settings').then((r) => json<Settings>(r)),

  putSettings: (patch: Settings) =>
    fetch('/api/v1/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(patch),
    }).then((r) => json<Settings>(r)),

  // Store an API key / token in the OS credential store. The value is
  // never read back — readiness comes from GET /api/v1/providers.
  putSecret: (provider: string, value: string) =>
    fetch(`/api/v1/secrets/${encodeURIComponent(provider)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ value }),
    }).then((r) => json<{ id: string; stored: boolean }>(r)),

  deleteSecret: (provider: string) =>
    fetch(`/api/v1/secrets/${encodeURIComponent(provider)}`, { method: 'DELETE' }).then((r) =>
      json<{ id: string; deleted: boolean }>(r),
    ),

  egress: (session?: string, limit?: number, offset?: number) => {
    const q = new URLSearchParams();
    if (session) q.set('session', session);
    if (limit) q.set('limit', String(limit));
    if (offset) q.set('offset', String(offset));
    const query = q.toString();
    return fetch(`/api/v1/egress${query ? `?${query}` : ''}`).then((r) => json<EgressLedger>(r));
  },

  diagnostics: () => fetch('/api/v1/diagnostics').then((r) => json<DiagnosticsInfo>(r)),

  asks: (session: string, limit: number, offset: number) => {
    const q = new URLSearchParams({
      session,
      limit: String(limit),
      offset: String(offset),
    });
    return fetch(`/api/v1/asks?${q}`)
      .then((r) => json<{ asks: AskInfo[] }>(r))
      .then((b) => b.asks);
  },

  deleteAsk: (id: number) =>
    fetch(`/api/v1/asks/${id}`, { method: 'DELETE' })
      .then(check)
      .then(() => undefined),

  backup: () => fetch('/api/v1/backup', { method: 'POST' }).then((r) => json<{ path: string }>(r)),

  exportUrl: (id: string) => `/api/v1/sessions/${encodeURIComponent(id)}/export?format=md`,

  // The export route takes no query token, so with one held a plain link
  // can't reach it.
  exportBlob: (id: string) =>
    fetch(api.exportUrl(id))
      .then(check)
      .then((r) => r.blob()),
};
