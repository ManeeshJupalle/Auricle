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
          ☰
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
          ⟨
        </button>
      </div>

      <input
        type="search"
        className="sidebar-search"
        placeholder="Search sessions & transcripts…"
        value={search}
        onChange={(e) => setSearch(e.target.value)}
      />

      <nav className="session-list">
        {sessions.length === 0 && (
          <p className="dim side-empty">
            {search ? 'No matches.' : 'No sessions yet — record your first below.'}
          </p>
        )}
        {sessions.map((s) => (
          <div
            key={s.id}
            className={`side-item ${selectedId === s.id && activeNav === 'session' ? 'active' : ''} ${
              activeSession === s.id ? 'recording' : ''
            }`}
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
                    {fmtDate(s.started_at)} · {fmtDuration(s)}
                  </span>
                </button>
                <span className="side-actions">
                  <button className="icon-btn" title="Rename" onClick={() => setRenaming(s.id)}>
                    ✎
                  </button>
                  <button
                    className="icon-btn danger"
                    title="Delete"
                    disabled={activeSession === s.id}
                    onClick={() => remove(s.id, s.title)}
                  >
                    🗑
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
            <button className="record-btn" onClick={startRecording} disabled={busy || !provider}>
              ● Start Recording
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
