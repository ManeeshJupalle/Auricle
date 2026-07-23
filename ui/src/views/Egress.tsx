import { useEffect, useState } from 'react';
import { api } from '../api';
import type { EgressEntry } from '../types';

const KIND_LABEL: Record<string, string> = {
  audio: 'Audio',
  prompt: 'Prompt',
  summary: 'Summary',
};

function fmtTime(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

function fmtSize(e: EgressEntry): string {
  if (e.items === null) return '';
  if (e.kind === 'audio') return `${e.items}`;
  return `${e.items.toLocaleString()} chars`;
}

/**
 * The egress ledger: a local, verifiable record of everything user data
 * that left the machine to a third party (and what explicitly stayed
 * local). Metadata only — the audio, prompts, and answers themselves are
 * never recorded, here or in the database.
 */
export function Egress() {
  const [entries, setEntries] = useState<EgressEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .egress()
      .then(setEntries)
      .catch((e) => setError((e as Error).message));
  }, []);

  const cloud = entries?.filter((e) => e.destination === 'cloud') ?? [];
  const hosts = [...new Set(cloud.map((e) => e.host).filter((h): h is string => !!h))];

  return (
    <div className="settings egress">
      {error && <div className="banner error">{error}</div>}

      <section className="panel egress-hero">
        <h2>Egress ledger</h2>
        {entries === null ? (
          <p className="dim">{error ? 'The ledger could not be read.' : 'Loading…'}</p>
        ) : cloud.length === 0 ? (
          <p className="egress-headline ok">
            Nothing has left this machine — every recorded action stayed fully local.
          </p>
        ) : (
          <p className="egress-headline">
            Data has left this machine <strong>{cloud.length}</strong>{' '}
            {cloud.length === 1 ? 'time' : 'times'} to{' '}
            <strong>{hosts.length}</strong> {hosts.length === 1 ? 'destination' : 'destinations'}:{' '}
            <span className="mono">{hosts.join(', ')}</span>.
          </p>
        )}
        <p className="dim note">
          Every audio stream, prompt, and summary is logged here with its destination and rough
          size — never its contents. Local providers are recorded too, so silence is never a gap.
        </p>
      </section>

      {entries !== null && entries.length > 0 && (
        <section className="panel">
          <h2>Activity</h2>
          <ul className="egress-list">
            {entries.map((e) => (
              <li key={e.id} className="egress-row">
                <span className={`egress-badge ${e.destination}`}>
                  {e.destination === 'cloud' ? 'CLOUD' : 'LOCAL'}
                </span>
                <span className="egress-kind">{KIND_LABEL[e.kind] ?? e.kind}</span>
                <span className="egress-dest">
                  {e.destination === 'cloud' ? (
                    <>
                      {'→ '}
                      {e.provider}
                      {e.host && <span className="dim"> ({e.host})</span>}
                    </>
                  ) : (
                    <span className="dim">stayed local ({e.provider})</span>
                  )}
                </span>
                <span className="egress-size dim mono">{fmtSize(e)}</span>
                <span className="egress-time dim mono">{fmtTime(e.ts)}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
