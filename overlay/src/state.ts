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
  hasHistory: false,
  quick: false,
  provider: null,
  startedAt: null,
  elapsedMs: null,
  usage: null,
};

/** The templated quick-assist question (Ctrl+Shift+A: no typing). */
export const QUICK_ASSIST_QUESTION =
  "What's happening right now? Briefly summarize what's being discussed and what's on my screen.";

export type OverlayEvent =
  | { type: 'SUMMON' }
  | { type: 'QUICK_ASSIST'; at: number }
  | { type: 'SUBMIT'; question: string; at: number }
  | { type: 'ASK_STARTED'; askId: string; chips: Chip[]; provider: string | null }
  | { type: 'DELTA'; text: string }
  | { type: 'DONE'; usage: { total_tokens: number } | null; at: number }
  | { type: 'ERROR'; message: string }
  | { type: 'DISMISS' };

const busy = (phase: Phase) => phase === 'asking' || phase === 'streaming';

/** Everything one new ask resets. */
function freshAsk(state: OverlayState, question: string, quick: boolean, at: number): OverlayState {
  return {
    ...state,
    phase: 'asking',
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
      return freshAsk(state, QUICK_ASSIST_QUESTION, true, ev.at);

    case 'SUBMIT': {
      if (busy(state.phase) || state.phase === 'hidden') return state;
      const q = ev.question.trim();
      if (q === '') return state;
      return freshAsk(state, q, false, ev.at);
    }

    case 'ASK_STARTED':
      if (state.phase !== 'asking') return state;
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
      if (!busy(state.phase)) return state;
      return { ...state, phase: 'streaming', answer: state.answer + ev.text };

    case 'DONE':
      if (!busy(state.phase)) return state;
      return {
        ...state,
        phase: 'answered',
        usage: ev.usage,
        hasHistory: true,
        elapsedMs: state.startedAt === null ? null : ev.at - state.startedAt,
      };

    case 'ERROR':
      if (state.phase === 'hidden') return state;
      return { ...state, phase: 'error', error: ev.message };

    case 'DISMISS':
      // Dismiss hides the window; a completed answer is kept (re-summon
      // shows it, and engine-side ask history still enables follow-ups).
      // Dismissing mid-stream abandons the overlay's view of the ask —
      // later deltas find phase 'hidden' and are dropped; the engine
      // finishes the ask regardless and mirrors it on /ws/live.
      return { ...state, phase: 'hidden' };
  }
}
