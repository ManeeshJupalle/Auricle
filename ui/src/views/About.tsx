import { useEffect, useState } from 'react';
import { api } from '../api';
import type { DiagnosticsInfo } from '../types';

function fmtTime(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

export function About() {
  const [version, setVersion] = useState<string>('');
  const [diag, setDiag] = useState<DiagnosticsInfo | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    api
      .health()
      .then((h) => setVersion(h.version))
      .catch(() => {});
    api
      .diagnostics()
      .then(setDiag)
      .catch(() => {});
  }, []);

  const copyDiagnostics = async () => {
    if (!diag) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(diag, null, 2));
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // clipboard blocked — the user can still read the file on disk
    }
  };

  const crashes = diag?.crashes ?? [];

  return (
    <div className="about">
      <section className="panel">
        <h2>
          auricle<span className="brand-dot">.</span>
        </h2>
        <p>
          A local-first meeting transcription engine. Your microphone is <strong>You</strong>,
          your system audio is <strong>Them</strong> — no bot joins your calls, and with the
          local Whisper backend no audio leaves this machine.
        </p>
        <p className="dim">
          Version {version || '…'} · MIT licensed ·{' '}
          <a href="https://github.com/ManeeshJupalle/Auricle" target="_blank" rel="noreferrer">
            github.com/ManeeshJupalle/Auricle
          </a>
        </p>
        <p className="dim">
          This web UI is just a client: everything it does is available over the REST/WebSocket
          API on this same port (see <code>docs/API.md</code>).
        </p>
      </section>

      <section className="panel">
        <h2>Diagnostics</h2>
        <p className="dim">
          Crashes are recorded to a local file only — nothing is ever sent anywhere. If you hit a
          bug, copy the diagnostics below and attach them to a{' '}
          <a
            href="https://github.com/ManeeshJupalle/Auricle/issues"
            target="_blank"
            rel="noreferrer"
          >
            GitHub issue
          </a>
          .
        </p>
        <div className="diag-row">
          <span className="dim mono">
            {diag ? `${diag.system.os}/${diag.system.arch} · v${diag.system.version}` : '…'} ·{' '}
            {crashes.length === 0
              ? 'no crashes recorded'
              : `${crashes.length} crash report${crashes.length === 1 ? '' : 's'}`}
          </span>
          <button className="btn" onClick={() => void copyDiagnostics()} disabled={!diag}>
            {copied ? 'Copied' : 'Copy diagnostics'}
          </button>
        </div>
        {crashes.length > 0 && (
          <ul className="diag-list">
            {crashes.slice(0, 5).map((c, i) => (
              <li key={i}>
                <span className="dim mono diag-when">{fmtTime(c.ts)}</span>
                <span className="diag-msg">{c.message}</span>
                <span className="dim mono diag-at">{c.location}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
