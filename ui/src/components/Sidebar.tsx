import { Fragment, useCallback, useEffect, useRef, useState } from 'react';
import { api, appendPage, PagedLoads } from '../api';
import { useAuricle } from '../store';
import type { ProviderInfo, SessionSummary, Settings } from '../types';
import { daysAgo, providerName, providerPlace } from '../words';
import { WhereMark } from './TwoVoices';

const SESSION_PAGE = 50;

/** The group heading already says which day, so recent rows only need the time. */
function fmtDate(unixSecs: number): string {
  const at = new Date(unixSecs * 1000);
  const time: Intl.DateTimeFormatOptions = { hour: 'numeric', minute: '2-digit' };
  return daysAgo(unixSecs) <= 1
    ? at.toLocaleTimeString(undefined, time)
    : at.toLocaleDateString(undefined, { month: 'short', day: 'numeric', ...time });
}

/** Sidebar group heading for a session's start time. */
function dayBucket(unixSecs: number): string {
  const days = daysAgo(unixSecs);
  if (days <= 0) return 'Today';
  if (days === 1) return 'Yesterday';
  if (days < 7) return 'This week';
  return 'Earlier';
}

function fmtDuration(s: SessionSummary): string {
  if (s.ended_at === null) return 'Recording';
  const mins = Math.round(Math.max(0, s.ended_at - s.started_at) / 60);
  const length = mins < 60 ? `${mins} min` : `${Math.floor(mins / 60)} h ${mins % 60} min`;
  return s.meta.interrupted === true ? `${length}, cut off` : length;
}

/** A picker option: who transcribes, and where the audio goes as a result. */
export function pickLabel(p: ProviderInfo): string {
  const where = p.kind === 'local' ? 'on this machine' : 'in the cloud';
  const note = p.ready ? '' : firstUseNote(p) ? ', downloads first' : ', not ready';
  return `${providerName(p.id)} ${where}${note}`;
}

/**
 * The provider a session gets with no explicit pick, in the engine's order:
 * settings, then config.
 */
export function serverDefault(list: ProviderInfo[], settings: Settings): string {
  const setting = settings.default_provider;
  return typeof setting === 'string' ? setting : (list.find((p) => p.default)?.id ?? '');
}

/** Not ready, but it fetches its own model: nothing is missing that the user must supply. */
export const firstUseNote = (p: ProviderInfo): boolean => !p.ready && p.kind === 'local';

/** Whether a picker offers this provider. */
export const canPick = (p: ProviderInfo): boolean => p.ready || firstUseNote(p);

/** Name a provider only if the picker was used, so the server default applies. */
export function startBody(pick: string | null): { stt_provider?: string } {
  return pick ? { stt_provider: pick } : {};
}

interface SidebarProps {
  collapsed: boolean;
  onToggle: () => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onDeleted: (id: string) => void;
  onRenamed: (id: string, title: string) => void;
  onNav: (view: 'settings' | 'egress' | 'about') => void;
  activeNav: 'session' | 'settings' | 'egress' | 'about';
  refreshKey: number;
  /** The search box's text, once typing pauses. */
  onSearch: (query: string) => void;
  /** The line search failed, so meetings listed for it can't be trusted. */
  searchFailed: boolean;
}

