import { useEffect, useRef, useState } from 'react';
import { useAuricle } from '../store';
import { buildMapTurns, SessionMap, type MapHit, type MapTurn } from './SessionMap';

/** How much of the pen's reach survives each frame, so it falls back like a needle. */
const FALL = 0.9;

/** The shortest track, so the first turns of a meeting don't fill the whole tape. */
const MIN_TRACK_MS = 60_000;

/** Same log mapping as VuMeter: -50 dBFS floor so quiet speech registers. */
function level(rms: number): number {
  const db = 20 * Math.log10(Math.max(rms, 1e-5));
  return Math.max(0, Math.min(1, (db + 50) / 50));
}

/**
 * The pen at the end of the tape: what the engine hears right now, Them on one
 * side of the rule and You on the other. Driven straight from store VU state
 * inside a rAF loop, so partial-heavy sessions never pay a React render for it.
 * It is an instrument, not a decoration: a You side that never moves during a
 * call means the microphone is dead.
 */
function Pen({ lost }: { lost: string | undefined }) {
  const them = useRef<HTMLElement>(null);
  const you = useRef<HTMLElement>(null);

  useEffect(() => {
    let raf = 0;
    let themReach = 0;
    let youReach = 0;
    const draw = () => {
      raf = requestAnimationFrame(draw);
      const vu = useAuricle.getState().vu;
      themReach = Math.max(level(vu['loopback'] ?? 0), themReach * FALL);
      youReach = Math.max(level(vu['mic'] ?? 0), youReach * FALL);
      them.current?.style.setProperty('--level', themReach.toFixed(3));
      you.current?.style.setProperty('--level', youReach.toFixed(3));
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, []);

  return (
    <div className="map-pen" aria-hidden="true">
      {lost !== 'loopback' && <i className="them" ref={them} />}
      {lost !== 'mic' && <i className="you" ref={you} />}
    </div>
  );
}

/**
 * The tape while a meeting runs: the same blocks on the same time axis as the
 * finished map, so a live tape is simply a map that isn't done yet. It reads
 * the store once a second instead of subscribing, so a partial's growth never
 * reaches it, and the transcript never pays for it. A device that goes away
 * breaks its lane there, so the tape can't look healthy with a side gone.
 */
export function LiveTape({
  showPen,
  onPick,
}: {
  showPen: boolean;
  onPick: (turn: MapTurn) => void;
}) {
  const [tape, setTape] = useState<{ turns: MapTurn[]; durationMs: number; lost?: MapHit }>({
    turns: [],
    durationMs: 0,
  });
  // The event carries no time, so this is when the tape first heard of it.
  const lostAt = useRef<MapHit | undefined>(undefined);

  useEffect(() => {
    const read = () => {
      const { turnOrder, turns, rows, liveStartedAtMs, deviceLost } = useAuricle.getState();
      const mapped = buildMapTurns(turnOrder, turns, rows);
      const elapsed = liveStartedAtMs === null ? 0 : Date.now() - liveStartedAtMs;
      const reached = mapped[mapped.length - 1]?.endMs ?? 0;
      if (deviceLost && !lostAt.current) {
        lostAt.current = {
          ms: Math.max(elapsed, reached),
          channel: deviceLost.startsWith('mic') ? 'mic' : 'loopback',
        };
      }
      setTape({
        turns: mapped,
        durationMs: Math.max(elapsed, reached, MIN_TRACK_MS),
        lost: lostAt.current,
      });
    };
    read();
    const timer = setInterval(read, 1000);
    return () => clearInterval(timer);
  }, []);

  return (
    <>
      <SessionMap
        turns={tape.turns}
        durationMs={tape.durationMs}
        onPick={onPick}
        lost={tape.lost}
      />
      {showPen && <Pen lost={tape.lost?.channel} />}
    </>
  );
}
