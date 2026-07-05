import { create } from 'zustand';
import type { SegmentInfo, WsEvent } from './types';

// Normalized transcript state: `order` holds row ids in display order,
// `rows` holds the row data. A partial updates rows[id] only — components
// subscribe per-row, so exactly one row re-renders; `order` changes only
// when a row is appended. Finals reuse the partial's row id when one is
// live (the row "flips" in place), so the two-lane WS policy (lossy
// partials, guaranteed finals) can never orphan or duplicate a row.

export interface Row {
  id: string;
  channel: string;
  speaker: string;
  tStartMs: number;
  tEndMs: number;
  text: string;
  final: boolean;
}

export type ConnState = 'connecting' | 'open' | 'reconnecting';

interface AuricleState {
  conn: ConnState;
  engineState: 'idle' | 'recording' | 'stopping';
  activeSession: string | null;
  sessionTitle: string | null;
  sessionProvider: string | null;

  order: string[];
  rows: Record<string, Row>;
  partialRowByChannel: Record<string, string | undefined>;

  vu: Record<string, number>;
  latencyMs: number | null;
  deviceLost: string | null;
  lastError: string | null;
  droppedPartials: number;

  setConn: (c: ConnState) => void;
  setEngine: (state: AuricleState['engineState'], session: string | null) => void;
  applyEvent: (ev: WsEvent) => void;
  /** Rebuild the transcript from persisted finals (reconnect resync). */
  loadFinals: (segments: SegmentInfo[]) => void;
  resetLive: () => void;
}

const finalRowId = (channel: string, tStartMs: number) => `f-${channel}-${tStartMs}`;
const channelName = (code: number) => (code === 0 ? 'mic' : 'loopback');

export const useAuricle = create<AuricleState>((set, get) => ({
  conn: 'connecting',
  engineState: 'idle',
  activeSession: null,
  sessionTitle: null,
  sessionProvider: null,
  order: [],
  rows: {},
  partialRowByChannel: {},
  vu: {},
  latencyMs: null,
  deviceLost: null,
  lastError: null,
  droppedPartials: 0,

  setConn: (conn) => set({ conn }),

  setEngine: (engineState, activeSession) => set({ engineState, activeSession }),

  applyEvent: (ev) => {
    switch (ev.type) {
      case 'partial': {
        const { rows, partialRowByChannel, order } = get();
        const existing = partialRowByChannel[ev.channel];
        if (existing && rows[existing] && !rows[existing].final) {
          // The single-row in-place update: only rows changes, order intact.
          set({
            rows: {
              ...rows,
              [existing]: {
                ...rows[existing],
                text: ev.text,
                tEndMs: ev.t_end_ms,
              },
            },
          });
        } else {
          const id = `p-${ev.channel}-${ev.t_start_ms}-${order.length}`;
          set({
            rows: {
              ...rows,
              [id]: {
                id,
                channel: ev.channel,
                speaker: ev.speaker,
                tStartMs: ev.t_start_ms,
                tEndMs: ev.t_end_ms,
                text: ev.text,
                final: false,
              },
            },
            order: [...order, id],
            partialRowByChannel: { ...partialRowByChannel, [ev.channel]: id },
          });
        }
        break;
      }
      case 'final': {
        const { rows, partialRowByChannel, order } = get();
        const partialId = partialRowByChannel[ev.channel];
        const row: Row = {
          id: partialId ?? finalRowId(ev.channel, ev.t_start_ms),
          channel: ev.channel,
          speaker: ev.speaker,
          tStartMs: ev.t_start_ms,
          tEndMs: ev.t_end_ms,
          text: ev.text,
          final: true,
        };
        set({
          rows: { ...rows, [row.id]: row },
          // Append only when this id is new (partial flip and resync
          // replays keep their position).
          order: rows[row.id] ? order : [...order, row.id],
          partialRowByChannel: { ...partialRowByChannel, [ev.channel]: undefined },
          latencyMs: ev.latency_ms ?? get().latencyMs,
        });
        break;
      }
      case 'vu':
        set({ vu: { ...get().vu, [ev.channel]: ev.rms } });
        break;
      case 'session_started':
        get().resetLive();
        set({
          engineState: 'recording',
          activeSession: ev.session,
          sessionTitle: ev.title,
          sessionProvider: ev.stt_provider,
        });
        break;
      case 'session_stopped':
        // Transcript stays on screen; live-session state clears.
        set({
          engineState: 'idle',
          activeSession: null,
          vu: {},
          partialRowByChannel: {},
        });
        break;
      case 'device_lost':
        set({ deviceLost: `${ev.channel}: ${ev.message}` });
        break;
      case 'error':
        set({ lastError: ev.message });
        break;
      case 'lag':
        // Lossy partial lane: some partials were shed for this consumer.
        // Finals still arrive, so the transcript stays correct.
        set({ droppedPartials: ev.dropped_partials });
        break;
    }
  },

  loadFinals: (segments) => {
    const rows: Record<string, Row> = {};
    const order: string[] = [];
    for (const seg of segments) {
      const channel = channelName(seg.channel);
      const id = finalRowId(channel, seg.t_start_ms);
      if (!rows[id]) order.push(id);
      rows[id] = {
        id,
        channel,
        speaker: seg.speaker,
        tStartMs: seg.t_start_ms,
        tEndMs: seg.t_end_ms,
        text: seg.text,
        final: true,
      };
    }
    set({ rows, order, partialRowByChannel: {} });
  },

  resetLive: () =>
    set({
      order: [],
      rows: {},
      partialRowByChannel: {},
      vu: {},
      latencyMs: null,
      deviceLost: null,
      lastError: null,
      droppedPartials: 0,
    }),
}));
