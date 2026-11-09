import { beforeEach, describe, expect, it, vi } from 'vitest';
import { applyAskEvent, streamAsk } from './ask';
import type { OverlayEvent } from './state';

// Only streamAsk touches Tauri; a run's channel and invoke are stand-ins.
const bridge = vi.hoisted(() => ({
  channels: [] as { onmessage: (ev: Record<string, unknown>) => void }[],
  invoke: vi.fn(),
}));

vi.mock('@tauri-apps/api/core', () => ({
  invoke: bridge.invoke,
  Channel: class {
    onmessage: (ev: Record<string, unknown>) => void = () => {};
    constructor() {
      bridge.channels.push(this);
    }
  },
}));

// applyAskEvent is the SSE-client core: raw engine events → reducer
// events, transport-independent (the HTTP legwork lives in Rust).

describe('ask SSE event mapping', () => {
  it('maps the real Phase 8 event sequence end to end', () => {
    // Shapes lifted from the captured curl exchange in docs/API.md.
    const started = applyAskEvent(
      {
        type: 'ask_started',
        ask_id: 'a19f69571183',
        provider: 'groq',
        model: 'llama-3.3-70b-versatile',
        screen: { app_name: 'msedge', ocr_ms: 74, window_title: 'Speech recognition - Wikipedia' },
        transcript_segments: 3,
      },
      10,
      1_000,
    );
    expect(started).toEqual({
      type: 'ASK_STARTED',
      askId: 'a19f69571183',
      provider: 'groq',
      chips: [
        { icon: 'screen', label: 'msedge — Speech recognition - Wikipedia' },
        { icon: 'transcript', label: 'transcript: last 10 min · 3 segments' },
      ],
    });

    expect(applyAskEvent({ type: 'answer_delta', ask_id: 'a', text: ' The' }, 10, 1_100)).toEqual({
      type: 'DELTA',
      text: ' The',
    });

    expect(
      applyAskEvent(
        {
          type: 'answer_done',
          ask_id: 'a',
          usage: { completion_tokens: 39, prompt_tokens: 366, total_tokens: 405 },
        },
        10,
        2_400,
      ),
    ).toEqual({
      type: 'DONE',
      usage: { completion_tokens: 39, prompt_tokens: 366, total_tokens: 405 },
      at: 2_400,
    });
  });

  it('maps ask_error and tolerates a missing usage or provider', () => {
    expect(
      applyAskEvent(
        { type: 'ask_error', ask_id: 'a', message: 'llm ollama returned HTTP 404' },
        10,
        0,
      ),
    ).toEqual({ type: 'ERROR', message: 'llm ollama returned HTTP 404' });
    expect(applyAskEvent({ type: 'answer_done', ask_id: 'a', usage: null }, 10, 7)).toEqual({
      type: 'DONE',
      usage: null,
      at: 7,
    });
    const started = applyAskEvent(
      { type: 'ask_started', ask_id: 'a', screen: null, transcript_segments: null },
      10,
      0,
    );
    expect(started).toMatchObject({ provider: null });
  });

  it('ignores unknown event types (forward compatibility)', () => {
    expect(applyAskEvent({ type: 'shiny_new_event' }, 10, 0)).toBeNull();
  });

  it('screen: null produces no screen chip', () => {
    const started = applyAskEvent(
      { type: 'ask_started', ask_id: 'a', screen: null, transcript_segments: 0 },
      10,
      0,
    );
    expect(started).toEqual({
      type: 'ASK_STARTED',
      askId: 'a',
      provider: null,
      chips: [{ icon: 'transcript', label: 'transcript: empty window' }],
    });
  });
});

describe('streamAsk', () => {
  const req = { question: 'q', include_screen: false, include_transcript: true, follow_up: false };

  beforeEach(() => {
    bridge.channels.length = 0;
    bridge.invoke.mockReset();
  });

  it('tags every event with the run that produced it', async () => {
    bridge.invoke.mockResolvedValue(undefined);
    const seen: OverlayEvent[] = [];
    await streamAsk(req, 1, 10, (ev) => seen.push(ev));
    await streamAsk(req, 2, 10, (ev) => seen.push(ev));
    // Interleaved, as a superseded stream would be.
    bridge.channels[1].onmessage({ type: 'answer_delta', text: 'new' });
    bridge.channels[0].onmessage({ type: 'answer_delta', text: 'old' });
    expect(seen).toEqual([
      { type: 'DELTA', text: 'new', gen: 2 },
      { type: 'DELTA', text: 'old', gen: 1 },
    ]);
  });

  it('forwards the request and tags a transport failure too', async () => {
    bridge.invoke.mockRejectedValue('engine unreachable');
    const seen: OverlayEvent[] = [];
    await streamAsk(req, 7, 10, (ev) => seen.push(ev));
    expect(bridge.invoke.mock.calls[0][1].req).toEqual(req);
    expect(seen).toEqual([{ type: 'ERROR', message: 'engine unreachable', gen: 7 }]);
  });
});
