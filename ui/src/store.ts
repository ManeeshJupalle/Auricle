import { create } from 'zustand';
import type { Health, SegmentInfo, SessionDetail, WsEvent } from './types';

// Normalized transcript state, two layers:
//
//   rows/order   — one entry per STT segment (the unit partials update)
//   turns/turnOrder — consecutive same-channel segments grouped into
//                     speaker turns (the unit the document layout renders),
//                     cut at long pauses and at a length limit
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

export interface ModelDownload {
  doneMb: number;
  sizeMb: number;
}

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
  /** Transient status line, e.g. a first-run model download blocking start. */
  notice: string | null;
  /**
   * How far that download has got. Apart from `notice` on purpose: screen
   * readers hear the notice, and a number changing every second would talk
   * over the meeting.
   */
  download: ModelDownload | null;
  /**
   * A start held up by that download was called off. The download itself runs
   * on, so the notice stays until the engine lets go of the session.
   */
  startCancelled: boolean;
  droppedPartials: number;
  /** Bumped whenever the session list may have changed (start/stop/retitle). */
  sessionsVersion: number;
  /** Bumped after a settings or API-key change; the sidebar outlives that view. */
  settingsVersion: number;
  /**
   * Summary picks live here because SessionView remounts per session: a local
   * LLM chosen for one session must not fall back to the default on the next.
   * Null means no pick, so the default provider applies.
   */
  summaryTemplate: string;
  summaryProvider: string | null;
  /** Why the token prompt is open: no token yet, or one the daemon refused. */
  tokenPrompt: 'required' | 'rejected' | null;

  setConn: (c: ConnState) => void;
  applyEvent: (ev: WsEvent) => void;
  /** The stop was accepted. A no-op once session_stopped has beaten the response. */
  markStopping: (session: string) => void;
  /** Rebuild the transcript from persisted finals (reconnect resync). */
  loadFinals: (segments: SegmentInfo[]) => void;
  /** Hold socket events back until endResync, so a snapshot can't overwrite them. */
  beginResync: () => void;
  /** Apply a REST snapshot (null if the fetch failed), then replay held events. */
  endResync: (snapshot: { health: Health; detail: SessionDetail | null } | null) => void;
  resetLive: () => void;
  settingsChanged: () => void;
}

const finalRowId = (channel: string, tStartMs: number) => `f-${channel}-${tStartMs}`;
const channelName = (code: number) => (code === 0 ? 'mic' : 'loopback');

// Events held back while a resync is in flight, or null when none is. Not
// state: nothing renders it.
let resyncBuffer: WsEvent[] | null = null;

let turnCounter = 0;
const nextTurnId = () => `t${++turnCounter}`;

/** A pause this long ends a turn even when nobody else speaks. */
export const TURN_GAP_MS = 5_000;
/**
 * A turn takes no new segment this long after it began. Without a limit a
 * one-sided session (a two-hour webinar) is one DOM item with one timestamp
 * and one block on the session map.
 */
export const TURN_MAX_MS = 60_000;

/**
 * Whether `row` extends `turn`, whose latest row is `prev`. Reads only the
 * start of `row`, so a live row is placed correctly from its first partial,
 * and live and stored grouping agree.
 */
function continuesTurn(turn: Turn, prev: Row | undefined, row: Row): boolean {
  return (
    turn.channel === row.channel &&
    (!prev || row.tStartMs - prev.tEndMs < TURN_GAP_MS) &&
    row.tStartMs - turn.tStartMs < TURN_MAX_MS
  );
}

