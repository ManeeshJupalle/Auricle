// The overlay's summon/ask lifecycle as a pure reducer — every hotkey,
// SSE event, and user action is an event here, so the whole state machine
// is unit-testable without Tauri or a DOM. Time enters only through
// event payloads (`at`), never from clocks inside the reducer.

export type Chip = { icon: 'screen' | 'transcript'; label: string };

export type Phase =
  | 'hidden' // window not shown
  | 'ready' // shown, waiting for a question
  | 'asking' // request sent, no ask_started yet
  | 'streaming' // ask accepted; deltas arriving (or first token pending)
  | 'answered' // answer complete
  | 'error'; // ask failed

export interface OverlayState {
  phase: Phase;
  question: string;
  answer: string;
  chips: Chip[];
  error: string | null;
  askId: string | null;
  /**
   * The run this view follows. Stream events are tagged with the run they
   * came from, so anything tagged otherwise belongs to a superseded ask;
   * null after a dismiss, when no run is followed. Tagged client-side, per
   * channel, because ask_id only arrives with ask_started and the first
   * delta may beat it.
   */
  gen: number | null;
  /** Set when the NEXT submit should carry follow_up: true. */
  hasHistory: boolean;
  /** True when the current ask came from the quick-assist hotkey. */
  quick: boolean;
  /** Provider id echoed by ask_started (e.g. "groq"). */
  provider: string | null;
  /** Timestamp (`at`) the current ask was submitted. */
  startedAt: number | null;
  /** Submit → answer_done duration, set on DONE. */
  elapsedMs: number | null;
  usage: { total_tokens: number } | null;
}

export const initialState: OverlayState = {
  phase: 'hidden',
  question: '',
  answer: '',
  chips: [],
  error: null,
  askId: null,
  gen: null,
  hasHistory: false,
  quick: false,
  provider: null,
  startedAt: null,
  elapsedMs: null,
  usage: null,
};

/** The templated quick-assist question (Ctrl+Shift+A: no typing). */
/**
 * How long a toast stays up. Some carry steps to follow, and a few are the
 * only warning there will be, so a long one has to last long enough to read.
 */
export const toastMs = (message: string): number => Math.max(6_000, message.length * 70);

export const QUICK_ASSIST_QUESTION =
  "What's happening right now? Briefly summarize what's being discussed and what's on my screen.";

/** What one ask's stream can report, before it is tagged with its run. */
export type AskStreamEvent =
  | { type: 'ASK_STARTED'; askId: string; chips: Chip[]; provider: string | null }
  | { type: 'DELTA'; text: string }
  | { type: 'DONE'; usage: { total_tokens: number } | null; at: number }
  | { type: 'ERROR'; message: string };

export type OverlayEvent =
  | { type: 'SUMMON' }
  | { type: 'QUICK_ASSIST'; at: number; gen: number }
  | { type: 'SUBMIT'; question: string; at: number; gen: number }
  | (AskStreamEvent & { gen: number })
  | { type: 'NEW_CHAT' }
  | { type: 'DISMISS' };

const busy = (phase: Phase) => phase === 'asking' || phase === 'streaming';

/** Everything one new ask resets. */
function freshAsk(
  state: OverlayState,
  question: string,
  quick: boolean,
  at: number,
  gen: number,
): OverlayState {
  return {
    ...state,
    phase: 'asking',
    gen,
    question,
    answer: '',
    chips: [],
    error: null,
    askId: null,
    quick,
    provider: null,
    startedAt: at,
    elapsedMs: null,
    usage: null,
  };
}

export function reduce(state: OverlayState, ev: OverlayEvent): OverlayState {
  switch (ev.type) {
    case 'SUMMON':
      // Re-summon while busy or answered keeps everything on screen; it
      // only ever makes the overlay visible and focusable.
      if (state.phase === 'hidden') return { ...state, phase: 'ready' };
      return state;

    case 'QUICK_ASSIST':
      // Immediate ask — works from any non-busy state, including hidden.
      if (busy(state.phase)) return state;
      return freshAsk(state, QUICK_ASSIST_QUESTION, true, ev.at, ev.gen);

    case 'SUBMIT': {
      if (busy(state.phase) || state.phase === 'hidden') return state;
      const q = ev.question.trim();
      if (q === '') return state;
      return freshAsk(state, q, false, ev.at, ev.gen);
    }

    case 'ASK_STARTED':
      // Busy, not just asking: on the fallback path below a delta may have
      // arrived first, and the chips must still report what was captured.
      if (ev.gen !== state.gen || !busy(state.phase)) return state;
      return {
        ...state,
        phase: 'streaming',
        askId: ev.askId,
        chips: ev.chips,
        provider: ev.provider,
      };

    case 'DELTA':
      // First delta may arrive before ask_started on the fallback
      // (non-streaming) provider path — accept from asking too.
      if (ev.gen !== state.gen || !busy(state.phase)) return state;
      return { ...state, phase: 'streaming', answer: state.answer + ev.text };

    case 'DONE':
      if (ev.gen !== state.gen || !busy(state.phase)) return state;
      return {
        ...state,
        phase: 'answered',
        usage: ev.usage,
        hasHistory: true,
        elapsedMs: state.startedAt === null ? null : ev.at - state.startedAt,
      };

    case 'ERROR':
      if (ev.gen !== state.gen) return state;
      return { ...state, phase: 'error', error: ev.message };

    case 'NEW_CHAT':
      // Clears the screen and the next ask's follow_up flag. Not while an
      // ask runs: its stream would have nothing left to land on.
      if (busy(state.phase) || state.phase === 'hidden') return state;
      return { ...initialState, phase: 'ready' };

    case 'DISMISS':
      // Dismiss hides the window; a completed answer is kept (re-summon
      // shows it, and engine-side ask history still enables follow-ups).
      // Dismissing mid-stream abandons the ask: the Rust side drops the
      // stream, and clearing gen makes anything already in flight stale.
      // The engine finishes the ask regardless and mirrors it on /ws/live.
      return { ...state, phase: 'hidden', gen: null };
  }
}
