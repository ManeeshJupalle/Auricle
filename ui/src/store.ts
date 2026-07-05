import { create } from 'zustand';
import type { SegmentInfo, WsEvent } from './types';

// Normalized transcript state, two layers:
//
//   rows/order   — one entry per STT segment (the unit partials update)
//   turns/turnOrder — consecutive same-channel segments grouped into
//                     speaker turns (the unit the document layout renders)
//
// The single-row re-render property from Phase 5 survives grouping because
// a partial's text growth mutates ONLY rows[id]; the turn that contains it
// (and turnOrder) keep their object identity, so the virtualized turn
// shells never re-render on partial updates — just the one segment span.

export interface Row {
  id: string;
  channel: string;
  speaker: string;
  tStartMs: number;
  tEndMs: number;
  text: string;
  final: boolean;
}

export interface Turn {
  id: string;
  channel: string;
  speaker: string;
  tStartMs: number;
  segIds: string[];
}

export type ConnState = 'connecting' | 'open' | 'reconnecting';

interface AuricleState {
  conn: ConnState;
  engineState: 'idle' | 'recording' | 'stopping';
  activeSession: string | null;
  sessionTitle: string | null;
  sessionProvider: string | null;
  liveStartedAtMs: number | null;

  order: string[];
  rows: Record<string, Row>;
  partialRowByChannel: Record<string, string | undefined>;
  turnOrder: string[];
  turns: Record<string, Turn>;

  vu: Record<string, number>;
  latencyMs: number | null;
  deviceLost: string | null;
  lastError: string | null;
  droppedPartials: number;
  /** Bumped whenever the session list may have changed (start/stop/retitle). */
  sessionsVersion: number;

  setConn: (c: ConnState) => void;
  setEngine: (state: AuricleState['engineState'], session: string | null) => void;
  applyEvent: (ev: WsEvent) => void;
  /** Rebuild the transcript from persisted finals (reconnect resync). */
  loadFinals: (segments: SegmentInfo[]) => void;
  resetLive: () => void;
}

const finalRowId = (channel: string, tStartMs: number) => `f-${channel}-${tStartMs}`;
const channelName = (code: number) => (code === 0 ? 'mic' : 'loopback');

let turnCounter = 0;
const nextTurnId = () => `t${++turnCounter}`;

/** Pure grouping used by the stored-session view (and loadFinals). */
export function buildTurns(
  order: string[],
  rows: Record<string, Row>,
): { turnOrder: string[]; turns: Record<string, Turn> } {
  const turnOrder: string[] = [];
  const turns: Record<string, Turn> = {};
  let current: Turn | null = null;
  for (const id of order) {
    const row = rows[id];
    if (!row) continue;
    if (current && current.channel === row.channel) {
      current.segIds.push(id);
    } else {
      current = {
        id: `t-${turnOrder.length}`,
        channel: row.channel,
        speaker: row.speaker,
        tStartMs: row.tStartMs,
        segIds: [id],
      };
      turnOrder.push(current.id);
      turns[current.id] = current;
    }
  }
  return { turnOrder, turns };
}

/** Append a new row id to the turn layers (mutating copies, not state). */
function appendToTurns(
  state: Pick<AuricleState, 'turnOrder' | 'turns'>,
  row: Row,
): Pick<AuricleState, 'turnOrder' | 'turns'> {
  const lastTurnId = state.turnOrder[state.turnOrder.length - 1];
  const lastTurn = lastTurnId ? state.turns[lastTurnId] : undefined;
  if (lastTurn && lastTurn.channel === row.channel) {
    return {
      turnOrder: state.turnOrder,
      turns: {
        ...state.turns,
        [lastTurn.id]: { ...lastTurn, segIds: [...lastTurn.segIds, row.id] },
      },
    };
  }
  const turn: Turn = {
    id: nextTurnId(),
    channel: row.channel,
    speaker: row.speaker,
    tStartMs: row.tStartMs,
    segIds: [row.id],
  };
  return {
    turnOrder: [...state.turnOrder, turn.id],
    turns: { ...state.turns, [turn.id]: turn },
  };
}

export const useAuricle = create<AuricleState>((set, get) => ({
  conn: 'connecting',
  engineState: 'idle',
  activeSession: null,
  sessionTitle: null,
  sessionProvider: null,
  liveStartedAtMs: null,
  order: [],
  rows: {},
  partialRowByChannel: {},
  turnOrder: [],
  turns: {},
  vu: {},
  latencyMs: null,
  deviceLost: null,
  lastError: null,
  droppedPartials: 0,
  sessionsVersion: 0,

  setConn: (conn) => set({ conn }),

  setEngine: (engineState, activeSession) => set({ engineState, activeSession }),

  applyEvent: (ev) => {
    switch (ev.type) {
      case 'partial': {
        const { rows, partialRowByChannel, order, turnOrder, turns } = get();
        const existing = partialRowByChannel[ev.channel];
        if (existing && rows[existing] && !rows[existing].final) {
          // In-place growth: rows only. Turn identity untouched — this is
          // the single-row re-render path.
          set({
            rows: {
              ...rows,
              [existing]: { ...rows[existing], text: ev.text, tEndMs: ev.t_end_ms },
            },
          });
        } else {
          const row: Row = {
            id: `p-${ev.channel}-${ev.t_start_ms}-${order.length}`,
            channel: ev.channel,
            speaker: ev.speaker,
            tStartMs: ev.t_start_ms,
            tEndMs: ev.t_end_ms,
            text: ev.text,
            final: false,
          };
          set({
            rows: { ...rows, [row.id]: row },
            order: [...order, row.id],
            partialRowByChannel: { ...partialRowByChannel, [ev.channel]: row.id },
            ...appendToTurns({ turnOrder, turns }, row),
          });
        }
        break;
      }
      case 'final': {
        const { rows, partialRowByChannel, order, turnOrder, turns } = get();
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
        const isNewRow = !rows[row.id];
        set({
          rows: { ...rows, [row.id]: row },
          order: isNewRow ? [...order, row.id] : order,
          partialRowByChannel: { ...partialRowByChannel, [ev.channel]: undefined },
          latencyMs: ev.latency_ms ?? get().latencyMs,
          // A flipped partial already sits in its turn; only new rows join.
          ...(isNewRow ? appendToTurns({ turnOrder, turns }, row) : {}),
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
          liveStartedAtMs: Date.now(),
          sessionsVersion: get().sessionsVersion + 1,
        });
        break;
      case 'session_stopped':
        set({
          engineState: 'idle',
          activeSession: null,
          vu: {},
          partialRowByChannel: {},
          sessionsVersion: get().sessionsVersion + 1,
        });
        break;
      case 'session_updated':
        // Currently: the post-stop auto-title landed.
        set({ sessionsVersion: get().sessionsVersion + 1 });
        break;
      case 'device_lost':
        set({ deviceLost: `${ev.channel}: ${ev.message}` });
        break;
      case 'error':
        set({ lastError: ev.message });
        break;
      case 'lag':
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
    set({ rows, order, partialRowByChannel: {}, ...buildTurns(order, rows) });
  },

  resetLive: () =>
    set({
      order: [],
      rows: {},
      partialRowByChannel: {},
      turnOrder: [],
      turns: {},
      vu: {},
      latencyMs: null,
      deviceLost: null,
      lastError: null,
      droppedPartials: 0,
      liveStartedAtMs: null,
    }),
}));