export function Sidebar({
  collapsed,
  onToggle,
  selectedId,
  onSelect,
  onDeleted,
  onRenamed,
  onNav,
  activeNav,
  refreshKey,
  onSearch,
  searchFailed,
}: SidebarProps) {
  const engineState = useAuricle((s) => s.engineState);
  const activeSession = useAuricle((s) => s.activeSession);
  const sessionsVersion = useAuricle((s) => s.sessionsVersion);
  const settingsVersion = useAuricle((s) => s.settingsVersion);
  const notice = useAuricle((s) => s.notice);
  const startCancelled = useAuricle((s) => s.startCancelled);
  const tokenPrompt = useAuricle((s) => s.tokenPrompt);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [total, setTotal] = useState(0);
  // Rows of the plain list on show. A reload keeps them all, so renaming a
  // meeting far down the list doesn't snap it back to the first page.
  const listRows = useRef(SESSION_PAGE);
  const listLoads = useRef(new PagedLoads());
  const [search, setSearch] = useState('');
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [defaultProvider, setDefaultProvider] = useState('');
  // An override for the next session only; null means the server default.
  const [pick, setPick] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  // Set when a rename ends from the keyboard, so focus returns to that row
  // instead of dropping to the top of the page.
  const refocusRow = useRef<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [listError, setListError] = useState<string | null>(null);

  const reload = useCallback(() => {
    // A search returns every match, so only the plain list pages.
    const load = search.trim()
      ? api.sessions(search).then((list) => ({ sessions: list, total: list.length }))
      : api.sessionPage(listRows.current, 0);
    listLoads.current.replace(
      load,
      (page) => {
        setSessions(page.sessions);
        setTotal(page.total);
        setListError(null);
      },
      (e) => setListError(e.message),
    );
  }, [search]);

  const showMore = () => {
    listLoads.current.extend(
      () => api.sessionPage(SESSION_PAGE, sessions.length),
      (page) => {
        setSessions((shown) => {
          const next = appendPage(shown, page.sessions);
          listRows.current = next.length;
          return next;
        });
        setTotal(page.total);
        setListError(null);
      },
      (e) => setListError(e.message),
    );
  };

  // Refresh on search (debounced), session lifecycle/retitles, and
  // App-driven changes (rename from the header).
  useEffect(() => {
    const t = setTimeout(reload, 200);
    return () => clearTimeout(t);
  }, [reload, sessionsVersion, refreshKey]);

  useEffect(() => {
    const t = setTimeout(() => onSearch(search.trim()), 200);
    return () => clearTimeout(t);
  }, [search, onSearch]);

  // Re-read on every settings or key change. That change is the newer
  // decision, so it also drops a pick made before it: a default switched to
  // local for a confidential call must not lose to a stale cloud pick.
  useEffect(() => {
    setPick(null);
    let stale = false;
    Promise.all([api.providers(), api.settings().catch((): Settings => ({}))])
      .then(([body, settings]) => {
        if (stale) return;
        setProviders(body.providers);
        setDefaultProvider(serverDefault(body.providers, settings));
      })
      .catch(() => {});
    return () => {
      stale = true;
    };
  }, [settingsVersion]);

  const startRecording = async () => {
    setBusy(true);
    setError(null);
    try {
      // No title: the daemon auto-titles the session from its transcript
      // once it stops.
      const { id } = await api.startSession(startBody(pick));
      setPick(null);
      onSelect(id);
    } catch (e) {
      // The engine answers a cancelled start with an error. Nothing went wrong.
      if (!useAuricle.getState().startCancelled) {
        setError(`Recording did not start: ${(e as Error).message}`);
      }
    } finally {
      setBusy(false);
      // The start request has settled either way; a model-download notice
      // must not outlive it (a failed download sends no WS event).
      useAuricle.setState({ notice: null, download: null, startCancelled: false });
    }
  };

  // Stop keeps a start from ever opening capture. The download runs on, and
  // the engine holds the session until it's done, so this only says no to the
  // recording. The start has no id here yet, but the engine lists it as active.
  const cancelStart = async () => {
    useAuricle.setState({ startCancelled: true });
    try {
      const { active_session: id } = await api.health();
      if (id) await api.stopSession(id);
    } catch (e) {
      useAuricle.setState({ startCancelled: false });
      setError(`The recording was not cancelled: ${(e as Error).message}`);
    }
  };

  // A start from another client ends here, with no request of ours to settle.
  useEffect(() => {
    if (notice === null && startCancelled) useAuricle.setState({ startCancelled: false });
  }, [notice, startCancelled]);

  const stopRecording = async () => {
    if (!activeSession) return;
    setBusy(true);
    try {
      await api.stopSession(activeSession);
      useAuricle.getState().markStopping(activeSession);
    } catch (e) {
      setError(`Recording did not stop: ${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  };

  const rename = async (id: string, title: string) => {
    setRenaming(null);
    if (!title.trim()) return;
    try {
      await api.renameSession(id, title.trim());
      onRenamed(id, title.trim());
      reload();
    } catch (e) {
      setError(`The meeting was not renamed: ${(e as Error).message}`);
    }
  };

  const remove = async (id: string, title: string) => {
    if (!window.confirm(`Delete "${title}" and its transcript? This cannot be undone.`)) return;
    try {
      await api.deleteSession(id);
      onDeleted(id);
      reload();
    } catch (e) {
      setError(`The meeting was not deleted: ${(e as Error).message}`);
    }
  };

  const recording = engineState !== 'idle';
  // The only notice is a model download, which holds a start back.
  const preparing = !recording && notice !== null;
  const selected = pick ?? defaultProvider;
  const pickedKind = providers.find((p) => p.id === selected)?.kind;

  const brand = (
    <span className="brand">
      Auricle
      {recording && <span className="tally" aria-hidden="true" />}
    </span>
  );

  // Start and Stop stay on screen however the sidebar is folded.
  const recordButton = recording ? (
    <button
      className="record-btn stop"
      title="Stop recording"
      onClick={stopRecording}
      disabled={busy || engineState === 'stopping'}
    >
      <span className="stop-mark" aria-hidden="true" />
      <span className="record-label">
        {engineState === 'stopping' ? 'Stopping…' : 'Stop recording'}
      </span>
      <span className="record-short" aria-hidden="true">
        Stop
      </span>
    </button>
  ) : preparing ? (
    <button
      className="record-btn pending"
      title={startCancelled ? 'Recording cancelled' : 'Nothing is being recorded yet'}
      onClick={() => void cancelStart()}
      disabled={startCancelled}
    >
      <span className="rec-ring" aria-hidden="true" />
      <span className="record-label">{startCancelled ? 'Finishing download…' : 'Cancel'}</span>
      <span className="record-short" aria-hidden="true">
        {startCancelled ? 'Wait' : 'Cancel'}
      </span>
    </button>
  ) : (
    <button
      className="record-btn"
      title="Start recording"
      onClick={startRecording}
      disabled={busy || !selected}
    >
      <span className="rec-ring" aria-hidden="true" />
      <span className="record-label">Start recording</span>
      <span className="record-short" aria-hidden="true">
        Start
      </span>
    </button>
  );

  if (collapsed) {
    return (
      <aside className="sidebar collapsed">
        {brand}
        {recordButton}
        <button
          className="icon-btn"
          onClick={onToggle}
          title="Show meetings"
          aria-label="Expand sidebar"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="4" y1="7" x2="20" y2="7" /><line x1="4" y1="12" x2="20" y2="12" /><line x1="4" y1="17" x2="20" y2="17" /></svg>
        </button>
        <div className="status-slot" role="status">
          {error && <div className="banner">{error}</div>}
        </div>
      </aside>
    );
  }

  return (
    <aside className={`sidebar ${search.trim() ? 'searching' : ''}`}>
      <div className="sidebar-top">
        {brand}
        <button
          className="icon-btn"
          onClick={onToggle}
          title="Hide meetings"
          aria-label="Collapse sidebar"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="15 18 9 12 15 6" /></svg>
        </button>
      </div>

      <div className="sidebar-search-wrap">
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
        <input
          type="search"
          className="sidebar-search"
          aria-label="Search meetings and transcripts"
          placeholder="Search your meetings"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
      </div>

      <nav className="session-list" aria-label="Meetings">
        <div className="status-slot" role="status">
          {/* While a token is missing every request fails, and the prompt says why. */}
          {listError && tokenPrompt === null && (
            <div className="banner">
              <span>
                Your meetings could not be loaded ({listError}). Try again in a moment. If it keeps
                failing, restart Auricle.
              </span>
              <button className="btn" onClick={reload}>
                Try again
              </button>
            </div>
          )}
        </div>
        {searchFailed && (
          <p className="dim side-empty">Results are hidden until search works again.</p>
        )}
        {!listError && !searchFailed && sessions.length === 0 && (
          <p className="dim side-empty">
            {search ? 'No meetings match.' : 'No meetings yet. Your first one will appear here.'}
          </p>
        )}
        {search.trim() !== '' && !searchFailed && sessions.length > 0 && (
          <p className="side-count">
            In {total} {total === 1 ? 'meeting' : 'meetings'}
          </p>
        )}
        {(searchFailed ? [] : sessions).map((s, i) => (
          <Fragment key={s.id}>
            {(i === 0 || dayBucket(sessions[i - 1].started_at) !== dayBucket(s.started_at)) && (
              <div className="side-label">{dayBucket(s.started_at)}</div>
            )}
          <div
            className={`side-item ${selectedId === s.id && activeNav === 'session' ? 'active' : ''}`}
          >
            {renaming === s.id ? (
              <input
                type="text"
                className="rename-input"
                aria-label="Session title"
                defaultValue={s.title}
                autoFocus
                onBlur={(e) => rename(s.id, e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === 'Escape') refocusRow.current = s.id;
                  if (e.key === 'Enter') {
                    // Focus lands on the row during this keydown; without this
                    // the same Enter would click it and open the session.
                    e.preventDefault();
                    rename(s.id, e.currentTarget.value);
                  }
                  if (e.key === 'Escape') setRenaming(null);
                }}
              />
            ) : (
              <>
                <button
                  className="side-item-main"
                  ref={(el) => {
                    if (el && refocusRow.current === s.id) {
                      refocusRow.current = null;
                      el.focus();
                    }
                  }}
                  onClick={() => onSelect(s.id)}
                >
                  <span className="side-title">
                    {activeSession === s.id && <span className="rec-dot" />} {s.title}
                  </span>
                  <span className="side-meta">
                    <WhereMark place={providerPlace(s.stt_provider)} small />
                    <span>{fmtDate(s.started_at)}</span>
                    <span>{fmtDuration(s)}</span>
                  </span>
                </button>
                <span className="side-actions">
                  <button
                    className="icon-btn"
                    title="Rename"
                    aria-label={`Rename ${s.title}`}
                    onClick={() => setRenaming(s.id)}
                  >
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M12 20h9" /><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" /></svg>
                  </button>
                  <button
                    className="icon-btn danger"
                    title="Delete"
                    aria-label={`Delete ${s.title}`}
                    disabled={activeSession === s.id}
                    onClick={() => remove(s.id, s.title)}
                  >
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
                  </button>
                </span>
              </>
            )}
          </div>
          </Fragment>
        ))}
        {!searchFailed && sessions.length < total && (
          <button className="btn quiet show-more" onClick={showMore}>
            Show more
          </button>
        )}
      </nav>

      <div className="status-slot" role="status">
        {error && <div className="banner">{error}</div>}
      </div>

      <div className="sidebar-bottom">
        {!recording && providers.length > 0 && (
          <>
            {/* One line, because it rarely changes. It can't go further away: what
                the next recording does with your audio has to be in sight of Start. */}
            <label className="provider-pick-wrap">
              <WhereMark
                place={pickedKind === undefined ? undefined : pickedKind === 'local' ? 'local' : 'cloud'}
              />
              <select
                className="provider-pick"
                value={selected}
                onChange={(e) => setPick(e.target.value)}
                aria-label="Transcribe the next meeting with"
                title="Who transcribes the next meeting, and where the audio goes"
              >
                {providers.map((p) => (
                  <option key={p.id} value={p.id} disabled={!canPick(p)}>
                    {pickLabel(p)}
                  </option>
                ))}
              </select>
            </label>
          </>
        )}
        {recordButton}
        <div className="side-nav">
          <button
            className={`side-nav-btn ${activeNav === 'settings' ? 'active' : ''}`}
            onClick={() => onNav('settings')}
          >
            Settings
          </button>
          <button
            className={`side-nav-btn ${activeNav === 'egress' ? 'active' : ''}`}
            onClick={() => onNav('egress')}
          >
            Privacy
          </button>
          <button
            className={`side-nav-btn ${activeNav === 'about' ? 'active' : ''}`}
            onClick={() => onNav('about')}
          >
            About
          </button>
        </div>
      </div>
    </aside>
  );
}
