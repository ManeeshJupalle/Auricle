import { useEffect, useMemo, useRef, useState } from 'react';
import { api } from '../api';
import { AudioBar, type AudioBarHandle } from '../components/AudioBar';
import { Markdown } from '../components/Markdown';
import { TurnDoc } from '../components/TurnDoc';
import { VuMeter } from '../components/VuMeter';
import { WaveRibbon } from '../components/WaveRibbon';
import { buildTurns, useAuricle, type Row } from '../store';

const reducedMotion =
  typeof window.matchMedia === 'function' &&
  window.matchMedia('(prefers-reduced-motion: reduce)').matches;
import type { LlmProviderInfo, SessionDetail, TemplateInfo } from '../types';

function fmtLongDate(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    weekday: 'short',
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

function fmtDur(secs: number): string {
  return `${String(Math.floor(secs / 60)).padStart(2, '0')}:${String(Math.floor(secs) % 60).padStart(2, '0')}`;
}

function ClockIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="10" />
      <polyline points="12 6 12 12 16 14" />
    </svg>
  );
}

function LiveTicker({ startedAtMs }: { startedAtMs: number | null }) {
  const [, force] = useState(0);
  useEffect(() => {
    const t = setInterval(() => force((n) => n + 1), 1000);
    return () => clearInterval(t);
  }, []);
  if (!startedAtMs) return null;
  return <span>{fmtDur((Date.now() - startedAtMs) / 1000)}</span>;
}

