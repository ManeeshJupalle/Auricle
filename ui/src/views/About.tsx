import { useEffect, useState } from 'react';
import { api } from '../api';

export function About() {
  const [version, setVersion] = useState<string>('');
  useEffect(() => {
    api.health().then((h) => setVersion(h.version)).catch(() => {});
  }, []);
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
    </div>
  );
}
