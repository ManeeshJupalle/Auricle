import { useEffect, useState } from 'react';
import { api } from './api';
import { Sidebar } from './components/Sidebar';
import { useAuricle } from './store';
import { About } from './views/About';
import { Egress } from './views/Egress';
import { Home } from './views/Home';
import { Onboarding } from './views/Onboarding';
import { SessionView } from './views/SessionView';
import { Settings } from './views/Settings';

type View = 'session' | 'settings' | 'egress' | 'about';

export function App() {
  const [view, setView] = useState<View>('session');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState(false);
  const [refreshKey, setRefreshKey] = useState(0);
  const [needsOnboarding, setNeedsOnboarding] = useState<boolean | null>(null);
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

  // First-run gate: show setup until the `onboarded` setting is true. If
  // settings are unreachable, don't block the app.
  useEffect(() => {
    api
      .settings()
      .then((s) => setNeedsOnboarding(s.onboarded !== true))
      .catch(() => setNeedsOnboarding(false));
  }, []);

  if (needsOnboarding === null) return <div className="app" />;
  if (needsOnboarding) return <Onboarding onDone={() => setNeedsOnboarding(false)} />;

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
          {/* Quiet when healthy: a bare dot. Loud only when it matters. */}
          <span className={`conn ${conn}`} title={`WebSocket: ${conn}`}>
            {conn === 'open' ? '' : conn}
          </span>
        </div>
        {view === 'settings' ? (
          <Settings />
        ) : view === 'egress' ? (
          <Egress />
        ) : view === 'about' ? (
          <About />
        ) : selectedId ? (
          <SessionView
            sessionId={selectedId}
            onRenamed={() => setRefreshKey((k) => k + 1)}
          />
        ) : (
          <Home onShowEgress={() => setView('egress')} />
        )}
      </main>
    </div>
  );
}
