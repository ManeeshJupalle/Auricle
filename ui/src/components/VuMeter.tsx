import { useAuricle } from '../store';

/**
 * Per-channel level meter fed by ~10 Hz `vu` events. RMS is mapped
 * logarithmically (-50 dBFS floor) so quiet speech still moves the bar.
 */
export function VuMeter({ channel, label }: { channel: string; label: string }) {
  const rms = useAuricle((s) => s.vu[channel] ?? 0);
  const db = 20 * Math.log10(Math.max(rms, 1e-5));
  const pct = Math.max(0, Math.min(1, (db + 50) / 50)) * 100;
  return (
    <div className="vu">
      <span className={`vu-label ${channel}`}>{label}</span>
      <div className="vu-track">
        <div className="vu-fill" style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}
