import { api } from './api';
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
 * Live event socket with auto-reconnect. On every (re)open it resyncs from
 * REST: engine state from /health, and — if a session is active — the
 * persisted finals from /sessions/:id, so a daemon restart or dropped
 * connection recovers without a manual refresh (partials missed while
 * disconnected are gone by design; finals are re-fetched).
 */
export function startLiveSocket(): void {
  const policy = new ReconnectPolicy();
  let everConnected = false;

  const connect = () => {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const socket = new WebSocket(`${proto}://${location.host}/ws/live`);

    socket.onopen = async () => {
      policy.reset();
      everConnected = true;
      useAuricle.getState().setConn('open');
      try {
        const health = await api.health();
        useAuricle.getState().setEngine(health.state, health.active_session);
        if (health.active_session) {
          const detail = await api.session(health.active_session);
          useAuricle.getState().loadFinals(detail.transcript);
        }
      } catch {
        // health will be re-fetched on the next reconnect
      }
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

  connect();
}
