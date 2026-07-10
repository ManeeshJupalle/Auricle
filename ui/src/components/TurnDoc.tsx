import { memo, useEffect, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useAuricle, type Row, type Turn } from '../store';

export function fmtTs(ms: number): string {
  const m = Math.floor(ms / 60_000);
  const s = Math.floor(ms / 1000) % 60;
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
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

const StaticSeg = memo(function StaticSeg({ row }: { row: Row }) {
  return <span className="seg">{row.text} </span>;
});

function TurnBlock({
  turn,
  children,
  onSeek,
}: {
  turn: Turn;
  children: React.ReactNode;
  onSeek?: (ms: number) => void;
}) {
  return (
    <div className="turn">
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
  onSeek,
}: {
  id: string;
  onSeek?: (ms: number) => void;
}) {
  const turn = useAuricle((s) => s.turns[id]);
  if (!turn) return null;
  return (
    <TurnBlock turn={turn} onSeek={onSeek}>
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
  emptyHint: string;
}

/**
 * Document-style transcript: virtualized speaker turns with a timestamp
 * gutter. `live` reads from the store (per-row subscriptions); otherwise
 * `turns`/`rows` props carry a stored session.
 */
export function TurnDoc({ turnOrder, turns, rows, live, follow, onSeek, emptyHint }: DocProps) {
  const parentRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  const virtualizer = useVirtualizer({
    count: turnOrder.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 72,
    overscan: 8,
  });

  useEffect(() => {
    const el = parentRef.current;
    if (!el || !follow) return;
    const onScroll = () => {
      stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 120;
    };
    el.addEventListener('scroll', onScroll);
    return () => el.removeEventListener('scroll', onScroll);
  }, [follow]);

  useEffect(() => {
    if (follow && stickToBottom.current && turnOrder.length > 0) {
      virtualizer.scrollToIndex(turnOrder.length - 1, { align: 'end' });
    }
  }, [follow, turnOrder.length, virtualizer]);

  if (turnOrder.length === 0) {
    return (
      <div className="doc-empty">
        <p>{emptyHint}</p>
      </div>
    );
  }

  return (
    <div className="doc-scroll" ref={parentRef}>
      <div
        className="doc-inner"
        style={{ height: virtualizer.getTotalSize(), position: 'relative' }}
      >
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
                <LiveTurn id={id} onSeek={onSeek} />
              ) : (
                <TurnBlock turn={turns![id]} onSeek={onSeek}>
                  {turns![id].segIds.map((segId) => (
                    <StaticSeg key={segId} row={rows![segId]} />
                  ))}
                </TurnBlock>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
