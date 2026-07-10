// Wire types, matching docs/API.md (real captured payloads).

export type WsEvent =
  | {
      type: 'partial' | 'final';
      session: string;
      channel: string;
      speaker: string;
      t_start_ms: number;
      t_end_ms: number;
      text: string;
      latency_ms?: number;
    }
  | { type: 'vu'; session: string; channel: string; rms: number }
  | { type: 'model_download_started'; session: string; model: string; size_mb: number }
  | { type: 'session_started'; session: string; title: string; stt_provider: string }
  | { type: 'session_stopped'; session: string }
  | { type: 'session_updated'; session: string; title: string }
  | { type: 'device_lost'; session: string; channel: string; message: string }
  | { type: 'error'; session: string; message: string }
  | { type: 'lag'; dropped_partials: number };

export interface Health {
  status: string;
  version: string;
  state: 'idle' | 'recording' | 'stopping';
  active_session: string | null;
}

export interface DeviceInfo {
  name: string;
  kind: 'input' | 'loopback';
  is_default: boolean;
  sample_rate_hz: number;
  channels: number;
  sample_format: string;
}

export interface ProviderInfo {
  id: string;
  kind: string;
  ready: boolean;
  detail: string;
  default: boolean;
}

export interface SegmentInfo {
  channel: number;
  speaker: string;
  t_start_ms: number;
  t_end_ms: number;
  text: string;
  provider: string;
}

export interface SessionSummary {
  id: string;
  title: string;
  started_at: number;
  ended_at: number | null;
  stt_provider: string;
  meta: Record<string, unknown>;
}

export interface SummaryInfo {
  id: number;
  template: string;
  model: string;
  content: string;
  created_at: number;
}

export interface SessionDetail extends SessionSummary {
  transcript: SegmentInfo[];
  summaries: SummaryInfo[];
}

export interface LlmProviderInfo {
  id: string;
  model: string;
  ready: boolean;
  detail: string;
  default: boolean;
}

export interface TemplateInfo {
  name: string;
  overridden: boolean;
}

export interface ProvidersResponse {
  providers: ProviderInfo[];
  llm: LlmProviderInfo[];
  templates: TemplateInfo[];
}

export type Settings = Record<string, unknown>;
