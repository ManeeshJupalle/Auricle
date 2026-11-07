import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { api, PagedLoads } from '../api';
import { Marked } from '../components/Marked';
import { fmtTs } from '../components/TurnDoc';
import { TapeLegend } from '../components/TwoVoices';
import type { SearchMatch } from '../types';

const SEARCH_PAGE = 50;

function fmtDay(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  });
}

export const matchKey = (m: SearchMatch): string => `${m.session_id}-${m.channel}-${m.t_start_ms}`;

/** The matches by meeting, in the order the server sent them. */
export function byMeeting(matches: SearchMatch[]): SearchMatch[][] {
  const groups = new Map<string, SearchMatch[]>();
  for (const m of matches) {
    const group = groups.get(m.session_id);
    if (group) group.push(m);
    else groups.set(m.session_id, [m]);
  }
  return [...groups.values()];
}

/**
 * Finding a moment. Each match is the line itself, set as it is in the
 * transcript, with its time and its side of the rule, so the words can be
 * recognised before anything is opened. Picking one opens that meeting there.
 */
export function Search({
  query,
  onOpen,
  onFailed,
}: {
  query: string;
  onOpen: (match: { id: string; ms?: number; channel?: number }) => void;
  /** Whether the last load failed, so nothing else keeps showing results for it. */
  onFailed: (failed: boolean) => void;
}) {
  const [matches, setMatches] = useState<SearchMatch[] | null>(null);
  const [total, setTotal] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const loads = useRef(new PagedLoads());

  const load = useCallback(() => {
    setError(null);
    onFailed(false);
    loads.current.replace(
      api.search(query, SEARCH_PAGE, 0),
      (page) => {
        setMatches(page.matches);
        setTotal(page.total);
      },
      (e) => {
        setError(e.message);
        setMatches(null);
        onFailed(true);
      },
    );
  }, [query, onFailed]);

  useEffect(() => {
    const running = loads.current;
    load();
    return () => running.cancel();
  }, [load]);

  const showMore = () => {
    loads.current.extend(
      () => api.search(query, SEARCH_PAGE, matches?.length ?? 0),
      (page) => {
        setMatches((shown) => [...(shown ?? []), ...page.matches]);
        setTotal(page.total);
      },
      (e) => setError(e.message),
    );
  };

  const groups = useMemo(() => byMeeting(matches ?? []), [matches]);

  return (
    <div className="ruled search-view">
      <TapeLegend />
      <div className="status-slot" role="status">
        {error && (
          <div className="banner">
            <span>
              Search did not finish ({error}). Try again in a moment. If it keeps failing, restart
              Auricle.
            </span>
            <button className="btn" onClick={load}>
              Try again
            </button>
          </div>
        )}
      </div>

      {matches === null ? (
        !error && <h1 className="page-title dim">Searching…</h1>
      ) : total === 0 ? (
        <>
          <h1 className="page-title">Nobody said “{query}”</h1>
          <p className="search-lede">
            Search reads the words of every transcript on this machine. Try a shorter word, or part
            of a name.
          </p>
        </>
      ) : (
        <>
          <h1 className="page-title">
            “{query}” came up {total} {total === 1 ? 'time' : 'times'}
          </h1>
          <p className="search-lede">
            {groups.length === 1 ? 'In one meeting. ' : `In ${groups.length} meetings, newest first. `}
            Pick a line to open the meeting there.
          </p>
        </>
      )}

      {groups.map((group) => (
        <section className="hit-group" key={group[0].session_id}>
          <h2>
            <button className="link-btn" onClick={() => onOpen({ id: group[0].session_id })}>
              {group[0].session_title}
            </button>
            <span className="hit-when">{fmtDay(group[0].started_at)}</span>
          </h2>
          <ol className="hit-list">
            {group.map((m) => {
              const voice = m.channel === 0 ? 'mic' : 'loopback';
              return (
                <li className="hit-row" key={matchKey(m)}>
                  <span className={`hit-lane ${voice}`} aria-hidden="true" />
                  <button
                    className={`turn hit ${voice}`}
                    onClick={() =>
                      onOpen({ id: m.session_id, ms: m.t_start_ms, channel: m.channel })
                    }
                  >
                    <span className="turn-time">{fmtTs(m.t_start_ms)}</span>
                    <span className={`turn-speaker ${voice}`}>{m.speaker}</span>
                    <span className="turn-text">
                      <Marked text={m.text} query={query} />
                    </span>
                  </button>
                </li>
              );
            })}
          </ol>
        </section>
      ))}

      {matches !== null && matches.length < total && (
        <button className="btn show-more" onClick={showMore}>
          Show more
        </button>
      )}
    </div>
  );
}
