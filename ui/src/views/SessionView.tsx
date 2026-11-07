import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Markdown } from '@shared/Markdown';
import { api, appendPage, PagedLoads } from '../api';
import { readToken } from '../auth';
import { ASK_PAGE, AskList, confirmDeleteAsk } from '../components/AskList';
import { AudioBar, type AudioBarHandle } from '../components/AudioBar';
import { LiveTape } from '../components/LiveTape';
import { hasMatch } from '../components/Marked';
import {
  buildMapTurns,
  SessionMap,
  turnAt,
  type MapHit,
  type MapTurn,
  type SessionMapHandle,
} from '../components/SessionMap';
import { fmtTs, TurnDoc, type TurnDocHandle } from '../components/TurnDoc';
import { TapeLegend, WhereMark } from '../components/TwoVoices';
import { VuMeter } from '../components/VuMeter';
import { buildTurns, useAuricle, type Row } from '../store';
import {
  providerName,
  providerPlace,
  providerWhere,
  templateName,
  withoutRepeatedTitle,
} from '../words';

const reducedMotion =
  typeof window.matchMedia === 'function' &&
  window.matchMedia('(prefers-reduced-motion: reduce)').matches;
import type { AskInfo, LlmProviderInfo, SessionDetail, TemplateInfo } from '../types';

function fmtLongDate(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString(undefined, {
    weekday: 'short',
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  });
}

function fmtDur(secs: number): string {
  return `${String(Math.floor(secs / 60)).padStart(2, '0')}:${String(Math.floor(secs) % 60).padStart(2, '0')}`;
}

