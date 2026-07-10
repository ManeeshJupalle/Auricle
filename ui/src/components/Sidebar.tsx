import { useCallback, useEffect, useState } from 'react';
import { api } from '../api';
import { useAuricle } from '../store';
import type { ProviderInfo, SessionSummary } from '../types';

function fmtDate(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

function fmtDuration(s: SessionSummary): string {
  if (s.ended_at === null) return 'live';
  const secs = Math.max(0, s.ended_at - s.started_at);
  return `${String(Math.floor(secs / 60)).padStart(2, '0')}:${String(secs % 60).padStart(2, '0')}`;
}

interface SidebarProps {
  collapsed: boolean;
  onToggle: () => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onDeleted: (id: string) => void;
  onNav: (view: 'settings' | 'about') => void;
  activeNav: 'session' | 'settings' | 'about';
  refreshKey: number;
}

export function Sidebar({
  collapsed,
  onToggle,
  selectedId,
  onSelect,
  onDeleted,
  onNav,
  activeNav,
  refreshKey,
}: SidebarProps) {
  const engineState = useAuricle((s) => s.engineState);
  const activeSession = useAuricle((s) => s.activeSession);
  const sessionsVersion = useAuricle((s) => s.sessionsVersion);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [search, setSearch] = useState('');
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [provider, setProvider] = useState('');
  const [renaming, setRenaming] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    api
      .sessions(search)
      .then(setSessions)
      .catch(() => {});
  }, [search]);

  // Refresh on search (debounced), session lifecycle/retitles, and
  // App-driven changes (rename from the header).
  useEffect(() => {
    const t = setTimeout(reload, 200);
    return () => clearTimeout(t);
  }, [reload, sessionsVersion, refreshKey]);

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
      .catch(() => {});
  }, []);

  const startRecording = async () => {
    setBusy(true);
    setError(null);
    try {
      // No title: the daemon auto-titles the session from its transcript
      // once it stops.
      const { id } = await api.startSession({
        stt_provider: provider || undefined,
      });
      onSelect(id);
    } catch (e) {
      setError(`start failed: ${(e as Error).message}`);
    } finally {
      setBusy(false);
      // The start request has settled either way; a model-download notice
      // must not outlive it (a failed download sends no WS event).
      useAuricle.setState({ notice: null });
    }
  };

  const stopRecording = async () => {
    if (!activeSession) return;
    setBusy(true);
    try {
      await api.stopSession(activeSession);
    } catch (e) {
      setError(`stop failed: ${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  };

  const rename = async (id: string, title: string) => {
    setRenaming(null);
    if (!title.trim()) return;
    try {
      await api.renameSession(id, title.trim());
      reload();
    } catch (e) {
      setError(`rename failed: ${(e as Error).message}`);
    }
  };

  const remove = async (id: string, title: string) => {
    if (!window.confirm(`Delete "${title}" and its transcript? This cannot be undone.`)) return;
    try {
      await api.deleteSession(id);
      onDeleted(id);
      reload();
    } catch (e) {
      setError(`delete failed: ${(e as Error).message}`);
    }
  };

  if (collapsed) {
    return (
      <aside className="sidebar collapsed">
        <button className="icon-btn" onClick={onToggle} title="Expand sidebar">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="9 18 15 12 9 6" /></svg>
        </button>
      </aside>
    );
  }

  const recording = engineState !== 'idle';

  return (
    <aside className="sidebar">
      <div className="sidebar-top">
        <span className="brand">
          auricle<span className="brand-dot">.</span>
        </span>
        <button className="icon-btn" onClick={onToggle} title="Collapse sidebar">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="15 18 9 12 15 6" /></svg>
        </button>
      </div>

      <div className="sidebar-search-wrap">
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
        <input
          type="search"
          className="sidebar-search"
          placeholder="Search sessions & transcripts…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
      </div>

      <nav className="session-list">
        <div className="side-label">Recent sessions</div>
        {sessions.length === 0 && (
          <p className="dim side-empty">
            {search ? 'No matches.' : 'No sessions yet — record your first below.'}
          </p>
        )}
        {sessions.map((s) => (
          <div
            key={s.id}
            className={`side-item ${selectedId === s.id && activeNav === 'session' ? 'active' : ''}`}
          >
            {renaming === s.id ? (
              <input
                type="text"
                className="rename-input"
                defaultValue={s.title}
                autoFocus
                onBlur={(e) => rename(s.id, e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') rename(s.id, e.currentTarget.value);
                  if (e.key === 'Escape') setRenaming(null);
                }}
              />
            ) : (
              <>
                <button className="side-item-main" onClick={() => onSelect(s.id)}>
                  <span className="side-title">
                    {activeSession === s.id && <span className="rec-dot" />} {s.title}
                  </span>
                  <span className="side-meta">
                    <span>{fmtDate(s.started_at)}</span>
                    <span style={{ opacity: 0.5 }}>·</span>
                    <span>{fmtDuration(s)}</span>
                  </span>
                </button>
                <span className="side-actions">
                  <button className="icon-btn" title="Rename" onClick={() => setRenaming(s.id)}>
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M12 20h9" /><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" /></svg>
                  </button>
                  <button
                    className="icon-btn danger"
                    title="Delete"
                    disabled={activeSession === s.id}
                    onClick={() => remove(s.id, s.title)}
                  >
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
                  </button>
                </span>
              </>
            )}
          </div>
        ))}
      </nav>

      {error && <div className="side-error">{error}</div>}

      <div className="sidebar-bottom">
        {recording ? (
          <button
            className="record-btn stop"
            onClick={stopRecording}
            disabled={busy || engineState === 'stopping'}
          >
            {engineState === 'stopping' ? 'Stopping…' : '■ Stop Recording'}
          </button>
        ) : (
          <>
            <div className="provider-pick-wrap">
              <svg className="left" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" /><path d="M19 10v2a7 7 0 0 1-14 0v-2" /><line x1="12" y1="19" x2="12" y2="22" /></svg>
              <select
                className="provider-pick"
                value={provider}
                onChange={(e) => setProvider(e.target.value)}
                title="STT provider for the next session"
              >
                {providers.map((p) => (
                  <option key={p.id} value={p.id} disabled={!p.ready}>
                    {p.id}
                    {p.ready ? '' : ' (not ready)'}
                  </option>
                ))}
              </select>
              <svg className="right" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="6 9 12 15 18 9" /></svg>
            </div>
            <button className="record-btn" onClick={startRecording} disabled={busy || !provider}>
              <span className="rec-pulse" />
              Start Recording
            </button>
          </>
        )}
        <div className="side-nav">
          <button
            className={`side-nav-btn ${activeNav === 'settings' ? 'active' : ''}`}
            onClick={() => onNav('settings')}
          >
            Settings
          </button>
          <span className="side-nav-divider" />
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
