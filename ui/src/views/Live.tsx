import { useEffect, useState } from 'react';
import { api } from '../api';
import { Transcript } from '../components/Transcript';
import { VuMeter } from '../components/VuMeter';
import { useAuricle } from '../store';
import type { ProviderInfo } from '../types';

export function Live() {
  const engineState = useAuricle((s) => s.engineState);
  const activeSession = useAuricle((s) => s.activeSession);
  const sessionTitle = useAuricle((s) => s.sessionTitle);
  const sessionProvider = useAuricle((s) => s.sessionProvider);
  const latencyMs = useAuricle((s) => s.latencyMs);
  const deviceLost = useAuricle((s) => s.deviceLost);
  const lastError = useAuricle((s) => s.lastError);

  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [provider, setProvider] = useState<string>('');
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  useEffect(() => {
    api
      .providers()
      .then((body) => {
        const list = body.providers;
        setProviders(list);
        const ready = list.filter((p) => p.ready);
        const preferred = ready.find((p) => p.default) ?? ready[0] ?? list[0];
        if (preferred) setProvider(preferred.id);
      })
      .catch(() => setActionError('cannot reach the daemon'));
  }, []);

  const recording = engineState !== 'idle';

  const start = async () => {
    setBusy(true);
    setActionError(null);
    try {
      await api.startSession({
        title: `Session ${new Date().toLocaleString()}`,
        stt_provider: provider || undefined,
      });
      // session_started over WS flips the UI state.
    } catch (e) {
      setActionError(`start failed: ${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    if (!activeSession) return;
    setBusy(true);
    setActionError(null);
    try {
      await api.stopSession(activeSession);
    } catch (e) {
      setActionError(`stop failed: ${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="live">
      <div className="live-controls panel">
        <select
          value={provider}
          onChange={(e) => setProvider(e.target.value)}
          disabled={recording}
          title="STT provider for the next session"
        >
          {providers.map((p) => (
            <option key={p.id} value={p.id} disabled={!p.ready}>
              {p.id}
              {p.ready ? '' : ' (not ready)'}
            </option>
          ))}
        </select>
        {recording ? (
          <button className="btn stop" onClick={stop} disabled={busy || engineState === 'stopping'}>
            {engineState === 'stopping' ? 'Stopping…' : 'Stop'}
          </button>
        ) : (
          <button className="btn start" onClick={start} disabled={busy || !provider}>
            Start session
          </button>
        )}
        <div className="live-meters">
          <VuMeter channel="mic" label="You" />
          <VuMeter channel="loopback" label="Them" />
        </div>
        <div className="live-status">
          {recording && sessionTitle && (
            <span className="dim" title={activeSession ?? ''}>
              {sessionTitle} · {sessionProvider}
            </span>
          )}
          <span className="latency" title="chunk→final latency of the last final segment">
            latency {latencyMs !== null ? `${(latencyMs / 1000).toFixed(1)}s` : '—'}
          </span>
        </div>
      </div>

      {deviceLost && <div className="banner error">audio device lost — {deviceLost}</div>}
      {(lastError ?? actionError) && (
        <div className="banner warn">{lastError ?? actionError}</div>
      )}

      <Transcript />
    </div>
  );
}
