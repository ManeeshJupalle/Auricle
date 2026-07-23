import { useEffect, useState } from 'react';
import { api } from '../api';
import type { EgressEntry, ProvidersResponse } from '../types';

// Deterministic duet motif: a still of the listening strip. Them (system
// audio) above the centerline, You (mic) below — a conversation where
// Them talks in longer runs and You interjects.
const THEM = [
  0.1, 0.35, 0.6, 0.5, 0.75, 0.55, 0.3, 0.15, 0, 0, 0.2, 0.5, 0.7, 0.85, 0.6, 0.4, 0.5, 0.25,
  0.1, 0, 0, 0, 0.15, 0.4, 0.65, 0.8, 0.55, 0.7, 0.45, 0.2, 0.1, 0, 0, 0.3, 0.55, 0.4, 0.6,
  0.35, 0.15, 0.05,
];
const YOU = [
  0, 0, 0, 0.1, 0, 0, 0.15, 0.45, 0.7, 0.5, 0.25, 0.1, 0, 0, 0, 0.1, 0, 0, 0.35, 0.6, 0.75,
  0.5, 0.3, 0.1, 0, 0, 0, 0.15, 0.05, 0, 0.2, 0.5, 0.65, 0.35, 0.1, 0, 0, 0.1, 0.3, 0.15,
];

function DuetMotif() {
  const w = THEM.length * 8 - 3;
  const mid = 27;
  const half = 24;
  return (
    <svg
      className="duet-motif"
      viewBox={`0 0 ${w} 54`}
      width={w}
      height="54"
      aria-hidden="true"
    >
      <line x1="0" y1={mid} x2={w} y2={mid} className="duet-line" />
      {THEM.map((v, i) =>
        v > 0 ? (
          <rect
            key={`t${i}`}
            className="duet-them"
            x={i * 8}
            y={mid - 1.5 - v * half}
            width="3.5"
            height={v * half}
            rx="1.5"
          />
        ) : null,
      )}
      {YOU.map((v, i) =>
        v > 0 ? (
          <rect
            key={`y${i}`}
            className="duet-you"
            x={i * 8}
            y={mid + 1.5}
            width="3.5"
            height={v * half}
            rx="1.5"
          />
        ) : null,
      )}
    </svg>
  );
}

/**
 * The idle home. Everything here is content the sidebar does NOT already
 * show: the You/Them model, what the engine can do right now (provider
 * readiness), the privacy proof from the egress ledger, and the copilot
 * hotkeys that otherwise live only in the README.
 */
export function Home({ onShowEgress }: { onShowEgress: () => void }) {
  const [prov, setProv] = useState<ProvidersResponse | null>(null);
  const [egress, setEgress] = useState<EgressEntry[] | null>(null);
  const [egressFailed, setEgressFailed] = useState(false);

  useEffect(() => {
    api.providers().then(setProv).catch(() => {});
    api
      .egress()
      .then(setEgress)
      .catch(() => setEgressFailed(true));
  }, []);

  const cloud = egress?.filter((e) => e.destination === 'cloud') ?? [];
  const hosts = [...new Set(cloud.map((e) => e.host).filter((h): h is string => !!h))];

  return (
    <div className="home">
      <div className="home-inner">
        <h1 className="home-brand">
          auricle<span className="brand-dot">.</span>
        </h1>
        <p className="home-tagline">The part of your computer that listens.</p>
        <DuetMotif />
        <p className="home-model">
          Your microphone is <strong className="voice-you">You</strong>; everything your computer
          plays is <strong className="voice-them">Them</strong>. No bot joins your calls — press{' '}
          <strong>Start recording</strong> in the sidebar when the meeting starts.
        </p>

        <div className="home-rows">
          {prov && (
            <div className="home-row">
              <span className="home-eyebrow">Transcription</span>
              <span className="home-chips">
                {prov.providers.map((p) => (
                  <span
                    key={p.id}
                    className={`ready-chip ${p.ready ? 'ok' : ''}`}
                    title={p.detail}
                  >
                    <span className="ready-dot" />
                    {p.id}
                  </span>
                ))}
              </span>
            </div>
          )}
          {prov && (
            <div className="home-row">
              <span className="home-eyebrow">Summaries</span>
              <span className="home-chips">
                {prov.llm.map((p) => (
                  <span
                    key={p.id}
                    className={`ready-chip ${p.ready ? 'ok' : ''}`}
                    title={p.detail}
                  >
                    <span className="ready-dot" />
                    {p.id}
                    <span className="ready-model">{p.model}</span>
                  </span>
                ))}
              </span>
            </div>
          )}
          {!egressFailed && (
          <div className="home-row">
            <span className="home-eyebrow">Privacy</span>
            <span className="home-privacy">
              {egress === null ? (
                <span className="dim">…</span>
              ) : cloud.length === 0 ? (
                <>
                  <span className="privacy-ok">Nothing has left this machine.</span>
                  {egress.length > 0 && (
                    <span className="dim">
                      {' '}
                      {egress.length} local {egress.length === 1 ? 'action' : 'actions'} on record.
                    </span>
                  )}
                </>
              ) : (
                <span>
                  Data left this machine <strong>{cloud.length}</strong>{' '}
                  {cloud.length === 1 ? 'time' : 'times'}, to{' '}
                  <span className="mono">{hosts.join(', ')}</span>.
                </span>
              )}{' '}
              <button className="link-btn" onClick={onShowEgress}>
                Open the ledger
              </button>
            </span>
          </div>
          )}
          <div className="home-row">
            <span className="home-eyebrow">Copilot</span>
            <span className="home-keys">
              <span>
                <kbd>Ctrl+Shift+Space</kbd> ask about the meeting or your screen
              </span>
              <span>
                <kbd>Ctrl+Shift+A</kbd> “what’s happening right now?”
              </span>
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}
