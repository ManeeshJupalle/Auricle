import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from 'react';
import { api } from '../api';
import { fmtTs } from './TurnDoc';

export interface AudioBarHandle {
  seek: (ms: number) => void;
}

/**
 * Floating playback bar for sessions recorded with retain_raw_audio: the
 * mic and loopback WAVs share one 16 kHz session timeline, so both play in
 * sync (that IS the meeting audio — both sides at once).
 */
export const AudioBar = forwardRef<AudioBarHandle, { sessionId: string }>(function AudioBar(
  { sessionId },
  ref,
) {
  const mic = useRef<HTMLAudioElement>(null);
  const loop = useRef<HTMLAudioElement>(null);
  const [playing, setPlaying] = useState(false);
  const [t, setT] = useState(0);
  const [duration, setDuration] = useState(0);

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

  const toggle = async () => {
    if (playing) {
      both().forEach((a) => a.pause());
      setPlaying(false);
    } else {
      await Promise.all(both().map((a) => a.play().catch(() => {})));
      setPlaying(true);
    }
  };

  const seekTo = (seconds: number) => {
    both().forEach((a) => {
      a.currentTime = Math.min(seconds, Number.isFinite(a.duration) ? a.duration : seconds);
    });
    setT(seconds);
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
      />
      <audio
        ref={loop}
        src={api.audioUrl(sessionId, 'loopback')}
        preload="metadata"
        onLoadedMetadata={refreshDuration}
        onTimeUpdate={(e) => setT(e.currentTarget.currentTime)}
        onEnded={() => setPlaying(false)}
      />
      <button className="audio-play" onClick={toggle} title={playing ? 'Pause' : 'Play'}>
        {playing ? '❚❚' : '▶'}
      </button>
      <input
        type="range"
        min={0}
        max={Math.max(duration, 0.01)}
        step={0.1}
        value={Math.min(t, duration)}
        onChange={(e) => seekTo(Number(e.target.value))}
      />
      <span className="audio-time">
        {fmtTs(t * 1000)} / {fmtTs(duration * 1000)}
      </span>
    </div>
  );
});
