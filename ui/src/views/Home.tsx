import { useEffect, useMemo, useState, type CSSProperties } from 'react';
import { api } from '../api';
import { buildMapTurns, SessionMap } from '../components/SessionMap';
import { TapeLegend, TwoVoices } from '../components/TwoVoices';
import { buildTurns, useAuricle, type Row } from '../store';
import type { EgressLedger, ProvidersResponse, SessionDetail, SessionSummary } from '../types';
import { agoWord, downloadLine, providerName } from '../words';

function ReadyList({ items }: { items: { id: string; ready: boolean; detail: string }[] }) {
  return (
    <ul className="ready-list">
      {items.map((p) => (
        <li key={p.id} className={`ready-item ${p.ready ? '' : 'missing'}`} title={p.detail}>
          <span className="ready-mark" aria-hidden="true" />
          {providerName(p.id)}
          {!p.ready && ', not set up'}
        </li>
      ))}
    </ul>
  );
}

/**
 * The amount is set in the sans: the serif's heavy zero closes up and reads as
 * an o, and tabular figures keep the line still as the number climbs.
 */
function DownloadLine({ line }: { line: string }) {
  const [what, amount] = line.split(', ');
  return amount ? (
    <>
      {what}, <span className="figures">{amount}</span>
    </>
  ) : (
    <>{line}</>
  );
}

/** A stored meeting as the blocks of its map. */
function tapeOf(detail: SessionDetail) {
  const rows: Record<string, Row> = {};
  const order = detail.transcript.map((seg, i) => {
    const id = `s-${i}`;
    rows[id] = {
      id,
      channel: seg.channel === 0 ? 'mic' : 'loopback',
      speaker: seg.speaker,
      tStartMs: seg.t_start_ms,
      tEndMs: seg.t_end_ms,
      text: seg.text,
      final: true,
    };
    return id;
  });
  const { turnOrder, turns } = buildTurns(order, rows);
  const mapped = buildMapTurns(turnOrder, turns, rows);
  const wallMs = ((detail.ended_at ?? detail.started_at) - detail.started_at) * 1000;
  return { turns: mapped, durationMs: Math.max(wallMs, mapped[mapped.length - 1]?.endMs ?? 0) };
}

/**
 * The idle home. Everything here is content the sidebar does NOT already show:
 * the You/Them model and what the engine can do, for someone new; the way back
 * to the last meeting for someone who isn't; and for both, the privacy proof
 * from the egress ledger and the copilot hotkeys. The line carries the last
 * meeting, so anyone can step back into a moment of it. For someone new it
 * shows how to read the exchange beside it, and during a first download it
 * fills as the model arrives.
 */
