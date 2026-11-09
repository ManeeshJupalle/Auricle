import { useCallback, useEffect, useState } from 'react';
import { api } from './api';
import { Sidebar } from './components/Sidebar';
import { liveStatus, useAuricle } from './store';
import { downloadLine } from './words';
import { About } from './views/About';
import { Egress } from './views/Egress';
import { Home } from './views/Home';
import { Onboarding } from './views/Onboarding';
import { Search } from './views/Search';
import { SessionView } from './views/SessionView';
import { Settings } from './views/Settings';

type View = 'session' | 'settings' | 'egress' | 'about';

/** At phone width the list is a full sheet, so it starts closed and closes on a pick. */
const phoneWidth = (): boolean => window.matchMedia('(max-width: 640px)').matches;

const CONN_LABEL = { connecting: 'Connecting to Auricle…', reconnecting: 'Reconnecting to Auricle…' };

export function App() {
  const [view, setView] = useState<View>('session');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState(phoneWidth);
  const [refreshKey, setRefreshKey] = useState(0);
  const [renamed, setRenamed] = useState<{ id: string; title: string } | null>(null);
  const [needsOnboarding, setNeedsOnboarding] = useState<boolean | null>(null);
  const [query, setQuery] = useState('');
  // Results hold the main pane until a meeting or page is picked; the query
  // stays, so that meeting can mark what was searched for.
  const [showResults, setShowResults] = useState(false);
  const [moment, setMoment] = useState<{ id: string; ms: number; channel: number } | null>(null);
  const [searchFailed, setSearchFailed] = useState(false);
  const conn = useAuricle((s) => s.conn);
  const activeSession = useAuricle((s) => s.activeSession);
  const droppedPartials = useAuricle((s) => s.droppedPartials);
  const notice = useAuricle((s) => s.notice);
  const download = useAuricle((s) => s.download);
  const status = useAuricle(liveStatus);

  // A newly started session (from the sidebar or another client) takes focus.
  useEffect(() => {
    if (activeSession) {
      setSelectedId(activeSession);
      setView('session');
      setShowResults(false);
    }
  }, [activeSession]);

  const search = useCallback((q: string) => {
    setQuery(q);
    setShowResults(q !== '');
    setSearchFailed(false);
  }, []);

  // First-run gate: show setup until the `onboarded` setting is true. If
  // settings are unreachable, don't block the app.
  useEffect(() => {
    api
      .settings()
      .then((s) => setNeedsOnboarding(s.onboarded !== true))
      .catch(() => setNeedsOnboarding(false));
  }, []);

  const onHome = !showResults && view === 'session' && !selectedId;

  if (needsOnboarding === null) return <div className="app" />;
  if (needsOnboarding) return <Onboarding onDone={() => setNeedsOnboarding(false)} />;

  return (
    <div className={`app ${collapsed ? 'sidebar-collapsed' : ''}`}>
      <div className="sr-only" role="status" aria-live="polite">
        {status}
      </div>
      <Sidebar
        collapsed={collapsed}
        onToggle={() => setCollapsed((c) => !c)}
        selectedId={selectedId}
        onSelect={(id) => {
          setSelectedId(id);
          setView('session');
          setShowResults(false);
          setMoment(null);
          if (phoneWidth()) setCollapsed(true);
        }}
        onDeleted={(id) => {
          if (selectedId === id) setSelectedId(null);
        }}
        onRenamed={(id, title) => {
          setRenamed({ id, title });
          if (id === activeSession) useAuricle.setState({ sessionTitle: title });
        }}
        onNav={(next) => {
          setView(next);
          setShowResults(false);
          if (phoneWidth()) setCollapsed(true);
        }}
        activeNav={view}
        refreshKey={refreshKey}
        onSearch={search}
        searchFailed={searchFailed}
      />
      <main className="main">
        <div className="main-status">
          {/* Home carries the notice as its headline; elsewhere it waits here. */}
          {notice && !onHome && (
            <span className="banner compact">{download ? downloadLine(download) : notice}</span>
          )}
          {droppedPartials > 0 && (
            <span
              className="banner compact"
              title="Live previews were dropped to keep up. The transcript is complete."
            >
              Skipped {droppedPartials} live previews
            </span>
          )}
          {/* Silent when healthy. It only speaks when something is wrong. */}
          {conn !== 'open' && <span className="banner compact">{CONN_LABEL[conn]}</span>}
        </div>
        {showResults ? (
          <Search
            query={query}
            onFailed={setSearchFailed}
            onOpen={({ id, ms, channel }) => {
              setSelectedId(id);
              setView('session');
              setMoment(ms === undefined ? null : { id, ms, channel: channel ?? 1 });
              setShowResults(false);
              if (phoneWidth()) setCollapsed(true);
            }}
          />
        ) : view === 'settings' ? (
          <Settings />
        ) : view === 'egress' ? (
          <Egress />
        ) : view === 'about' ? (
          <About />
        ) : selectedId ? (
          <SessionView
            key={selectedId}
            sessionId={selectedId}
            renamed={renamed}
            onRenamed={() => setRefreshKey((k) => k + 1)}
            query={query}
            moment={moment}
            onBackToResults={() => setShowResults(true)}
            onOpenSettings={() => setView('settings')}
          />
        ) : (
          <Home
            onShowEgress={() => setView('egress')}
            onOpen={(id, ms) => {
              setSelectedId(id);
              setMoment(ms === undefined ? null : { id, ms, channel: 1 });
            }}
          />
        )}
      </main>
    </div>
  );
}
