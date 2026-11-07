import { Markdown } from '@shared/Markdown';
import { api } from '../api';
import type { AskInfo } from '../types';
import { providerName } from '../words';
import { fmtTs } from './TurnDoc';

export const ASK_PAGE = 100;

/** There is no other copy and no undo, so nothing is deleted unconfirmed. */
export async function confirmDeleteAsk(
  id: number,
  confirm: (message: string) => boolean,
): Promise<boolean> {
  const message =
    'Delete this question, its answer, and any screen text kept with it? This cannot be undone.';
  if (!confirm(message)) return false;
  await api.deleteAsk(id);
  return true;
}

/** Stored as JSON; shown as what was on screen. Anything else shows as stored. */
export function screenText(stored: string | null): string | null {
  if (stored === null) return null;
  try {
    const screen = JSON.parse(stored) as { window_title?: string; text?: string };
    const parts = [screen.window_title, screen.text].filter(Boolean);
    return parts.length > 0 ? parts.join('\n\n') : stored;
  } catch {
    return stored;
  }
}

/** Copilot questions kept for this session, which only `copilot.retain_context` writes. */
export function AskList({
  asks,
  hasMore,
  onMore,
  onDelete,
  askedAtMs,
  onJump,
}: {
  asks: AskInfo[];
  hasMore: boolean;
  onMore: () => void;
  onDelete: (id: number) => void;
  /** How far into the meeting an ask came, or null if it came outside it. */
  askedAtMs: (ask: AskInfo) => number | null;
  /** Open the transcript at that moment. */
  onJump: (ms: number) => void;
}) {
  return (
    <div className="summary-pane">
      <div className="summary-scroll">
        <p className="note">
          You chose to keep copilot questions, so these are saved. Each holds your question, the
          answer, and any screen text sent with it. The dots on the line show when you asked.
        </p>
        {asks.map((a) => {
          const screen = screenText(a.screen_context);
          const askedAt = askedAtMs(a);
          return (
            <div className="summary ask" key={a.id}>
              <div className="summary-head">
                {askedAt !== null && (
                  <button
                    className="turn-time"
                    title="Read this moment in the transcript"
                    onClick={() => onJump(askedAt)}
                  >
                    {fmtTs(askedAt)}
                  </button>
                )}
                <span className="summary-title">{a.question}</span>
                <span className="ask-meta">
                  <span className="summary-model">{providerName(a.provider)}</span>
                  <button
                    className="btn quiet"
                    aria-label={`Delete question: ${a.question}`}
                    onClick={() => onDelete(a.id)}
                  >
                    Delete
                  </button>
                </span>
              </div>
              <div className="summary-body">
                <Markdown source={a.answer} />
                {screen !== null && (
                  <details className="ask-screen">
                    <summary>Screen text sent with this question</summary>
                    <pre>{screen}</pre>
                  </details>
                )}
                <p className="note">
                  {new Date(a.created_at * 1000).toLocaleString(undefined, {
                    month: 'short',
                    day: 'numeric',
                    hour: 'numeric',
                    minute: '2-digit',
                  })}
                </p>
              </div>
            </div>
          );
        })}
        {hasMore && (
          <button className="btn quiet show-more" onClick={onMore}>
            Show more
          </button>
        )}
      </div>
    </div>
  );
}
