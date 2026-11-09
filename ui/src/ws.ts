import { api } from './api';
import { readToken, withToken } from './auth';
import { useAuricle } from './store';
import type { WsEvent } from './types';

/** Exponential backoff for WS reconnects: 1s, 2s, 4s, 8s, capped at 10s. */
export class ReconnectPolicy {
  private attempt = 0;
  constructor(
    private readonly baseMs = 1000,
    private readonly maxMs = 10_000,
  ) {}

  nextDelay(): number {
    const delay = Math.min(this.baseMs * 2 ** this.attempt, this.maxMs);
    this.attempt += 1;
    return delay;
  }

  reset(): void {
    this.attempt = 0;
  }
}

/**
 * Rebuild live state from REST: engine state from /health and, if a session
 * is active, its title, provider, start time, and persisted finals from
 * /sessions/:id. Socket events wait in the store meanwhile, so a snapshot
 * taken before them can't overwrite them. `isCurrent` turns false once a newer
 * connection has taken over, and a superseded snapshot is discarded.
 */
export async function resync(isCurrent: () => boolean = () => true): Promise<void> {
  const { beginResync, endResync } = useAuricle.getState();
  beginResync();
  try {
    const health = await api.health();
    const detail = health.active_session
      ? await api.session(health.active_session).catch(() => null)
      : null;
    if (isCurrent()) endResync({ health, detail });
  } catch {
    // health will be re-fetched on the next reconnect
    if (isCurrent()) endResync(null);
  }
}

export function liveUrl(at: Pick<Location, 'protocol' | 'host'>): string {
  const proto = at.protocol === 'https:' ? 'wss' : 'ws';
  return withToken(`${proto}://${at.host}/ws/live`);
}

/**
 * Live event socket with auto-reconnect. On every (re)open it resyncs from
 * REST, so a page reload, daemon restart, or dropped connection recovers
 * without a manual refresh (partials missed while disconnected are gone by
 * design; finals are re-fetched).
 */
export function startLiveSocket(): void {
  const policy = new ReconnectPolicy();
  let everConnected = false;
  let current: WebSocket | null = null;

  const connect = async () => {
    // The browser prints a failed socket's URL to the console, and here that
    // URL holds the token. So don't try one until the daemon has answered,
    // which also clears a refused token before it can reach the URL.
    if (readToken() && !(await api.health().then(() => true, () => false))) {
      setTimeout(connect, policy.nextDelay());
      return;
    }

    const socket = new WebSocket(liveUrl(location));
    current = socket;

    socket.onopen = () => {
      policy.reset();
      everConnected = true;
      useAuricle.getState().setConn('open');
      void resync(() => socket === current);
    };

    socket.onmessage = (msg: MessageEvent<string>) => {
      try {
        const ev = JSON.parse(msg.data) as WsEvent;
        useAuricle.getState().applyEvent(ev);
      } catch {
        // ignore unparseable frames
      }
    };

    socket.onclose = () => {
      useAuricle.getState().setConn(everConnected ? 'reconnecting' : 'connecting');
      setTimeout(connect, policy.nextDelay());
    };
  };

  void connect();
}