export function SessionView({
  sessionId,
  onRenamed,
}: {
  sessionId: string;
  onRenamed: () => void;
}) {
  const activeSession = useAuricle((s) => s.activeSession);
  const engineState = useAuricle((s) => s.engineState);
  const liveTurnOrder = useAuricle((s) => s.turnOrder);
  const liveStartedAtMs = useAuricle((s) => s.liveStartedAtMs);
  const sessionTitleLive = useAuricle((s) => s.sessionTitle);
  const sessionProvider = useAuricle((s) => s.sessionProvider);
  const latencyMs = useAuricle((s) => s.latencyMs);
  const deviceLost = useAuricle((s) => s.deviceLost);
  const lastError = useAuricle((s) => s.lastError);

  const sessionsVersion = useAuricle((s) => s.sessionsVersion);
  const isLive = activeSession === sessionId;
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [tab, setTab] = useState<'transcript' | 'summary'>('transcript');
  const [editingTitle, setEditingTitle] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const audioRef = useRef<AudioBarHandle>(null);

  // Summary tab state.
  const [llmProviders, setLlmProviders] = useState<LlmProviderInfo[]>([]);
  const [templates, setTemplates] = useState<TemplateInfo[]>([]);
  const [template, setTemplate] = useState('minutes');
  const [llmProvider, setLlmProvider] = useState('');
  const [summarizing, setSummarizing] = useState(false);

  useEffect(() => {
    setTab('transcript');
    setDetail(null);
    setError(null);
    if (!isLive) {
      api.session(sessionId).then(setDetail).catch((e) => setError((e as Error).message));
    }
  }, [sessionId, isLive]);

  // When the session we're watching live gets stopped — or its auto-title
  // lands (session_updated bumps sessionsVersion) — fetch the stored form.
  useEffect(() => {
    if (!isLive && engineState === 'idle') {
      api.session(sessionId).then(setDetail).catch(() => {});
    }
  }, [isLive, engineState, sessionId, sessionsVersion]);

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

  const staticDoc = useMemo(() => {
    if (!detail) return null;
    const rows: Record<string, Row> = {};
    const order: string[] = [];
    detail.transcript.forEach((seg, i) => {
      const channel = seg.channel === 0 ? 'mic' : 'loopback';
      const id = `s-${i}`;
      order.push(id);
      rows[id] = {
        id,
        channel,
        speaker: seg.speaker,
        tStartMs: seg.t_start_ms,
        tEndMs: seg.t_end_ms,
        text: seg.text,
        final: true,
      };
    });
    return { rows, ...buildTurns(order, rows) };
  }, [detail]);

  const hasAudio = !isLive && detail?.meta != null && typeof detail.meta === 'object'
    && (detail.meta as Record<string, unknown>)['audio'] != null;

  const title = isLive ? (sessionTitleLive ?? 'Recording…') : (detail?.title ?? '…');

  const saveTitle = async (value: string) => {
    setEditingTitle(false);
    const next = value.trim();
    if (!next || next === title) return;
    try {
      await api.renameSession(sessionId, next);
      if (detail) setDetail({ ...detail, title: next });
      if (isLive) useAuricle.setState({ sessionTitle: next });
      onRenamed();
    } catch (e) {
      setError(`rename failed: ${(e as Error).message}`);
    }
  };

  const summarize = async () => {
    setSummarizing(true);
    setError(null);
    try {
      await api.summarize(sessionId, { template, provider: llmProvider });
      const fresh = await api.session(sessionId);
      setDetail(fresh);
    } catch (e) {
      setError(`summarize failed: ${(e as Error).message}`);
    } finally {
      setSummarizing(false);
    }
  };

  return (
    <div className="session-view">
      <header className="session-header">
        <div className="session-title-row">
          {editingTitle ? (
            <input
              className="title-input"
              type="text"
              defaultValue={title}
              autoFocus
              onBlur={(e) => saveTitle(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') saveTitle(e.currentTarget.value);
                if (e.key === 'Escape') setEditingTitle(false);
              }}
            />
          ) : (
            <h1
              className="session-title"
              title="Click to rename"
              onClick={() => setEditingTitle(true)}
            >
              {title}
            </h1>
          )}
          {isLive && (
            <span className="rec">
              <span className="rec-dot" /> REC
            </span>
          )}
        </div>
        <div className="session-meta-row">
          {isLive ? (
            <>
              <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}>
                <ClockIcon />
                <LiveTicker startedAtMs={liveStartedAtMs} />
              </span>
              <span style={{ opacity: 0.45 }}>·</span>
              <span className="meta-chip">{sessionProvider}</span>
              {latencyMs !== null && (
                <>
                  <span style={{ opacity: 0.45 }}>·</span>
                  <span>latency {(latencyMs / 1000).toFixed(1)}s</span>
                </>
              )}
              {reducedMotion && (
                <div className="header-meters">
                  <VuMeter channel="mic" label="You" />
                  <VuMeter channel="loopback" label="Them" />
                </div>
              )}
            </>
          ) : (
            detail && (
              <>
                <span>{fmtLongDate(detail.started_at)}</span>
                <span style={{ opacity: 0.45 }}>·</span>
                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}>
                  <ClockIcon />
                  {detail.ended_at ? fmtDur(detail.ended_at - detail.started_at) : 'in progress'}
                </span>
                <span style={{ opacity: 0.45 }}>·</span>
                <span className="meta-chip">{detail.stt_provider}</span>
                {(detail.meta as Record<string, unknown>)['interrupted'] === true && (
                  <span className="tag warn">interrupted</span>
                )}
                <span style={{ opacity: 0.45 }}>·</span>
                <a
                  className="header-export"
                  href={api.exportUrl(sessionId)}
                  download={`${sessionId}.md`}
                >
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><polyline points="7 10 12 15 17 10" /><line x1="12" y1="15" x2="12" y2="3" /></svg>
                  Export .md
                </a>
              </>
            )
          )}
        </div>
        {isLive && !reducedMotion && <WaveRibbon />}
        <div className="tabs">
          <button
            className={`tab ${tab === 'transcript' ? 'active' : ''}`}
            onClick={() => setTab('transcript')}
          >
            Transcript
          </button>
          <button
            className={`tab ${tab === 'summary' ? 'active' : ''}`}
            onClick={() => setTab('summary')}
            disabled={isLive}
            title={isLive ? 'Available after the session stops' : undefined}
          >
            AI Summary
          </button>
        </div>
      </header>

      {deviceLost && isLive && <div className="banner error">audio device lost — {deviceLost}</div>}
      {(error ?? (isLive ? lastError : null)) && (
        <div className="banner warn">{error ?? lastError}</div>
      )}

      {tab === 'transcript' ? (
        <div className="doc-wrap">
          {isLive ? (
            <TurnDoc
              turnOrder={liveTurnOrder}
              live
              follow
              emptyHint="Listening — speak, or play audio. Partials appear dim and settle into place."
            />
          ) : staticDoc ? (
            <TurnDoc
              turnOrder={staticDoc.turnOrder}
              turns={staticDoc.turns}
              rows={staticDoc.rows}
              live={false}
              onSeek={hasAudio ? (ms) => audioRef.current?.seek(ms) : undefined}
              emptyHint="No speech was transcribed in this session."
            />
          ) : (
            <div className="doc-empty">
              <p className="dim">Loading…</p>
            </div>
          )}
          {hasAudio && <AudioBar ref={audioRef} sessionId={sessionId} />}
        </div>
      ) : (
        <div className="summary-pane">
          <div className="summary-controls">
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
              className="btn accent"
              onClick={summarize}
              disabled={summarizing || !llmProvider || !detail || detail.transcript.length === 0}
            >
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M5 3v4" /><path d="M19 17v4" /><path d="M3 5h4" /><path d="M17 19h4" /><path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9L12 3Z" /></svg>
              {summarizing ? 'Summarizing…' : 'Generate summary'}
            </button>
          </div>
          <div className="summary-scroll">
            {detail?.summaries.length === 0 && (
              <p className="dim">
                No summaries yet — pick a template and provider, then generate one.
              </p>
            )}
            {detail?.summaries.map((s) => (
              <div className="summary" key={s.id}>
                <div className="summary-head">
                  <span className="summary-title">{s.template}</span>
                  <span className="summary-model">{s.model}</span>
                </div>
                <div className="summary-body">
                  <Markdown source={s.content} />
                </div>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
