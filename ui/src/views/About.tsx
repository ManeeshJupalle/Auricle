import { useEffect, useState } from 'react';
import { api } from '../api';
import type { DiagnosticsInfo } from '../types';

function fmtTime(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
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
        <h1 className="page-title">Auricle</h1>
        <p className="about-lede">
          The outer ear: the part of you that listens. It sits on your head, not in the cloud.
        </p>
        <p>
          Auricle writes down your meetings from your own machine. Your microphone is You and what
          your computer plays is Them. No bot joins your calls, and with local Whisper no audio
          leaves this computer.
        </p>
        <p className="note">
          Version {version || '…'}, MIT licensed.{' '}
          <a href="https://github.com/ManeeshJupalle/Auricle" target="_blank" rel="noreferrer">
            github.com/ManeeshJupalle/Auricle
          </a>
        </p>
        <p className="note">
          This page is one client of the engine. Everything it does is also available over the REST
          and WebSocket API on this same port (see <code>docs/API.md</code>).
        </p>
      </section>

      <section className="panel">
        <h2>Diagnostics</h2>
        <p className="note">
          Crashes are written to a local file and never sent anywhere. If you hit a bug, copy the
          diagnostics and attach them to a{' '}
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
          <span className="note">
            {diag ? `Version ${diag.system.version} on ${diag.system.os}, ${diag.system.arch}. ` : '… '}
            {crashes.length === 0
              ? 'No crashes on record.'
              : `${crashes.length} crash ${crashes.length === 1 ? 'report' : 'reports'} on record.`}
          </span>
          <button className="btn" onClick={() => void copyDiagnostics()} disabled={!diag}>
            {copied ? 'Copied' : 'Copy diagnostics'}
          </button>
        </div>
        {crashes.length > 0 && (
          <ul className="diag-list">
            {crashes.slice(0, 5).map((c, i) => (
              <li key={i}>
                <span className="dim diag-when">{fmtTime(c.ts)}</span>
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
