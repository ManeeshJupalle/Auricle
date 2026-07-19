import type {
  DeviceInfo,
  Health,
  ProvidersResponse,
  SessionDetail,
  SessionSummary,
  Settings,
  SummaryInfo,
} from './types';

async function json<T>(resp: Response): Promise<T> {
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
  return resp.json() as Promise<T>;
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

  audioUrl: (id: string, channel: 'mic' | 'loopback') =>
    `/api/v1/sessions/${encodeURIComponent(id)}/audio/${channel}`,

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

  exportUrl: (id: string) => `/api/v1/sessions/${encodeURIComponent(id)}/export?format=md`,
};
