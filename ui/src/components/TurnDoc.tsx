import { forwardRef, memo, useCallback, useEffect, useImperativeHandle, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useAuricle, type Row, type Turn } from '../store';
import { Marked } from './Marked';

export function fmtTs(ms: number): string {
  const h = Math.floor(ms / 3_600_000);
  const m = Math.floor(ms / 60_000) % 60;
  const s = Math.floor(ms / 1000) % 60;
  const clock = `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  return h > 0 ? `${h}:${clock}` : clock;
}

/**
 * One segment inside a live turn. Subscribed to exactly its own row —
 * partial growth re-renders this span alone (see store.ts; vitest asserts
 * the turn layer keeps identity).
 */
const LiveSeg = memo(function LiveSeg({ id }: { id: string }) {
  const row = useAuricle((s) => s.rows[id]);
  if (!row) return null;
  return <span className={row.final ? 'seg' : 'seg partial'}>{row.text} </span>;
});

const StaticSeg = memo(function StaticSeg({ row, query }: { row: Row; query: string }) {
  return (
    <span className="seg">
      <Marked text={row.text} query={query} />{' '}
    </span>
  );
});

function TurnBlock({
  turn,
  continued,
  found = false,
  current = false,
  children,
  onSeek,
}: {
  turn: Turn;
  /** The same voice as the turn before: it closes up like a new paragraph. */
  continued: boolean;
  /** The moment a search opened this meeting at. */
  found?: boolean;
  /** Being played right now. */
  current?: boolean;
  children: React.ReactNode;
  onSeek?: (ms: number) => void;
}) {
  return (
    <div
      className={`turn ${turn.channel} ${continued ? 'cont' : ''} ${found ? 'found' : ''} ${
        current ? 'current' : ''
      }`}
    >
      <div className="turn-side">
        <span className={`turn-speaker ${turn.channel}`}>{turn.speaker}</span>
        <button
          className="turn-time"
          title={onSeek ? 'Play from here' : undefined}
          disabled={!onSeek}
          onClick={() => onSeek?.(turn.tStartMs)}
        >
          {fmtTs(turn.tStartMs)}
        </button>
      </div>
      <p className="turn-text">{children}</p>
    </div>
  );
}

/** The live turn shell only re-renders when its own segment list grows. */
const LiveTurn = memo(function LiveTurn({
  id,
  prevId,
  onSeek,
}: {
  id: string;
  prevId: string | undefined;
  onSeek?: (ms: number) => void;
}) {
  const turn = useAuricle((s) => s.turns[id]);
  // A string, so growth in the turn before can't re-render this one.
  const prevChannel = useAuricle((s) => (prevId ? s.turns[prevId]?.channel : undefined));
  if (!turn) return null;
  return (
    <TurnBlock turn={turn} continued={prevChannel === turn.channel} onSeek={onSeek}>
      {turn.segIds.map((segId) => (
        <LiveSeg key={segId} id={segId} />
      ))}
    </TurnBlock>
  );
});

interface DocProps {
  turnOrder: string[];
  turns?: Record<string, Turn>;
  rows?: Record<string, Row>;
  live: boolean;
  follow?: boolean;
  onSeek?: (ms: number) => void;
  /** The turns on screen, by index, as the reader scrolls. */
  onRange?: (first: number, last: number) => void;
  /** Marked wherever it appears in a stored transcript. */
  query?: string;
  /** The turn a search opened this meeting at. */
  foundTurn?: number | null;
  /** The turn being played. */
  currentTurn?: number | null;
  /** The reader took the scroll position into their own hands. */
  onUserScroll?: () => void;
  emptyHint: React.ReactNode;
}

const SCROLL_KEYS = new Set(['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' ']);

/** A scroll this soon after the reader touched something is theirs. */
const READER_MS = 300;

export interface TurnDocHandle {
  /** Scroll a turn into view (session map → transcript). */
  scrollToTurn: (index: number) => void;
}

/**
 * Document-style transcript: virtualized speaker turns with a timestamp
 * gutter. `live` reads from the store (per-row subscriptions); otherwise
 * `turns`/`rows` props carry a stored session.
 */
export const TurnDoc = forwardRef<TurnDocHandle, DocProps>(function TurnDoc(
  {
    turnOrder,
    turns,
    rows,
    live,
    follow,
    onSeek,
    onRange,
    query = '',
    foundTurn,
    currentTurn,
    onUserScroll,
    emptyHint,
  },
  ref,
) {
  const parentRef = useRef<HTMLDivElement | null>(null);
  const stickToBottom = useRef(true);
  const lastReaderInput = useRef(-Infinity);
  const userScrolled = useRef(onUserScroll);
  userScrolled.current = onUserScroll;
  const followRef = useRef(follow);
  followRef.current = follow;

  // A callback ref, because the scroller appears late: the empty state has
  // none, so an effect keyed on `follow` would run once, find nothing, and
  // never attach.
  const attachScroller = useCallback((el: HTMLDivElement) => {
    parentRef.current = el;
    // Only the reader can leave the bottom. Rows being measured can push it a
    // long way off in one step, and that must not end the follow.
    const onScroll = () => {
      const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 120;
      if (nearBottom) stickToBottom.current = true;
      else if (performance.now() - lastReaderInput.current < READER_MS) {
        stickToBottom.current = false;
      }
    };
    // Scroll events can't tell a reader from a follow, so listen for the reader.
    const onReader = () => {
      lastReaderInput.current = performance.now();
      userScrolled.current?.();
    };
    const onKey = (e: KeyboardEvent) => {
      if (SCROLL_KEYS.has(e.key)) onReader();
    };
    const onScrollbar = (e: PointerEvent) => {
      if (e.target === el) onReader();
    };
    // The window onto the text can change without the text changing: a header
    // folding away, or meters arriving above it.
    const resized = new ResizeObserver(() => {
      if (followRef.current && stickToBottom.current) el.scrollTop = el.scrollHeight;
    });
    resized.observe(el);
    el.addEventListener('scroll', onScroll);
    el.addEventListener('wheel', onReader, { passive: true });
    el.addEventListener('touchmove', onReader, { passive: true });
    el.addEventListener('pointerdown', onScrollbar);
    el.addEventListener('keydown', onKey);
    return () => {
      resized.disconnect();
      el.removeEventListener('scroll', onScroll);
      el.removeEventListener('wheel', onReader);
      el.removeEventListener('touchmove', onReader);
      el.removeEventListener('pointerdown', onScrollbar);
      el.removeEventListener('keydown', onKey);
      parentRef.current = null;
    };
  }, []);

  const virtualizer = useVirtualizer({
    count: turnOrder.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 96,
    overscan: 8,
    // Room under the last line, inside the list, so the end of the list and
    // the end of the scroller are the same place.
    paddingEnd: 36,
  });

  // Keyed on the measured height too: a monologue grows inside one turn, so
  // the turn count alone would let it run off the bottom of the screen.
  const totalSize = virtualizer.getTotalSize();
  useEffect(() => {
    // The real bottom, padding included, so the newest words clear the fade.
    // scrollToIndex stops at the last row's edge, and only gets further on its
    // animation-frame retries, which a page with nothing animating may not run.
    const el = parentRef.current;
    if (follow && stickToBottom.current && el) el.scrollTop = el.scrollHeight;
  }, [follow, turnOrder.length, totalSize]);

  const first = virtualizer.range?.startIndex;
  const last = virtualizer.range?.endIndex;
  useEffect(() => {
    if (first !== undefined && last !== undefined) onRange?.(first, last);
  }, [first, last, onRange]);

  useImperativeHandle(
    ref,
    () => ({
      scrollToTurn: (index: number) => {
        // Only a hop to a row already on the page may animate. A long smooth
        // scroll runs through rows nobody has measured, which sit at their
        // estimated height and overlap until they are.
        const near = virtualizer.getVirtualItems().some((v) => v.index === index);
        const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
        virtualizer.scrollToIndex(index, {
          align: 'start',
          behavior: near && !reduced ? 'smooth' : 'auto',
        });
      },
    }),
    [virtualizer],
  );

  if (turnOrder.length === 0) {
    return (
      <div className="doc-empty">
        <div className="doc-empty-body">{emptyHint}</div>
      </div>
    );
  }

  return (
    <div className="doc-scroll" ref={attachScroller}>
      <div className="doc-inner" style={{ height: totalSize, position: 'relative' }}>
        {virtualizer.getVirtualItems().map((v) => {
          const id = turnOrder[v.index];
          return (
            <div
              key={id}
              data-index={v.index}
              ref={virtualizer.measureElement}
              style={{
                position: 'absolute',
                top: 0,
                left: 0,
                width: '100%',
                transform: `translateY(${v.start}px)`,
              }}
            >
              {live ? (
                <LiveTurn id={id} prevId={turnOrder[v.index - 1]} onSeek={onSeek} />
              ) : (
                <TurnBlock
                  turn={turns![id]}
                  continued={turns![turnOrder[v.index - 1]]?.channel === turns![id].channel}
                  found={v.index === foundTurn}
                  current={v.index === currentTurn}
                  onSeek={onSeek}
                >
                  {turns![id].segIds.map((segId) => (
                    <StaticSeg key={segId} row={rows![segId]} query={query} />
                  ))}
                </TurnBlock>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
});
