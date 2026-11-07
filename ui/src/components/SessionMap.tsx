import {
  forwardRef,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from 'react';
import type { Row, Turn } from '../store';

/** mm:ss for a session-relative offset. */
function fmt(ms: number): string {
  const m = Math.floor(ms / 60_000);
  const s = Math.floor(ms / 1000) % 60;
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
}

export interface MapTurn {
  index: number;
  channel: string;
  speaker: string;
  startMs: number;
  endMs: number;
}

/** Turn layer → positioned segments, with each turn's end from its last row. */
export function buildMapTurns(
  turnOrder: string[],
  turns: Record<string, Turn>,
  rows: Record<string, Row>,
): MapTurn[] {
  return turnOrder.map((id, index) => {
    const turn = turns[id];
    const last = turn.segIds[turn.segIds.length - 1];
    return {
      index,
      channel: turn.channel,
      speaker: turn.speaker,
      startMs: turn.tStartMs,
      endMs: rows[last]?.tEndMs ?? turn.tStartMs,
    };
  });
}

/**
 * The narrowest block, as a share of the track. A share and not pixels, so the
 * layout below holds at any window width.
 */
export const MIN_BLOCK_PCT = 0.5;

export interface MapBlock {
  /** Where the block jumps: the first turn merged into it. */
  turn: MapTurn;
  /** How many turns it stands for. */
  turns: number;
  leftPct: number;
  widthPct: number;
}

/**
 * Turns → drawn blocks. A turn that would land on the previous block of its
 * own voice joins that block instead, so no two overlap and a long session
 * never draws more than 100 / MIN_BLOCK_PCT blocks per voice. The two voices
 * sit on opposite sides of the centerline, so they never merge. Blocks stay in
 * time order, which is the order the arrow keys walk.
 */
export function layoutMap(turns: MapTurn[], durationMs: number): MapBlock[] {
  const minMs = (durationMs * MIN_BLOCK_PCT) / 100;
  const spans: { turn: MapTurn; turns: number; startMs: number; endMs: number }[] = [];
  const lastInLane = new Map<string, (typeof spans)[number]>();

  for (const turn of turns) {
    // The closing turn of a meeting starts right at the end of the track, so
    // shift it back to keep it wide enough to see.
    const startMs = Math.min(turn.startMs, durationMs - minMs);
    const last = lastInLane.get(turn.channel);
    if (last && startMs < last.endMs) {
      last.turns += 1;
      // The turn's real end, not its minimum width: padding here would chain
      // a whole stretch of short turns into a single block.
      last.endMs = Math.max(last.endMs, turn.endMs);
    } else {
      const span = { turn, turns: 1, startMs, endMs: Math.max(turn.endMs, startMs + minMs) };
      lastInLane.set(turn.channel, span);
      spans.push(span);
    }
  }

  return spans.map((s) => ({
    turn: s.turn,
    turns: s.turns,
    leftPct: (s.startMs / durationMs) * 100,
    widthPct: ((s.endMs - s.startMs) / durationMs) * 100,
  }));
}

/** The block a key moves focus to, or null for a key the map doesn't use. */
export function nextMapIndex(key: string, current: number, count: number): number | null {
  switch (key) {
    case 'ArrowRight':
    case 'ArrowDown':
      return Math.min(current + 1, count - 1);
    case 'ArrowLeft':
    case 'ArrowUp':
      return Math.max(current - 1, 0);
    case 'Home':
      return 0;
    case 'End':
      return count - 1;
    default:
      return null;
  }
}

/** The narrowest band that shows on the map, as a share of the track. */
const MIN_WINDOW_PCT = 1;

const SCALE_STEPS_MIN = [1, 2, 5, 10, 15, 30, 60, 120, 240];

/** The scale marks between the two ends: the finest step that leaves at most five. */
export function scaleMarks(durationMs: number): number[] {
  const step =
    (SCALE_STEPS_MIN.find((m) => durationMs / (m * 60_000) <= 6) ?? 480) * 60_000;
  const marks: number[] = [];
  // Stop short of the end, whose own label would collide with a last mark.
  for (let t = step; t < durationMs - step / 3; t += step) marks.push(t);
  return marks;
}

/**
 * How much of each block was actually speech, as a share of its length. A
 * block that stands for one turn is close to 1. In a long meeting most blocks
 * merge many turns, and this is what keeps a voice that talks from looking
 * like a voice that only says "yes".
 */
export function talkShares(blocks: MapBlock[], turns: MapTurn[], durationMs: number): number[] {
  const left = new Map<string, MapTurn[]>();
  for (const t of turns) {
    const lane = left.get(t.channel);
    if (lane) lane.push(t);
    else left.set(t.channel, [t]);
  }
  return blocks.map((b) => {
    const merged = left.get(b.turn.channel)?.splice(0, b.turns) ?? [];
    const talked = merged.reduce((sum, t) => sum + Math.max(0, t.endMs - t.startMs), 0);
    const spanMs = (b.widthPct / 100) * durationMs;
    return spanMs > 0 ? Math.min(1, talked / spanMs) : 0;
  });
}

/** A point on the scale, in the words a person would use. */
export function fmtScale(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)} s`;
  const mins = Math.round(ms / 60_000);
  if (mins < 60) return `${mins} min`;
  return mins % 60 === 0 ? `${mins / 60} h` : `${Math.floor(mins / 60)} h ${mins % 60} min`;
}

/** The turn under a moment: the last one to have begun by then, or -1 before the first. */
export function turnAt(turns: MapTurn[], ms: number): number {
  let found = -1;
  turns.forEach((t, i) => {
    if (t.startMs <= ms) found = i;
  });
  return found;
}

/** Where a block sits on the track. CSS reads these along whichever axis the map runs. */
const along = (atPct: number, spanPct?: number): CSSProperties =>
  ({ '--at': `${atPct}%`, '--span': spanPct === undefined ? undefined : `${spanPct}%` }) as CSSProperties;

/** A moment worth marking on the tape, on its speaker's side of the rule. */
export interface MapHit {
  ms: number;
  channel: string;
}

export interface SessionMapHandle {
  /**
   * Mark the stretch of the meeting the transcript has on screen. Writes
   * straight to the element, so scrolling never re-renders the blocks.
   */
  showWindow: (first: MapTurn | undefined, last: MapTurn | undefined) => void;
  /** Where playback is. Written the same way, so playing never re-renders them either. */
  showPlayhead: (ms: number) => void;
}

/**
 * The session map: who spoke when, across the whole recording. Them (system
 * audio) sits on one side of the rule, You (mic) on the other, the same tape
 * the live waveform writes while a meeting runs, frozen into the finished
 * session. It runs down the window beside the text, in the direction you
 * read, and lies across the page at phone width. It is navigation, not
 * decoration: click a block to jump the transcript there (and the audio, when
 * the recording was retained). From the keyboard the map is a single tab stop:
 * arrow keys walk the blocks, Enter jumps.
 */
export const SessionMap = forwardRef<
  SessionMapHandle,
  {
    turns: MapTurn[];
    durationMs: number;
    onPick: (turn: MapTurn) => void;
    /** Search hits, or the moments questions were asked. */
    hits?: MapHit[];
    /** Where the recording was cut off, when it didn't stop cleanly. */
    cutMs?: number;
    /** Where one side's device went away. That lane is dead from there on. */
    lost?: MapHit;
    /** Pick a moment anywhere on the line, not just on a block. */
    onSeek?: (ms: number) => void;
  }
>(function SessionMap({ turns, durationMs, onPick, hits = [], cutMs, lost, onSeek }, ref) {
  const blocks = useMemo(() => layoutMap(turns, durationMs), [turns, durationMs]);
  const shares = useMemo(
    () => talkShares(blocks, turns, durationMs),
    [blocks, turns, durationMs],
  );
  const [active, setActive] = useState(0);
  const buttons = useRef<(HTMLButtonElement | null)[]>([]);
  const reading = useRef<HTMLDivElement>(null);
  const playhead = useRef<HTMLDivElement>(null);

  useImperativeHandle(
    ref,
    () => ({
      showWindow: (first, last) => {
        const el = reading.current;
        if (!el || !first || !last || durationMs <= 0) return;
        const span = ((last.endMs - first.startMs) / durationMs) * 100;
        el.style.setProperty('--at', `${(first.startMs / durationMs) * 100}%`);
        el.style.setProperty('--span', `${Math.max(span, MIN_WINDOW_PCT)}%`);
        el.dataset.at = fmt(first.startMs);
      },
      showPlayhead: (ms) => {
        const el = playhead.current;
        if (!el || durationMs <= 0) return;
        el.classList.toggle('on', ms > 0);
        el.style.setProperty('--at', `${Math.min(100, (ms / durationMs) * 100)}%`);
        el.dataset.at = fmt(ms);
      },
    }),
    [durationMs],
  );

  if (turns.length === 0 || durationMs <= 0) return null;

  // A loss near the end would land on the end's own label, and matters more.
  const lostNearEnd = lost !== undefined && durationMs - lost.ms < durationMs * 0.06;

  // The transcript can reload with fewer blocks than the one last focused.
  const current = Math.min(active, blocks.length - 1);

  return (
    <div className="session-map">
      <div
        className="session-map-track"
        role="toolbar"
        aria-label="Session map"
        onClick={(e) => {
          if (!onSeek || e.target !== e.currentTarget) return;
          const box = e.currentTarget.getBoundingClientRect();
          const tall = box.height > box.width;
          const share = tall
            ? (e.clientY - box.top) / box.height
            : (e.clientX - box.left) / box.width;
          onSeek(Math.max(0, Math.min(1, share)) * durationMs);
        }}
        onKeyDown={(e) => {
          const next = nextMapIndex(e.key, current, blocks.length);
          if (next === null) return;
          e.preventDefault();
          setActive(next);
          buttons.current[next]?.focus();
        }}
      >
        {blocks.map((b, i) => {
          const at = `${b.turn.speaker} at ${fmt(b.turn.startMs)}`;
          return (
            <button
              key={b.turn.index}
              ref={(el) => {
                buttons.current[i] = el;
              }}
              className={`map-turn ${b.turn.channel}`}
              style={
                { ...along(b.leftPct, b.widthPct), '--fill': shares[i].toFixed(2) } as CSSProperties
              }
              tabIndex={i === current ? 0 : -1}
              title={`${b.turn.speaker} at ${fmt(b.turn.startMs)}`}
              aria-label={`Jump to ${at}${b.turns > 1 ? `, ${b.turns} turns` : ''}`}
              onClick={() => {
                setActive(i);
                onPick(b.turn);
              }}
            />
          );
        })}
        <div className="map-window" ref={reading} aria-hidden="true" />
        <div className="map-playhead" ref={playhead} aria-hidden="true" />
        {cutMs !== undefined && (
          <div
            className="map-cut"
            style={along((cutMs / durationMs) * 100)}
            aria-hidden="true"
          />
        )}
        {lost && (
          <div
            className={`map-lost ${lost.channel}`}
            style={along((lost.ms / durationMs) * 100)}
            aria-hidden="true"
          />
        )}
        {hits.map((h) => (
          <span
            key={`${h.channel}-${h.ms}`}
            className={`map-hit ${h.channel}`}
            style={along((h.ms / durationMs) * 100)}
            aria-hidden="true"
          />
        ))}
      </div>
      <div className="session-map-scale" aria-hidden="true">
        {scaleMarks(durationMs).map((t) => (
          <span key={t} style={along((t / durationMs) * 100)}>
            {fmtScale(t)}
          </span>
        ))}
        {cutMs === undefined ? (
          !lostNearEnd && (
            <span className="end" style={along(100)}>
              {fmtScale(durationMs)}
            </span>
          )
        ) : (
          <span className="end cut" style={along((cutMs / durationMs) * 100)}>
            Cut off
          </span>
        )}
        {lost && (
          <span className="cut" style={along((lost.ms / durationMs) * 100)}>
            {lost.channel === 'mic' ? 'Mic lost' : 'Audio lost'}
          </span>
        )}
      </div>
    </div>
  );
});
