// The overlay's SSE client. Transport: the engine rejects cross-origin
// browser fetches (its same-origin defense sees the webview's
// tauri.localhost origin), so the HTTP+SSE legwork happens in the
// overlay's Rust side against the public API; parsed events arrive here
// over a Tauri channel. This module turns those raw events into reducer
// events — pure apart from the invoke, and the mapping is unit-tested.

import { invoke, Channel } from '@tauri-apps/api/core';
import { deriveChips, type AskStartedInfo } from './chips';
import type { OverlayEvent } from './state';

/** One parsed `data:` JSON payload from the ask's SSE stream. */
export type AskSseEvent = Record<string, unknown>;

/**
 * Map one SSE event to a reducer event (null = ignore). Exported for
 * tests; transport-independent.
 */
export function applyAskEvent(ev: AskSseEvent, windowMin: number): OverlayEvent | null {
  switch (ev.type) {
    case 'ask_started': {
      const info: AskStartedInfo = {
        screen: (ev.screen as AskStartedInfo['screen']) ?? null,
        transcript_segments:
          typeof ev.transcript_segments === 'number' ? ev.transcript_segments : null,
      };
      return {
        type: 'ASK_STARTED',
        askId: String(ev.ask_id ?? ''),
        chips: deriveChips(info, windowMin),
      };
    }
    case 'answer_delta':
      return { type: 'DELTA', text: String(ev.text ?? '') };
    case 'answer_done': {
      const usage = ev.usage as { total_tokens: number } | null | undefined;
      return { type: 'DONE', usage: usage ?? null };
    }
    case 'ask_error':
      return { type: 'ERROR', message: String(ev.message ?? 'ask failed') };
    default:
      return null;
  }
}

export interface AskRequest {
  question: string;
  include_screen: boolean;
  include_transcript: boolean;
  follow_up: boolean;
}

/**
 * Run one ask through the Rust bridge. `onEvent` receives reducer events
 * in stream order; a transport failure (engine down, HTTP error status)
 * surfaces as a final ERROR event.
 */
export async function streamAsk(
  req: AskRequest,
  windowMin: number,
  onEvent: (ev: OverlayEvent) => void,
): Promise<void> {
  const channel = new Channel<AskSseEvent>();
  channel.onmessage = (ev) => {
    const mapped = applyAskEvent(ev, windowMin);
    if (mapped) onEvent(mapped);
  };
  try {
    await invoke('ask_stream', { req, onEvent: channel });
  } catch (e) {
    onEvent({ type: 'ERROR', message: String(e) });
  }
}