/** A finished meeting's length, in the words a person would use. */
function fmtLength(secs: number): string {
  const mins = Math.round(secs / 60);
  if (mins < 1) return 'under a minute';
  if (mins < 60) return `${mins} ${mins === 1 ? 'minute' : 'minutes'}`;
  const rest = mins % 60;
  return `${Math.floor(mins / 60)} h ${rest} min`;
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
  renamed,
  onRenamed,
  query,
  moment,
  onBackToResults,
  onOpenSettings,
}: {
  sessionId: string;
  /** The latest rename from the sidebar; PATCH broadcasts nothing to pick up. */
  renamed: { id: string; title: string } | null;
  onRenamed: () => void;
  /** What the search box holds, marked in the text and on the tape. */
  query: string;
  /** The moment to open at, when a search result or the home tape asked for one. */
  moment: { id: string; ms: number; channel: number } | null;
  onBackToResults: () => void;
  onOpenSettings: () => void;
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
  const [tab, setTab] = useState<'transcript' | 'summary' | 'asks'>('transcript');
  const [asks, setAsks] = useState<AskInfo[]>([]);
  const [moreAsks, setMoreAsks] = useState(false);
  const askLoads = useRef(new PagedLoads());
  const [editingTitle, setEditingTitle] = useState(false);
  // Set when an edit ends from the keyboard, so focus returns to the title
  // instead of dropping to the top of the page.
  const refocusTitle = useRef(false);
  const [error, setError] = useState<string | null>(null);
  const audioRef = useRef<AudioBarHandle>(null);
  const docRef = useRef<TurnDocHandle>(null);
  const mapRef = useRef<SessionMapHandle>(null);
  // A jump from another tab, held until the transcript mounts.
  const pendingJump = useRef<{ turn: MapTurn; play: boolean } | null>(null);
  const [foundTurn, setFoundTurn] = useState<number | null>(null);
  // Scrolled into the text. At phone width the title and meta give up their room.
  const [reading, setReading] = useState(false);
  const [currentTurn, setCurrentTurn] = useState<number | null>(null);
  // The transcript keeps up with playback until the reader scrolls away.
  const [following, setFollowing] = useState(true);

  // Summary tab state.
  const [llmProviders, setLlmProviders] = useState<LlmProviderInfo[]>([]);
  const [templates, setTemplates] = useState<TemplateInfo[]>([]);
  const template = useAuricle((s) => s.summaryTemplate);
  const llmPick = useAuricle((s) => s.summaryProvider);
  const [llmDefault, setLlmDefault] = useState('');
  const llmProvider = llmPick ?? llmDefault;
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

  // No stored asks is the usual case, so a failure here just leaves the tab out.
  useEffect(() => {
    setAsks([]);
    setMoreAsks(false);
    const loads = askLoads.current;
    if (!isLive) {
      loads.replace(
        api.asks(sessionId, ASK_PAGE, 0),
        (page) => {
          setAsks(page);
          setMoreAsks(page.length >= ASK_PAGE);
        },
        () => {},
      );
    }
    return () => loads.cancel();
  }, [sessionId, isLive]);

  useEffect(() => {
    api
      .providers()
      .then((body) => {
        setLlmProviders(body.llm);
        setTemplates(body.templates);
        const ready = body.llm.filter((p) => p.ready);
        const preferred = ready.find((p) => p.default) ?? ready[0];
        if (preferred) setLlmDefault(preferred.id);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (renamed?.id === sessionId) setDetail((d) => d && { ...d, title: renamed.title });
  }, [renamed, sessionId]);

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

  // The session map spans the recording, so it needs the wall-clock
  // duration — a transcript that ends early still leaves trailing silence.
  const mapTurns = useMemo(
    () => (staticDoc ? buildMapTurns(staticDoc.turnOrder, staticDoc.turns, staticDoc.rows) : []),
    [staticDoc],
  );
  const durationMs = useMemo(() => {
    if (!detail?.ended_at) return 0;
    const wall = (detail.ended_at - detail.started_at) * 1000;
    return Math.max(wall, mapTurns[mapTurns.length - 1]?.endMs ?? 0);
  }, [detail, mapTurns]);

  const showReading = useCallback(
    (first: number, last: number) => mapRef.current?.showWindow(mapTurns[first], mapTurns[last]),
    [mapTurns],
  );

  const jumpTo = (t: MapTurn, play: boolean) => {
    docRef.current?.scrollToTurn(t.index);
    if (play && hasAudio) audioRef.current?.seek(t.startMs);
  };

  const openAt = (t: MapTurn, play: boolean) => {
    setFollowing(true);
    if (tab === 'transcript') {
      jumpTo(t, play);
    } else {
      pendingJump.current = { turn: t, play };
      setTab('transcript');
    }
  };

  /** A moment picked anywhere on the line, between blocks included. */
  const seekOnLine = (ms: number) => {
    const turn = mapTurns[Math.max(0, turnAt(mapTurns, ms))];
    if (!turn) return;
    if (tab === 'transcript') {
      openAt(turn, false);
      if (hasAudio) audioRef.current?.seek(ms);
    } else {
      openAt(turn, true);
    }
  };

  // Called several times a second, so the map is written to directly and the
  // turn only changes state when playback crosses into a new one.
  const followAudio = useCallback(
    (ms: number) => {
      mapRef.current?.showPlayhead(ms);
      const index = turnAt(mapTurns, ms);
      if (ms > 0 && index >= 0) setCurrentTurn(index);
    },
    [mapTurns],
  );

  useEffect(() => {
    if (following && currentTurn !== null) docRef.current?.scrollToTurn(currentTurn);
  }, [following, currentTurn]);

  useEffect(() => {
    if (tab !== 'transcript' || !pendingJump.current) return;
    jumpTo(pendingJump.current.turn, pendingJump.current.play);
    pendingJump.current = null;
  });

  // Opened at a moment: land on that turn without starting playback. A search
  // wants the words, and sound out of nowhere would be a surprise.
  useEffect(() => {
    if (moment?.id !== sessionId || mapTurns.length === 0) return;
    const voice = moment.channel === 0 ? 'mic' : 'loopback';
    const inTurn = (t: MapTurn) => moment.ms >= t.startMs && moment.ms <= t.endMs;
    const index = Math.max(
      mapTurns.findIndex((t) => t.channel === voice && inTurn(t)),
      mapTurns.findIndex(inTurn),
    );
    if (index < 0) return;
    setFoundTurn(index);
    pendingJump.current = { turn: mapTurns[index], play: false };
    setTab('transcript');
  }, [moment, sessionId, mapTurns]);

  useEffect(() => setReading(false), [tab]);

  const searchHits = useMemo<MapHit[]>(() => {
    if (!staticDoc || !query) return [];
    return Object.values(staticDoc.rows)
      .filter((row) => hasMatch(row.text, query))
      .map((row) => ({ ms: row.tStartMs, channel: row.channel }));
  }, [staticDoc, query]);

  const askedAtMs = useCallback(
    (ask: AskInfo): number | null => {
      if (!detail) return null;
      const ms = (ask.created_at - detail.started_at) * 1000;
      return ms >= 0 && ms <= durationMs ? ms : null;
    },
    [detail, durationMs],
  );

  // A question is your own voice, so it marks your side of the line.
  const askMarks = useMemo<MapHit[]>(
    () =>
      asks.flatMap((a) => {
        const ms = askedAtMs(a);
        return ms === null ? [] : [{ ms, channel: 'mic' }];
      }),
    [asks, askedAtMs],
  );

  const interrupted = (detail?.meta as Record<string, unknown> | undefined)?.['interrupted'] === true;
  const loadFailed = !isLive && !detail && error !== null;

  const retry = () => {
    setError(null);
    api.session(sessionId).then(setDetail).catch((e) => setError((e as Error).message));
  };

  const title = isLive
    ? (sessionTitleLive ?? 'Recording…')
    : (detail?.title ?? (error ? 'This meeting could not be opened' : 'Opening this meeting…'));

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
      setError(`The meeting was not renamed: ${(e as Error).message}`);
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
      setError(`The summary was not written: ${(e as Error).message}. Check the provider in Settings, then try again.`);
    } finally {
      setSummarizing(false);
    }
  };

  const saveExport = async () => {
    try {
      const url = URL.createObjectURL(await api.exportBlob(sessionId));
      const link = document.createElement('a');
      link.href = url;
      link.download = `${sessionId}.md`;
      link.click();
      setTimeout(() => URL.revokeObjectURL(url));
    } catch (e) {
      setError(`The export failed: ${(e as Error).message}`);
    }
  };

  const showMoreAsks = () => {
    setError(null);
    askLoads.current.extend(
      () => api.asks(sessionId, ASK_PAGE, asks.length),
      (page) => {
        setAsks((shown) => appendPage(shown, page));
        setMoreAsks(page.length >= ASK_PAGE);
      },
      (e) => setError(`More questions could not be loaded: ${e.message}`),
    );
  };

  const deleteAsk = async (id: number) => {
    setError(null);
    try {
      if (!(await confirmDeleteAsk(id, (message) => window.confirm(message)))) return;
      // A page in flight was read from before the delete, and may hold this row.
      askLoads.current.cancel();
      setAsks((shown) => shown.filter((a) => a.id !== id));
      const rest = asks.filter((a) => a.id !== id);
      if (rest.length === 0 && !moreAsks) setTab('transcript');
    } catch (e) {
      setError(`The question was not deleted: ${(e as Error).message}`);
    }
  };

  return (
    <div
      className={`session-view ${reading ? 'reading' : ''}`}
      onScrollCapture={(e) => {
        const el = e.target as HTMLElement;
        // Too little text and folding the header would end the scroll that
        // caused it, and the two would flip back and forth.
        const room = el.scrollHeight - el.clientHeight;
        setReading((was) => (was ? el.scrollTop > 4 : el.scrollTop > 24 && room > 200));
      }}
    >
      <header className="session-header">
        {query && (
          <button className="link-btn back-link" onClick={onBackToResults}>
            Back to results for “{query}”
          </button>
        )}
        <div className="session-title-row">
          {editingTitle ? (
            <input
              className="title-input"
              type="text"
              aria-label="Session title"
              defaultValue={title}
              autoFocus
              onBlur={(e) => saveTitle(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === 'Escape') refocusTitle.current = true;
                if (e.key === 'Enter') {
                  // Focus lands on the title during this keydown; without this
                  // the same Enter would click it and reopen the editor.
                  e.preventDefault();
                  saveTitle(e.currentTarget.value);
                }
                if (e.key === 'Escape') setEditingTitle(false);
              }}
            />
          ) : (
            <h1 className="session-title">
              <button
                className="session-title-btn"
                title={`Rename: ${title}`}
                ref={(el) => {
                  if (el && refocusTitle.current) {
                    refocusTitle.current = false;
                    el.focus();
                  }
                }}
                onClick={() => setEditingTitle(true)}
              >
                {title}
              </button>
            </h1>
          )}
          {isLive && (
            <span className="rec">
              <span className="rec-dot" /> Recording
            </span>
          )}
        </div>
        <div className="session-meta-row">
          {isLive ? (
            <>
              <span className="meta-item">
                <LiveTicker startedAtMs={liveStartedAtMs} />
              </span>
              {sessionProvider && (
                <span className="meta-item where">
                  <WhereMark place={providerPlace(sessionProvider)} />
                  {providerWhere(sessionProvider)}
                </span>
              )}
              {latencyMs !== null && (
                <span className="meta-item">{(latencyMs / 1000).toFixed(1)} s behind</span>
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
                <span className="meta-item">{fmtLongDate(detail.started_at)}</span>
                <span className="meta-item">
                  {detail.ended_at ? fmtLength(detail.ended_at - detail.started_at) : 'In progress'}
                </span>
                <span className="meta-item where">
                  <WhereMark place={providerPlace(detail.stt_provider)} />
                  {providerWhere(detail.stt_provider)}
                </span>
                {detail.transcript.length > 0 && (
                  <a
                    className="btn quiet header-export"
                    title="Save as a Markdown file"
                    href={api.exportUrl(sessionId)}
                    download={`${sessionId}.md`}
                    onClick={(e) => {
                      if (!readToken()) return;
                      e.preventDefault();
                      void saveExport();
                    }}
                  >
                    Export
                  </a>
                )}
              </>
            )
          )}
        </div>
        {!loadFailed && <div className="tabs">
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
            Summary
          </button>
          {(asks.length > 0 || moreAsks) && (
            <button
              className={`tab ${tab === 'asks' ? 'active' : ''}`}
              onClick={() => setTab('asks')}
            >
              Asks
            </button>
          )}
        </div>}
      </header>

      {/* After the header in the DOM, so Tab still reaches the map after the
          tabs. The grid is what puts it beside the text. */}
      <div
        className={`session-tape ${isLive ? 'live' : ''} ${tab === 'transcript' ? '' : 'no-frame'}`}
      >
        <TapeLegend
          lost={
            isLive && deviceLost ? (deviceLost.startsWith('mic') ? 'mic' : 'loopback') : undefined
          }
        />
        {isLive ? (
          <LiveTape showPen={!reducedMotion} onPick={(t) => openAt(t, false)} />
        ) : (
          <SessionMap
            ref={mapRef}
            turns={mapTurns}
            durationMs={durationMs}
            hits={tab === 'asks' ? askMarks : searchHits}
            cutMs={interrupted ? mapTurns[mapTurns.length - 1]?.endMs : undefined}
            onPick={(t) => openAt(t, true)}
            onSeek={seekOnLine}
          />
        )}
      </div>

      {deviceLost && isLive && (
        <div className="banner">
          <span>
            <strong>Audio device lost.</strong>{' '}
            {deviceLost.replace(/^mic: /, '').replace(/^loopback: /, 'System audio: ')}. Reconnect
            it, then stop and start a new recording to capture that side again.
          </span>
        </div>
      )}
      {/* lastError is announced from App's status line, so only this view's
          own failures need a region here. */}
      <div className="status-slot" role="status">
        {error &&
          (loadFailed ? (
            <div className="banner">
              <span>
                Auricle could not read this meeting ({error}). Try again in a moment. If it keeps
                failing, restart Auricle.
              </span>
              <button className="btn" onClick={retry}>
                Try again
              </button>
            </div>
          ) : (
            <div className="banner">{error}</div>
          ))}
      </div>
      {!error && isLive && lastError && <div className="banner">{lastError}</div>}
      {interrupted && (
        <div className="banner passing">
          <span>
            Auricle closed unexpectedly during this meeting, so the recording ends at{' '}
            {fmtTs(mapTurns[mapTurns.length - 1]?.endMs ?? 0)}. Everything before that was saved.
          </span>
        </div>
      )}

      {tab === 'transcript' ? (
        <div className="doc-wrap">
          {isLive ? (
            <TurnDoc
              ref={docRef}
              turnOrder={liveTurnOrder}
              live
              follow
              emptyHint="Listening. Speak, or play some audio. Words appear here as they are heard, and settle once they are sure."
            />
          ) : staticDoc ? (
            <TurnDoc
              ref={docRef}
              turnOrder={staticDoc.turnOrder}
              turns={staticDoc.turns}
              rows={staticDoc.rows}
              live={false}
              onSeek={
                hasAudio
                  ? (ms) => {
                      setFollowing(true);
                      audioRef.current?.seek(ms);
                    }
                  : undefined
              }
              onRange={showReading}
              query={query}
              foundTurn={foundTurn}
              currentTurn={currentTurn}
              onUserScroll={() => setFollowing(false)}
              emptyHint={
                <>
                  <p>
                    <strong>No speech was heard in this meeting.</strong>
                  </p>
                  <p>
                    Neither side made a sound for the whole recording. If people were talking, the
                    wrong device may have been listening.
                  </p>
                  <button className="btn" onClick={onOpenSettings}>
                    Check your audio devices
                  </button>
                </>
              }
            />
          ) : null}
          {hasAudio && (
            <AudioBar
              ref={audioRef}
              sessionId={sessionId}
              onTime={followAudio}
              following={following || currentTurn === null}
              onFollow={() => setFollowing(true)}
            />
          )}
        </div>
      ) : tab === 'asks' ? (
        <AskList
          asks={asks}
          hasMore={moreAsks}
          onMore={showMoreAsks}
          onDelete={deleteAsk}
          askedAtMs={askedAtMs}
          onJump={(ms) => {
            const turn = mapTurns.find((t) => ms < t.endMs) ?? mapTurns[mapTurns.length - 1];
            if (turn) openAt(turn, false);
          }}
        />
      ) : (
        <div className="summary-pane">
          <div className="summary-scroll">
            {detail?.summaries.length === 0 && (
              <p className="note">
                No summaries yet. Choose a template and who writes it, then press Write a summary.
              </p>
            )}
            {detail?.summaries.map((s) => (
              <div className="summary" key={s.id}>
                <p className="summary-head">
                  {templateName(s.template)}, written by {s.model}
                </p>
                <div className="summary-body">
                  <Markdown
                    source={withoutRepeatedTitle(s.content, title, templateName(s.template))}
                  />
                </div>
              </div>
            ))}
            {detail && detail.summaries.length > 0 && (
              <p className="note summary-check">
                To check a point, pick that part of the line and read the transcript there.
              </p>
            )}
            <div className="summary-controls">
              <label className="control">
                <span>Template</span>
                <select
                  value={template}
                  onChange={(e) => useAuricle.setState({ summaryTemplate: e.target.value })}
                >
                  {templates.map((t) => (
                    <option key={t.name} value={t.name}>
                      {templateName(t.name)}
                      {t.overridden ? ' (customized)' : ''}
                    </option>
                  ))}
                </select>
              </label>
              <label className="control">
                <span>Written by</span>
                <select
                  value={llmProvider}
                  onChange={(e) => useAuricle.setState({ summaryProvider: e.target.value })}
                >
                  {llmProviders.map((p) => (
                    <option key={p.id} value={p.id} disabled={!p.ready}>
                      {providerName(p.id)} ({p.model}){p.ready ? '' : ', not ready'}
                    </option>
                  ))}
                </select>
              </label>
              <button
                className={detail?.summaries.length ? 'btn' : 'btn accent'}
                onClick={summarize}
                disabled={summarizing || !llmProvider || !detail || detail.transcript.length === 0}
              >
                {summarizing
                  ? 'Writing…'
                  : detail?.summaries.length
                    ? 'Write another summary'
                    : 'Write a summary'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
