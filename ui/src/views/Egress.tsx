import { useEffect, useState } from 'react';
import { api } from '../api';
import type { EgressEntry, EgressLedger, EgressTotal } from '../types';

const KIND_LABEL: Record<string, string> = {
  audio: 'Audio',
  prompt: 'Prompt',
  summary: 'Summary',
  live_transcript: 'Live transcript',
  session_list: 'Meeting list',
  session_read: 'Meeting',
  transcript_search: 'Search',
};

function fmtTime(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

/** Roll the whole-ledger totals up for one destination. */
function tally(totals: EgressTotal[] | undefined, destination: EgressTotal['destination']) {
  const rows = totals?.filter((t) => t.destination === destination) ?? [];
  return { count: rows.reduce((n, t) => n + t.count, 0), who: rows.map((t) => t.who) };
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
  const [ledger, setLedger] = useState<EgressLedger | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .egress()
      .then(setLedger)
      .catch((e) => setError((e as Error).message));
  }, []);

  // Headline counts come from the whole-ledger totals, never from the page
  // of entries below: an agent can make far more reads than a page holds.
  const cloud = tally(ledger?.totals, 'cloud');
  const agent = tally(ledger?.totals, 'agent');
  const entries = ledger?.entries ?? null;

  return (
    <div className="settings egress">
      {error && <div className="banner error">{error}</div>}

      <section className="panel egress-hero">
        <h2>Egress ledger</h2>
        {entries === null ? (
          <p className="dim">{error ? 'The ledger could not be read.' : 'Loading…'}</p>
        ) : cloud.count === 0 && agent.count === 0 ? (
          <p className="egress-headline ok">
            Nothing has left this machine — every recorded action stayed fully local.
          </p>
        ) : (
          <>
            {cloud.count > 0 && (
              <p className="egress-headline">
                Data has left this machine <strong>{cloud.count}</strong>{' '}
                {cloud.count === 1 ? 'time' : 'times'} to <strong>{cloud.who.length}</strong>{' '}
                {cloud.who.length === 1 ? 'destination' : 'destinations'}:{' '}
                <span className="mono">{cloud.who.join(', ')}</span>.
              </p>
            )}
            {agent.count > 0 && (
              <p className="egress-headline">
                {cloud.count === 0 && <>Auricle sent nothing off this machine. </>}
                Local agents read your transcript <strong>{agent.count}</strong>{' '}
                {agent.count === 1 ? 'time' : 'times'} (
                <span className="mono">{agent.who.join(', ')}</span>) — where those agents sent it
                next is outside Auricle&rsquo;s view.
              </p>
            )}
          </>
        )}
        <p className="dim note">
          Every audio stream, prompt, and summary is logged here with its destination and rough
          size — never its contents. Local providers are recorded too, so silence is never a gap.
          Agent reads are logged the same way: Auricle can attest to what it sent, not to what a
          program that read from it did afterwards.
        </p>
      </section>

      {entries !== null && entries.length > 0 && (
        <section className="panel">
          <h2>Activity</h2>
          <ul className="egress-list">
            {entries.map((e) => (
              <li key={e.id} className="egress-row">
                <span className={`egress-badge ${e.destination}`}>
                  {e.destination === 'cloud'
                    ? 'CLOUD'
                    : e.destination === 'agent'
                      ? 'AGENT'
                      : 'LOCAL'}
                </span>
                <span className="egress-kind">{KIND_LABEL[e.kind] ?? e.kind}</span>
                <span className="egress-dest">
                  {e.destination === 'cloud' ? (
                    <>
                      {'→ '}
                      {e.provider}
                      {e.host && <span className="dim"> ({e.host})</span>}
                    </>
                  ) : e.destination === 'agent' ? (
                    <>
                      {'read by '}
                      <span className="mono">{e.provider}</span>
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
