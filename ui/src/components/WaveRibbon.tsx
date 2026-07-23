import { useEffect, useRef } from 'react';
import { useAuricle } from '../store';

// One sample every ~90 ms tracks the daemon's ~10 Hz vu cadence.
const SAMPLE_MS = 90;
const BAR_W = 2;
const STEP = 4; // bar + gap

/** Same log mapping as VuMeter: -50 dBFS floor so quiet speech registers. */
function level(rms: number): number {
  const db = 20 * Math.log10(Math.max(rms, 1e-5));
  return Math.max(0, Math.min(1, (db + 50) / 50));
}

function cssColor(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/**
 * The listening strip: a scrolling two-voice waveform of what the engine
 * hears right now — Them (system audio) rises above the centerline, You
 * (mic) falls below it. Driven straight from store VU state inside a rAF
 * loop, so partial-heavy sessions never pay a React render for it. It is
 * an instrument, not a decoration: a flat blue line during a call means
 * the microphone is dead.
 */
export function WaveRibbon() {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    const you: number[] = [];
    const them: number[] = [];
    let lastSample = 0;
    let raf = 0;
    let theme = document.documentElement.dataset.theme;
    let colors = { you: cssColor('--you'), them: cssColor('--them'), line: cssColor('--dim-2') };

    const draw = (now: number) => {
      raf = requestAnimationFrame(draw);

      if (document.documentElement.dataset.theme !== theme) {
        theme = document.documentElement.dataset.theme;
        colors = { you: cssColor('--you'), them: cssColor('--them'), line: cssColor('--dim-2') };
      }

      const dpr = window.devicePixelRatio || 1;
      const w = canvas.clientWidth;
      const h = canvas.clientHeight;
      if (w === 0 || h === 0) return;
      if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
        canvas.width = w * dpr;
        canvas.height = h * dpr;
      }

      const cap = Math.ceil(w / STEP) + 1;
      if (now - lastSample >= SAMPLE_MS) {
        lastSample = now;
        const vu = useAuricle.getState().vu;
        you.push(level(vu['mic'] ?? 0));
        them.push(level(vu['loopback'] ?? 0));
        while (you.length > cap) you.shift();
        while (them.length > cap) them.shift();
      }

      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const mid = h / 2;
      const half = mid - 1.5;

      ctx.globalAlpha = 0.5;
      ctx.fillStyle = colors.line;
      ctx.fillRect(0, mid - 0.5, w, 1);
      ctx.globalAlpha = 1;

      // Newest sample hugs the right edge.
      for (let i = 0; i < you.length; i++) {
        const x = w - (you.length - i) * STEP;
        const up = Math.max(them[i] * half, them[i] > 0.002 ? 1 : 0);
        const down = Math.max(you[i] * half, you[i] > 0.002 ? 1 : 0);
        if (up > 0) {
          ctx.fillStyle = colors.them;
          ctx.fillRect(x, mid - 1 - up, BAR_W, up);
        }
        if (down > 0) {
          ctx.fillStyle = colors.you;
          ctx.fillRect(x, mid + 1, BAR_W, down);
        }
      }
    };

    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, []);

  return (
    <div className="wave-ribbon" aria-hidden="true">
      <span className="wave-tag them">Them</span>
      <canvas ref={canvasRef} />
      <span className="wave-tag you">You</span>
    </div>
  );
}