export function Home({
  onShowEgress,
  onOpen,
}: {
  onShowEgress: () => void;
  onOpen: (id: string, ms?: number) => void;
}) {
  const notice = useAuricle((s) => s.notice);
  const startCancelled = useAuricle((s) => s.startCancelled);
  const download = useAuricle((s) => s.download);
  const [prov, setProv] = useState<ProvidersResponse | null>(null);
  const [egress, setEgress] = useState<EgressLedger | null>(null);
  const [egressFailed, setEgressFailed] = useState(false);
  // Null until the answer is in, so the page doesn't flip heroes on load. A
  // failed list says nothing about whether this person is new, so it stays out
  // of 'new'.
  const [history, setHistory] = useState<'new' | 'returning' | null>(null);
  const [latest, setLatest] = useState<SessionSummary | null>(null);
  const [lastMeeting, setLastMeeting] = useState<SessionDetail | null>(null);

  useEffect(() => {
    api.providers().then(setProv).catch(() => {});
    api
      .egress()
      .then(setEgress)
      .catch(() => setEgressFailed(true));
    api
      .sessionPage(1, 0)
      .then((page) => {
        const last = page.sessions[0] ?? null;
        setLatest(last);
        setHistory(last ? 'returning' : 'new');
        if (last) api.session(last.id).then(setLastMeeting).catch(() => {});
      })
      .catch(() => setHistory('returning'));
  }, []);

  const tape = useMemo(() => (lastMeeting ? tapeOf(lastMeeting) : null), [lastMeeting]);

  // Whole-ledger totals, not the page of entries: the claim this line makes
  // must not depend on how many rows the API returned.
  const totals = egress?.totals ?? [];
  const count = (d: string) =>
    totals.filter((t) => t.destination === d).reduce((n, t) => n + t.count, 0);
  const cloud = count('cloud');
  const agent = count('agent');
  const local = count('local');
  const hosts = totals.filter((t) => t.destination === 'cloud').map((t) => t.who);

  const hasTape = tape !== null && tape.turns.length > 0;
  const verifying = download !== null && download.doneMb >= download.sizeMb;
  const filled = download ? Math.min(1, download.doneMb / download.sizeMb) : 0;

  return (
    <div className="home">
      <div
        className={`session-tape ${download ? 'filling' : ''}`}
        style={{ '--filled': filled } as CSSProperties}
      >
        <TapeLegend />
        {hasTape && !notice && (
          <SessionMap
            turns={tape.turns}
            durationMs={tape.durationMs}
            onPick={(t) => latest && onOpen(latest.id, t.startMs)}
          />
        )}
      </div>
      <div className="home-scroll">
        <div className="home-inner">
          {notice ? (
            <>
              <h1 className="home-title">
                {startCancelled ? (
                  'Recording cancelled'
                ) : download ? (
                  <DownloadLine line={downloadLine(download)} />
                ) : (
                  notice
                )}
              </h1>
              <p className="home-lede">
                {startCancelled ? (
                  <>
                    Nothing will be recorded. A download can&rsquo;t be stopped part way, so this one
                    is finishing on its own
                    {download && !verifying
                      ? ` (${Math.floor(download.doneMb)} of ${download.sizeMb} MB)`
                      : ''}
                    . You can record again as soon as it&rsquo;s done, and the model will be ready.
                  </>
                ) : verifying ? (
                  'Nothing is being recorded yet. The download is complete, and Auricle is making sure the file is intact. Recording starts by itself right after.'
                ) : (
                  'Nothing is being recorded yet. This only happens the first time, and recording starts by itself when it finishes. Press Cancel if you would rather not wait.'
                )}
              </p>
            </>
          ) : (
            <>
              {history === 'new' && (
                <>
                  <h1 className="sr-only">Auricle</h1>
                  <TwoVoices />
                  <p className="home-lede">
                    No bot joins your calls. Press Start recording when the meeting begins.
                  </p>
                </>
              )}
              {history === 'returning' && (
                <>
                  <h1 className="home-title">Ready when your next meeting starts</h1>
                  <p className="home-lede">
                    Press Start recording when it begins. No bot joins your calls.
                  </p>
                  {latest && (
                    <p className="home-last">
                      Last meeting, {agoWord(latest.started_at)}:{' '}
                      <button className="link-btn" onClick={() => onOpen(latest.id)}>
                        {latest.title}
                      </button>
                    </p>
                  )}
                </>
              )}
            </>
          )}

          {!notice && <dl className="facts">
            {!egressFailed && (
              <div className="fact">
                <dt>Privacy</dt>
                <dd>
                  {egress === null ? (
                    '…'
                  ) : cloud === 0 && agent === 0 ? (
                    <>
                      Nothing has left this machine.
                      {local > 0 && (
                        <>
                          {' '}
                          {local} local {local === 1 ? 'action' : 'actions'} on record.
                        </>
                      )}
                    </>
                  ) : cloud === 0 ? (
                    <>
                      Auricle sent nothing off this machine. Agents read your transcript {agent}{' '}
                      {agent === 1 ? 'time' : 'times'}.
                    </>
                  ) : (
                    <>
                      Data left this machine {cloud} {cloud === 1 ? 'time' : 'times'}, to{' '}
                      {hosts.join(' and ')}.
                      {agent > 0 && (
                        <>
                          {' '}
                          Agents read your transcript {agent} {agent === 1 ? 'time' : 'times'}.
                        </>
                      )}
                    </>
                  )}{' '}
                  <button className="link-btn" onClick={onShowEgress}>
                    {cloud === 0 && agent === 0 ? 'See the record' : 'See what left'}
                  </button>
                </dd>
              </div>
            )}
            {/* Setup information. Once there's history the sidebar shows what
                the next recording will use, and this would only wear. */}
            {prov && history === 'new' && (
              <div className="fact">
                <dt>Transcription</dt>
                <dd>
                  <ReadyList items={prov.providers} />
                </dd>
              </div>
            )}
            {prov && history === 'new' && (
              <div className="fact">
                <dt>Summaries</dt>
                <dd>
                  <ReadyList items={prov.llm} />
                </dd>
              </div>
            )}
            <div className="fact">
              <dt>Copilot</dt>
              <dd className="keys-list">
                <span>
                  <kbd>Ctrl+Shift+Space</kbd> ask about the meeting or your screen
                </span>
                <span>
                  <kbd>Ctrl+Shift+A</kbd> ask what’s happening right now
                </span>
              </dd>
            </div>
          </dl>}
        </div>
      </div>
    </div>
  );
}
