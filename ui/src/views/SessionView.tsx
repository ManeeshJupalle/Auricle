import { useEffect, useMemo, useRef, useState } from 'react';
import { api } from '../api';
import { AudioBar, type AudioBarHandle } from '../components/AudioBar';
import { Markdown } from '../components/Markdown';
import { TurnDoc } from '../components/TurnDoc';
import { VuMeter } from '../components/VuMeter';
import { buildTurns, useAuricle, type Row } from '../store';
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
              <LiveTicker startedAtMs={liveStartedAtMs} />
              <span className="dim">· {sessionProvider}</span>
              <span className="dim mono">
                {latencyMs !== null ? `· latency ${(latencyMs / 1000).toFixed(1)}s` : ''}
              </span>
              <div className="header-meters">
                <VuMeter channel="mic" label="You" />
                <VuMeter channel="loopback" label="Them" />
              </div>
            </>
          ) : (
            detail && (
              <>
                <span>{fmtLongDate(detail.started_at)}</span>
                <span className="dim">
                  · {detail.ended_at ? fmtDur(detail.ended_at - detail.started_at) : 'in progress'}
                </span>
                <span className="dim">· {detail.stt_provider}</span>
                {(detail.meta as Record<string, unknown>)['interrupted'] === true && (
                  <span className="tag warn">interrupted</span>
                )}
                <a
                  className="header-export"
                  href={api.exportUrl(sessionId)}
                  download={`${sessionId}.md`}
                >
                  Export .md
                </a>
              </>
            )
          )}
        </div>
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
                  <span className="dim">{s.model}</span>
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
