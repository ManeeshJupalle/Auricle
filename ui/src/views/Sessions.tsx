import { useEffect, useState } from 'react';
import { api } from '../api';
import { Markdown } from '../components/Markdown';
import { useAuricle } from '../store';
import type { LlmProviderInfo, SessionDetail, SessionSummary, TemplateInfo } from '../types';

function fmtDate(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString();
}

function fmtDuration(s: SessionSummary): string {
  if (s.ended_at === null) return 'in progress';
  const secs = Math.max(0, s.ended_at - s.started_at);
  return `${String(Math.floor(secs / 60)).padStart(2, '0')}:${String(secs % 60).padStart(2, '0')}`;
}

function fmtTs(ms: number): string {
  const m = Math.floor(ms / 60_000);
  const s = Math.floor(ms / 1000) % 60;
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
}

export function Sessions() {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<SessionDetail | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [llmProviders, setLlmProviders] = useState<LlmProviderInfo[]>([]);
  const [templates, setTemplates] = useState<TemplateInfo[]>([]);
  const [template, setTemplate] = useState('minutes');
  const [llmProvider, setLlmProvider] = useState('');
  const [summarizing, setSummarizing] = useState(false);
  const engineState = useAuricle((s) => s.engineState);

  // Refresh when a session ends while this view is open.
  useEffect(() => {
    api.sessions().then(setSessions).catch((e) => setLoadError((e as Error).message));
  }, [engineState]);

  useEffect(() => {
    api
      .providers()
      .then((body) => {
        setLlmProviders(body.llm);
        setTemplates(body.templates);
        const ready = body.llm.filter((p) => p.ready);
        const preferred = ready.find((p) => p.default) ?? ready[0];
        if (preferred) setLlmProvider(preferred.id);
      })
      .catch(() => {});
  }, []);

  const open = (id: string) => {
    setLoadError(null);
    api.session(id).then(setSelected).catch((e) => setLoadError((e as Error).message));
  };

  const summarize = async () => {
    if (!selected) return;
    setSummarizing(true);
    setLoadError(null);
    try {
      await api.summarize(selected.id, { template, provider: llmProvider });
      const fresh = await api.session(selected.id);
      setSelected(fresh);
    } catch (e) {
      setLoadError(`summarize failed: ${(e as Error).message}`);
    } finally {
      setSummarizing(false);
    }
  };

  const filtered = sessions.filter((s) =>
    s.title.toLowerCase().includes(search.toLowerCase()),
  );

  if (selected) {
    return (
      <div className="sessions-detail">
        <div className="detail-header panel">
          <button className="btn ghost" onClick={() => setSelected(null)}>
            ← Sessions
          </button>
          <div className="detail-title">
            <h2>{selected.title}</h2>
            <span className="dim">
              {fmtDate(selected.started_at)} · {fmtDuration(selected)} ·{' '}
              {selected.stt_provider}
              {selected.meta['interrupted'] === true && (
                <span className="tag warn"> interrupted</span>
              )}
            </span>
          </div>
          <div className="detail-actions">
            <select value={template} onChange={(e) => setTemplate(e.target.value)}>
              {templates.map((t) => (
                <option key={t.name} value={t.name}>
                  {t.name}
                  {t.overridden ? ' *' : ''}
                </option>
              ))}
            </select>
            <select value={llmProvider} onChange={(e) => setLlmProvider(e.target.value)}>
              {llmProviders.map((p) => (
                <option key={p.id} value={p.id} disabled={!p.ready}>
                  {p.id} ({p.model}){p.ready ? '' : ' — not ready'}
                </option>
              ))}
            </select>
            <button
              className="btn"
              onClick={summarize}
              disabled={summarizing || !llmProvider || selected.transcript.length === 0}
            >
              {summarizing ? 'Summarizing…' : 'Summarize'}
            </button>
            <a className="btn accent" href={api.exportUrl(selected.id)} download={`${selected.id}.md`}>
              Export .md
            </a>
          </div>
        </div>
        {loadError && <div className="banner warn">{loadError}</div>}
        <div className="t-scroll static">
          {selected.transcript.length === 0 && (
            <div className="t-empty">
              <p>No speech was transcribed in this session.</p>
            </div>
          )}
          {selected.transcript.map((seg, i) => (
            <div className="t-row final" key={i}>
              <span className="t-time">{fmtTs(seg.t_start_ms)}</span>
              <span className={`t-speaker ${seg.channel === 0 ? 'mic' : 'loopback'}`}>
                {seg.speaker}
              </span>
              <span className="t-text">{seg.text}</span>
            </div>
          ))}
          {selected.summaries.map((s) => (
            <div className="summary" key={s.id}>
              <div className="summary-head">
                <span className="summary-title">Summary — {s.template}</span>
                <span className="dim">{s.model}</span>
              </div>
              <div className="summary-body">
                <Markdown source={s.content} />
              </div>
            </div>
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className="sessions">
      <div className="panel sessions-toolbar">
        <input
          type="search"
          placeholder="Search by title…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        <span className="dim">
          {filtered.length} of {sessions.length} sessions
        </span>
      </div>
      {loadError && <div className="banner warn">{loadError}</div>}
      {sessions.length === 0 && !loadError && (
        <div className="t-empty">
          <p>No sessions recorded yet.</p>
          <p className="dim">Start one from the Live tab — it will appear here when stopped.</p>
        </div>
      )}
      <div className="sessions-list">
        {filtered.map((s) => (
          <button className="session-card" key={s.id} onClick={() => open(s.id)}>
            <span className="session-title">{s.title}</span>
            <span className="session-meta dim">
              {fmtDate(s.started_at)} · {fmtDuration(s)} · {s.stt_provider}
              {s.meta['interrupted'] === true && <span className="tag warn"> interrupted</span>}
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}
