import { useEffect, useState } from 'react';
import { Sidebar } from './components/Sidebar';
import { useAuricle } from './store';
import { About } from './views/About';
import { SessionView } from './views/SessionView';
import { Settings } from './views/Settings';

type View = 'session' | 'settings' | 'about';

export function App() {
  const [view, setView] = useState<View>('session');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState(false);
  const [refreshKey, setRefreshKey] = useState(0);
  const conn = useAuricle((s) => s.conn);
  const activeSession = useAuricle((s) => s.activeSession);
  const droppedPartials = useAuricle((s) => s.droppedPartials);
  const notice = useAuricle((s) => s.notice);

  // A newly started session (from the sidebar or another client) takes focus.
  useEffect(() => {
    if (activeSession) {
      setSelectedId(activeSession);
      setView('session');
    }
  }, [activeSession]);

  return (
    <div className={`app ${collapsed ? 'sidebar-collapsed' : ''}`}>
      <Sidebar
        collapsed={collapsed}
        onToggle={() => setCollapsed((c) => !c)}
        selectedId={selectedId}
        onSelect={(id) => {
          setSelectedId(id);
          setView('session');
        }}
        onDeleted={(id) => {
          if (selectedId === id) setSelectedId(null);
        }}
        onNav={setView}
        activeNav={view}
        refreshKey={refreshKey}
      />
      <main className="main">
        <div className="main-status">
          {notice && <span className="dim">{notice}</span>}
          {droppedPartials > 0 && (
            <span className="dim mono" title="partials shed for this consumer (finals unaffected)">
              ~{droppedPartials} partials shed
            </span>
          )}
          <span className={`conn ${conn}`} title={`WebSocket: ${conn}`}>
            {conn === 'open' ? 'connected' : conn}
          </span>
        </div>
        {view === 'settings' ? (
          <Settings />
        ) : view === 'about' ? (
          <About />
        ) : selectedId ? (
          <SessionView
            sessionId={selectedId}
            onRenamed={() => setRefreshKey((k) => k + 1)}
          />
        ) : (
          <div className="home-empty">
            <h1>
              auricle<span className="brand-dot">.</span>
            </h1>
            <p className="dim">
              Pick a session from the sidebar, or press <strong>Start Recording</strong> to
              capture your next meeting — your voice is <em>You</em>, everything your computer
              plays is <em>Them</em>.
            </p>
          </div>
        )}
      </main>
    </div>
  );
}
