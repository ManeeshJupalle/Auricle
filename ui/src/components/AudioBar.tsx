import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
  type SyntheticEvent,
} from 'react';
import { api } from '../api';
import { fmtTs } from './TurnDoc';

export interface AudioBarHandle {
  seek: (ms: number) => void;
}

interface Track {
  currentTime: number;
  duration: number;
}

const lengthOf = (a: Track) => (Number.isFinite(a.duration) ? a.duration : 0);
const finished = (a: Track) => lengthOf(a) > 0 && a.currentTime >= a.duration;

/** The longer track owns the bar: it keeps playing after the other runs out. */
export function leadTrack<T extends Track>(tracks: T[]): T {
  return tracks.reduce((lead, a) => (lengthOf(a) > lengthOf(lead) ? a : lead));
}

/**
 * What Play does. A track past its end stays silent, because play() would
 * rewind it to 0 and out of step with the bar. Once every track has finished,
 * Play starts them all over.
 */
export function playPlan<T extends Track>(tracks: T[]): { rewind: boolean; start: T[] } {
  const rewind = tracks.every(finished);
  return { rewind, start: rewind ? tracks : tracks.filter((a) => !finished(a)) };
}

/**
 * Floating playback bar for sessions recorded with retain_raw_audio: the
 * mic and loopback WAVs share one 16 kHz session timeline, so both play in
 * sync (that IS the meeting audio — both sides at once).
 */
export const AudioBar = forwardRef<
  AudioBarHandle,
  {
    sessionId: string;
    /** Where playback is, for anything that shows it besides this bar. */
    onTime?: (ms: number) => void;
    /** Whether the transcript is keeping up with playback. */
    following?: boolean;
    onFollow?: () => void;
  }
>(function AudioBar({ sessionId, onTime, following = true, onFollow }, ref) {
  const mic = useRef<HTMLAudioElement>(null);
  const loop = useRef<HTMLAudioElement>(null);
  const [playing, setPlaying] = useState(false);
  const [t, setT] = useState(0);
  const [duration, setDuration] = useState(0);
  const [playError, setPlayError] = useState<string | null>(null);

  const both = () => [mic.current, loop.current].filter(Boolean) as HTMLAudioElement[];

  useEffect(() => {
    setPlaying(false);
    setT(0);
    setDuration(0);
  }, [sessionId]);

  const refreshDuration = () => {
    const d = Math.max(...both().map((a) => (Number.isFinite(a.duration) ? a.duration : 0)), 0);
    setDuration(d);
  };

  const play = async (tracks: HTMLAudioElement[]) => {
    const results = await Promise.allSettled(tracks.map((a) => a.play()));
    const failed = results.flatMap((r, i) =>
      r.status === 'rejected' ? [{ track: tracks[i], reason: r.reason as Error }] : [],
    );
    const names = failed.map((f) => (f.track === mic.current ? 'mic' : 'system audio'));
    setPlayError(
      failed.length > 0 ? `Could not play ${names.join(' or ')}: ${failed[0].reason.message}` : null,
    );
    setPlaying(both().some((a) => !a.paused));
  };

  const toggle = async () => {
    if (playing) {
      both().forEach((a) => a.pause());
      setPlaying(false);
    } else {
      const { rewind, start } = playPlan(both());
      if (rewind) seekTo(0);
      await play(start);
    }
  };

  const seekTo = (seconds: number) => {
    both().forEach((a) => {
      a.currentTime = Math.min(seconds, Number.isFinite(a.duration) ? a.duration : seconds);
    });
    setT(seconds);
    onTime?.(seconds * 1000);
    // A shorter track stopped itself at its end; seeking back into it while
    // the other plays has to start it again.
    if (playing) void play(both().filter((a) => a.paused && !finished(a)));
  };

  const onTimeUpdate = (e: SyntheticEvent<HTMLAudioElement>) => {
    if (e.currentTarget === leadTrack(both())) {
      setT(e.currentTarget.currentTime);
      onTime?.(e.currentTarget.currentTime * 1000);
    }
  };

  const onEnded = (e: SyntheticEvent<HTMLAudioElement>) => {
    if (e.currentTarget === leadTrack(both())) setPlaying(false);
  };

  useImperativeHandle(ref, () => ({
    seek: (ms: number) => {
      seekTo(ms / 1000);
      if (!playing) void toggle();
    },
  }));

  return (
    <div className="audio-bar">
      <audio
        ref={mic}
        src={api.audioUrl(sessionId, 'mic')}
        preload="metadata"
        onLoadedMetadata={refreshDuration}
        onTimeUpdate={onTimeUpdate}
        onEnded={onEnded}
      />
      <audio
        ref={loop}
        src={api.audioUrl(sessionId, 'loopback')}
        preload="metadata"
        onLoadedMetadata={refreshDuration}
        onTimeUpdate={onTimeUpdate}
        onEnded={onEnded}
      />
      <button
        className="audio-play"
        onClick={toggle}
        title={playing ? 'Pause' : 'Play'}
        aria-label={playing ? 'Pause' : 'Play'}
      >
        <svg width="14" height="14" viewBox="0 0 14 14" fill="currentColor" aria-hidden="true">
          {playing ? (
            <path d="M3 1.5h3v11H3zM8 1.5h3v11H8z" />
          ) : (
            <path d="M3.5 1.2 12 7l-8.5 5.8z" />
          )}
        </svg>
      </button>
      <input
        type="range"
        aria-label="Playback position"
        aria-valuetext={`${fmtTs(t * 1000)} of ${fmtTs(duration * 1000)}`}
        min={0}
        max={Math.max(duration, 0.01)}
        step={0.1}
        value={Math.min(t, duration)}
        onChange={(e) => seekTo(Number(e.target.value))}
      />
      <span className="audio-time">
        {fmtTs(t * 1000)} / {fmtTs(duration * 1000)}
      </span>
      {following ? (
        <span className="audio-hint">Pick any part of the line to jump there</span>
      ) : (
        <button className="btn quiet follow-btn" onClick={onFollow}>
          Follow along
        </button>
      )}
      <span className="audio-error banner compact" role="status">
        {playError}
      </span>
    </div>
  );
});
