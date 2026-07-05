import { memo, useEffect, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useAuricle } from '../store';

function fmtTs(ms: number): string {
  const m = Math.floor(ms / 60_000);
  const s = Math.floor(ms / 1000) % 60;
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
}

/**
 * One transcript row. Memoized and subscribed to exactly its own row, so a
 * partial text update re-renders this component only — the list shell and
 * every other row are untouched (the anti-Meetily thesis, measurable in
 * the React profiler).
 */
const TranscriptRow = memo(function TranscriptRow({ id }: { id: string }) {
  const row = useAuricle((s) => s.rows[id]);
  if (!row) return null;
  return (
    <div className={`t-row ${row.final ? 'final' : 'partial'}`}>
      <span className="t-time">{fmtTs(row.tStartMs)}</span>
      <span className={`t-speaker ${row.channel}`}>{row.speaker}</span>
      <span className="t-text">{row.text}</span>
    </div>
  );
});

export function Transcript() {
  const order = useAuricle((s) => s.order);
  const parentRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  const virtualizer = useVirtualizer({
    count: order.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 32,
    overscan: 12,
  });

  // Follow the live tail unless the user scrolled up.
  useEffect(() => {
    const el = parentRef.current;
    if (!el) return;
    const onScroll = () => {
      stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    };
    el.addEventListener('scroll', onScroll);
    return () => el.removeEventListener('scroll', onScroll);
  }, []);

  useEffect(() => {
    if (stickToBottom.current && order.length > 0) {
      virtualizer.scrollToIndex(order.length - 1, { align: 'end' });
    }
  }, [order.length, virtualizer]);

  if (order.length === 0) {
    return (
      <div className="t-empty">
        <p>No speech yet.</p>
        <p className="dim">
          Start a session, then talk or play audio — partials appear dim and settle into
          finals.
        </p>
      </div>
    );
  }

  return (
    <div className="t-scroll" ref={parentRef}>
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
        {virtualizer.getVirtualItems().map((v) => (
          <div
            key={order[v.index]}
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
            <TranscriptRow id={order[v.index]} />
          </div>
        ))}
      </div>
    </div>
  );
}
