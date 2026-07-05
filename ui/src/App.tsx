import { useState } from 'react';
import { useAuricle } from './store';
import { Live } from './views/Live';
import { Sessions } from './views/Sessions';
import { Settings } from './views/Settings';

type View = 'live' | 'sessions' | 'settings';

export function App() {
  const [view, setView] = useState<View>('live');
  const conn = useAuricle((s) => s.conn);
  const engineState = useAuricle((s) => s.engineState);
  const droppedPartials = useAuricle((s) => s.droppedPartials);

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">
          auricle<span className="brand-dot">.</span>
        </span>
        <nav>
          {(['live', 'sessions', 'settings'] as View[]).map((v) => (
            <button
              key={v}
              className={`nav-btn ${view === v ? 'active' : ''}`}
              onClick={() => setView(v)}
            >
              {v}
            </button>
          ))}
        </nav>
        <div className="topbar-right">
          {engineState === 'recording' && (
            <span className="rec">
              <span className="rec-dot" /> REC
            </span>
          )}
          {droppedPartials > 0 && (
            <span className="dim" title="partials shed for this consumer (finals unaffected)">
              ~{droppedPartials} partials shed
            </span>
          )}
          <span className={`conn ${conn}`} title={`WebSocket: ${conn}`}>
            {conn === 'open' ? 'connected' : conn}
          </span>
        </div>
      </header>
      <main>
        {view === 'live' && <Live />}
        {view === 'sessions' && <Sessions />}
        {view === 'settings' && <Settings />}
      </main>
    </div>
  );
}
