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
 * The session map: who spoke when, across the whole recording. Them (system
 * audio) sits above the centerline, You (mic) below — the same two-voice
 * band as the live listening strip, frozen into the finished session. It is
 * navigation, not decoration: click a block to jump the transcript there
 * (and the audio, when the recording was retained).
 */
export function SessionMap({
  turns,
  durationMs,
  onPick,
}: {
  turns: MapTurn[];
  durationMs: number;
  onPick: (turn: MapTurn) => void;
}) {
  if (turns.length === 0 || durationMs <= 0) return null;

  return (
    <div className="session-map">
      <div className="session-map-track">
        {turns.map((t) => {
          const left = (t.startMs / durationMs) * 100;
          const width = Math.max(((t.endMs - t.startMs) / durationMs) * 100, 0.4);
          return (
            <button
              key={t.index}
              className={`map-turn ${t.channel}`}
              style={{ left: `${left}%`, width: `${Math.min(width, 100 - left)}%` }}
              title={`${t.speaker} · ${fmt(t.startMs)}`}
              aria-label={`Jump to ${t.speaker} at ${fmt(t.startMs)}`}
              onClick={() => onPick(t)}
            />
          );
        })}
      </div>
      <div className="session-map-scale">
        <span>00:00</span>
        <span>{fmt(durationMs)}</span>
      </div>
    </div>
  );
}