/** Pure grouping used by the stored-session view (and loadFinals). */
export function buildTurns(
  order: string[],
  rows: Record<string, Row>,
): { turnOrder: string[]; turns: Record<string, Turn> } {
  const turnOrder: string[] = [];
  const turns: Record<string, Turn> = {};
  let current: Turn | null = null;
  let prev: Row | undefined;
  for (const id of order) {
    const row = rows[id];
    if (!row) continue;
    if (current && continuesTurn(current, prev, row)) {
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
    prev = row;
  }
  return { turnOrder, turns };
}

/** Append a new row id to the turn layers (mutating copies, not state). */
function appendToTurns(
  state: Pick<AuricleState, 'turnOrder' | 'turns' | 'rows'>,
  row: Row,
): Pick<AuricleState, 'turnOrder' | 'turns'> {
  const lastTurnId = state.turnOrder[state.turnOrder.length - 1];
  const lastTurn = lastTurnId ? state.turns[lastTurnId] : undefined;
  const prev = lastTurn && state.rows[lastTurn.segIds[lastTurn.segIds.length - 1]];
  if (lastTurn && continuesTurn(lastTurn, prev, row)) {
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

/**
 * The screen-reader status line: recording state and problems only. Transcript
 * text stays out, or a reader would talk over the meeting word by word. The
 * initial value is never spoken, only changes are, so "Not recording." is
 * heard once a session ends rather than at page load.
 */
export function liveStatus(s: AuricleState): string {
  const engine = {
    idle: 'Not recording.',
    recording: 'Recording.',
    stopping: 'Stopping recording.',
  }[s.engineState];
  return [
    s.conn === 'reconnecting' && 'Connection lost, reconnecting.',
    engine,
    s.notice,
    s.activeSession && s.deviceLost && `Audio device lost: ${s.deviceLost}.`,
    s.activeSession && s.lastError && `Error: ${s.lastError}.`,
  ]
    .filter(Boolean)
    .join(' ');
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
  notice: null,
  download: null,
  startCancelled: false,
  droppedPartials: 0,
  sessionsVersion: 0,
  settingsVersion: 0,
  summaryTemplate: 'minutes',
  summaryProvider: null,
  tokenPrompt: null,

  setConn: (conn) => set({ conn }),

  applyEvent: (ev) => {
    if (resyncBuffer) {
      resyncBuffer.push(ev);
      return;
    }
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
            ...appendToTurns({ turnOrder, turns, rows }, row),
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
          ...(isNewRow ? appendToTurns({ turnOrder, turns, rows }, row) : {}),
        });
        break;
      }
      case 'partial_cleared': {
        // The engine dropped this line's final as speaker bleed, so no final
        // is coming to flip the row. Rare, so a full turn rebuild is fine.
        const { rows, order, partialRowByChannel } = get();
        const id = partialRowByChannel[ev.channel];
        if (!id) break;
        const kept = { ...rows };
        delete kept[id];
        const keptOrder = order.filter((rowId) => rowId !== id);
        set({
          rows: kept,
          order: keptOrder,
          partialRowByChannel: { ...partialRowByChannel, [ev.channel]: undefined },
          ...buildTurns(keptOrder, kept),
        });
        break;
      }
      case 'vu':
        set({ vu: { ...get().vu, [ev.channel]: ev.rms } });
        break;
      case 'model_download_started':
        // The start request is blocked on a first-run model download;
        // without this the Start button is a silent multi-minute hang.
        set({
          notice: `Downloading the ${ev.model} speech model, ${ev.size_mb} MB.`,
          download: { doneMb: 0, sizeMb: ev.size_mb },
        });
        break;
      case 'model_download_progress':
        set({
          download: { doneMb: ev.done_mb, sizeMb: ev.size_mb },
          // Heard once, when the step changes. A client that joined mid-way
          // never saw the start, so this may be its first word of it.
          notice:
            ev.done_mb >= ev.size_mb
              ? `Checking the ${ev.model} speech model.`
              : (get().notice ?? `Downloading the ${ev.model} speech model, ${ev.size_mb} MB.`),
        });
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
          notice: null,
          download: null,
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

  markStopping: (session) => {
    if (get().activeSession === session) set({ engineState: 'stopping' });
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

  beginResync: () => {
    resyncBuffer = [];
  },

  endResync: (snapshot) => {
    const held = resyncBuffer ?? [];
    resyncBuffer = null;
    if (snapshot) {
      const { health, detail } = snapshot;
      // Another session started while we were away: its banners aren't ours.
      if (detail && detail.id !== get().activeSession) get().resetLive();
      set({
        engineState: health.state,
        activeSession: health.active_session,
        sessionsVersion: get().sessionsVersion + 1,
      });
      if (detail) {
        set({
          sessionTitle: detail.title,
          sessionProvider: detail.stt_provider,
          liveStartedAtMs: detail.started_at * 1000,
        });
        get().loadFinals(detail.transcript);
      }
    }
    // Held partials are dropped: one could reopen a segment the snapshot
    // already has as final, and the next partial carries the text anyway.
    for (const ev of held) {
      if (ev.type !== 'partial') get().applyEvent(ev);
    }
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
      notice: null,
      download: null,
      droppedPartials: 0,
      liveStartedAtMs: null,
    }),

  settingsChanged: () =>
    set({ settingsVersion: get().settingsVersion + 1, summaryProvider: null }),
}));
