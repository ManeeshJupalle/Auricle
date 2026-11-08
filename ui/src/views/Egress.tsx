import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { api, appendPage } from '../api';
import { TapeLegend } from '../components/TwoVoices';
import type { EgressEntry, EgressLedger, EgressTotal, SessionSummary } from '../types';
import { providerName } from '../words';

const KIND_LABEL: Record<string, string> = {
  audio: 'Audio',
  prompt: 'Prompt',
  summary: 'Summary',
  title: 'Auto-title',
  dictation: 'Dictation audio',
  dictation_polish: 'Dictation polish',
  live_transcript: 'Live transcript',
  session_list: 'Meeting list',
  session_read: 'Meeting',
  transcript_search: 'Search',
};

export const kindLabel = (kind: string): string => KIND_LABEL[kind] ?? kind;

export const EGRESS_PAGE = 200;

/** Only a short page proves the ledger has ended. */
export const egressHasMore = (pageRows: number): boolean => pageRows >= EGRESS_PAGE;

function fmtTime(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
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
  if (e.kind === 'audio') return '';
  return `${e.items.toLocaleString()} characters`;
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
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [session, setSession] = useState('');
  const [nextOffset, setNextOffset] = useState(0);
  const [hasMore, setHasMore] = useState(false);
  const latest = useRef(0);

  const load = useCallback((scope: string, offset: number) => {
    latest.current += 1;
    const request = latest.current;
    api
      .egress(scope || undefined, EGRESS_PAGE, offset)
      .then((page) => {
        if (request !== latest.current) return;
        setLedger((shown) =>
          shown && offset > 0
            ? { ...page, entries: appendPage(shown.entries, page.entries) }
            : page,
        );
        // Only on success, so Show more after a failure retries the same page.
        setNextOffset(offset + EGRESS_PAGE);
        setHasMore(egressHasMore(page.entries.length));
        setError(null);
      })
      .catch((e) => {
        if (request === latest.current) setError((e as Error).message);
      });
  }, []);

  useEffect(() => {
    load(session, 0);
  }, [session, load]);

  // Only feeds the filter: without it the view still shows the whole ledger.
  useEffect(() => {
    api.sessions().then(setSessions).catch(() => {});
  }, []);

  // Headline counts come from the whole-ledger totals, never from the page
  // of entries below: an agent can make far more reads than a page holds.
  const cloud = tally(ledger?.totals, 'cloud');
  const agent = tally(ledger?.totals, 'agent');
  const entries = ledger?.entries ?? null;
  const titles = useMemo(() => new Map(sessions.map((s) => [s.id, s.title])), [sessions]);
  const meetingOf = (e: EgressEntry): string =>
    e.session_id === null ? 'Across meetings' : (titles.get(e.session_id) ?? 'A deleted meeting');

  return (
    <div className="ruled settings egress">
      {/* The same rule as a meeting's tape, in the same place, now the edge of
          this machine. What stayed sits inside it and what left has crossed. */}
      <TapeLegend left="Stayed" right="Left" />
      <div className="status-slot" role="status">
        {error && <div className="banner">{error}</div>}
      </div>

      <section className="panel egress-hero">
        <h1 className="page-title">What left this machine</h1>
        {entries === null ? (
          <p className="note">{error ? 'The ledger could not be read.' : 'Loading…'}</p>
        ) : cloud.count === 0 && agent.count === 0 ? (
          <p className="egress-headline ok">
            {entries.length === 0
              ? 'Nothing has left this machine, and there is nothing on record yet.'
              : 'Nothing has left this machine. Everything on record stayed here.'}
          </p>
        ) : (
          <>
            {cloud.count > 0 && (
              <p className="egress-headline">
                Data has left this machine {cloud.count} {cloud.count === 1 ? 'time' : 'times'},
                to {cloud.who.join(' and ')}.
              </p>
            )}
            {agent.count > 0 && (
              <p className="egress-headline">
                {cloud.count === 0 && <>Auricle sent nothing off this machine. </>}
                Local agents read your transcript {agent.count}{' '}
                {agent.count === 1 ? 'time' : 'times'} ({agent.who.join(', ')}). Where they sent it
                next is out of Auricle&rsquo;s sight.
              </p>
            )}
          </>
        )}
        <p className="note">
          Every audio stream, prompt, summary, and auto-title is listed here with where it went and
          roughly how big it was, never what it said. Work done on this machine is listed too, so a
          quiet ledger is never a gap. Auricle can vouch for what it sent, not for what a program
          that read from it did afterwards.
        </p>
      </section>

      {entries !== null && (entries.length > 0 || session !== '') && (
        <section className="panel">
          <div className="egress-activity-head">
            <h2>Everything on record</h2>
            {sessions.length > 0 && (
              <select
                aria-label="Show the record for"
                value={session}
                onChange={(e) => setSession(e.target.value)}
              >
                <option value="">All meetings</option>
                {sessions.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.title}, {fmtTime(s.started_at)}
                  </option>
                ))}
              </select>
            )}
          </div>
          {entries.length === 0 && <p className="note">Nothing on record for this meeting.</p>}
          {entries.length > 0 && (
            <ul className="egress-key" aria-label="What the marks mean">
              <li>
                <span className="egress-lane local" aria-hidden="true">
                  <i />
                </span>
                Stayed on this machine
              </li>
              <li>
                <span className="egress-lane agent" aria-hidden="true">
                  <i />
                </span>
                Read by an agent on this machine
              </li>
              <li>
                <span className="egress-lane cloud" aria-hidden="true">
                  <i />
                </span>
                Sent off this machine
              </li>
            </ul>
          )}
          <ul className="egress-list">
            {entries.map((e) => (
              <li key={e.id} className={`egress-row ${e.destination}`}>
                <span className={`egress-lane ${e.destination}`} aria-hidden="true">
                  <i />
                </span>
                <span className="egress-what">
                  <span className="egress-kind">{kindLabel(e.kind)}</span>{' '}
                  {e.destination === 'cloud' ? (
                    <>
                      sent to {providerName(e.provider)}
                      {e.host && <> ({e.host})</>}
                    </>
                  ) : e.destination === 'agent' ? (
                    <>read by {e.provider}</>
                  ) : (
                    <>stayed here ({providerName(e.provider)})</>
                  )}
                </span>
                <span className="egress-meeting">{meetingOf(e)}</span>
                <span className="egress-time">{fmtTime(e.ts)}</span>
                <span className="egress-size">{fmtSize(e)}</span>
              </li>
            ))}
          </ul>
          {hasMore && (
            <button className="btn egress-more" onClick={() => load(session, nextOffset)}>
              Show more
            </button>
          )}
        </section>
      )}
    </div>
  );
}
