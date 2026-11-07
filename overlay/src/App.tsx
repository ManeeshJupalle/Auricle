import { useCallback, useEffect, useReducer, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { initialState, reduce, toastMs, type OverlayEvent } from './state';
import { streamAsk } from './ask';
import { nextPollDelay } from './connection';
import { Markdown } from '@shared/Markdown';

interface InitInfo {
  hotkey_warnings: string[];
  /** Messages raised before this webview was listening. */
  notices: string[];
  window_min: number;
  theme: string;
  summon_hotkey: string;
  quick_hotkey: string;
}

type AskStart = { type: 'SUBMIT'; question: string; at: number } | { type: 'QUICK_ASSIST'; at: number };

// Clipboard fallback for webviews where the async clipboard API is
// unavailable (tauri.localhost is not always a secure context).
function copyFallback(text: string, done: () => void) {
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  document.execCommand('copy');
  ta.remove();
  done();
}

export function App() {
  const [state, dispatch] = useReducer(reduce, initialState);
  const stateRef = useRef(state);
  stateRef.current = state;

  const [init, setInit] = useState<InitInfo | null>(null);
  const [connected, setConnected] = useState(true);
  const [recording, setRecording] = useState(false);
  const [copied, setCopied] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const [includeScreen, setIncludeScreen] = useState(true);
  const includeScreenRef = useRef(includeScreen);
  includeScreenRef.current = includeScreen;
  const genRef = useRef(0);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const answerRef = useRef<HTMLDivElement>(null);
  const cardRef = useRef<HTMLDivElement>(null);

  const windowMin = init?.window_min ?? 10;
  const windowMinRef = useRef(windowMin);
  windowMinRef.current = windowMin;

  // The one way an ask starts. The reducer decides whether it may; a stream
  // only starts if it did, because the Rust side cancels whatever ran
  // before, and a stream the reducer refused would take the live one down
  // with it. stateRef advances now, not at the next render, so a second
  // press in the same frame is judged against this ask.
  const startAsk = useCallback((start: AskStart, followUp: boolean): boolean => {
    const gen = ++genRef.current;
    const ev: OverlayEvent = { ...start, gen };
    const next = reduce(stateRef.current, ev);
    if (next === stateRef.current) return false;
    stateRef.current = next;
    dispatch(ev);
    void streamAsk(
      {
        question: next.question,
        include_screen: includeScreenRef.current,
        include_transcript: true,
        follow_up: followUp,
      },
      gen,
      windowMinRef.current,
      (streamEv: OverlayEvent) => dispatch(streamEv),
    );
    return true;
  }, []);

  // Init + hotkey events from the Rust side.
  useEffect(() => {
    let toastTimer: number | undefined;
    const showToast = (message: string) => {
      setToast(message);
      window.clearTimeout(toastTimer);
      toastTimer = window.setTimeout(() => setToast(null), toastMs(message));
    };
    const unSummon = listen('summon', () => {
      dispatch({ type: 'SUMMON' });
    });
    const unQuick = listen('quick-assist', () => {
      // A fresh situational ask, not a follow-up.
      startAsk({ type: 'QUICK_ASSIST', at: performance.now() }, false);
    });
    const unToast = listen<string>('tray-error', (e) => {
      dispatch({ type: 'SUMMON' });
      showToast(e.payload);
    });
    // After the listeners, so nothing falls between get_init handing over
    // the early messages and the live events taking over. Rust has already
    // shown the window for these, and only this tells the reducer.
    void Promise.all([unSummon, unQuick, unToast])
      .then(() => invoke<InitInfo>('get_init'))
      .then((info) => {
        setInit(info);
        document.documentElement.dataset.theme = info.theme;
        if (info.notices.length > 0) showToast(info.notices.join(' · '));
        if (info.notices.length > 0 || info.hotkey_warnings.length > 0) {
          dispatch({ type: 'SUMMON' });
        }
      });
    return () => {
      window.clearTimeout(toastTimer);
      void unSummon.then((f) => f());
      void unQuick.then((f) => f());
      void unToast.then((f) => f());
    };
  }, [startAsk]);

  // The window is sized to the rendered card: a transparent window region
  // would still swallow clicks, so there must not be one. +20 covers the
  // 10 px shadow gutter on each side.
  useEffect(() => {
    const card = cardRef.current;
    if (!card) return;
    const observer = new ResizeObserver(() => {
      void invoke('resize_overlay', { height: card.offsetHeight + 20 });
    });
    observer.observe(card);
    return () => observer.disconnect();
  }, []);

  // Esc dismisses — window-level, only while the overlay has focus. (A
  // global Esc hotkey would swallow Esc from every app on the system.)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        dispatch({ type: 'DISMISS' });
        void invoke('hide_overlay');
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // Engine connection monitor (through the Rust bridge; backoff while
  // down). Health also drives the header dot: red = a session is
  // actually recording, nothing else.
  useEffect(() => {
    let failures = 0;
    let timer: number;
    let stopped = false;
    const poll = async () => {
      try {
        const health = await invoke<{ state?: string }>('get_health');
        failures = 0;
        if (!stopped) {
          setConnected(true);
          setRecording(health.state === 'recording');
        }
      } catch {
        failures += 1;
        if (!stopped) {
          setConnected(false);
          setRecording(false);
        }
      }
      if (!stopped) timer = window.setTimeout(poll, nextPollDelay(failures));
    };
    void poll();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
    };
  }, []);

  // Focus the input whenever the overlay becomes ready for typing.
  useEffect(() => {
    if (state.phase === 'ready' || state.phase === 'answered' || state.phase === 'error') {
      inputRef.current?.focus();
    }
  }, [state.phase]);

  // Follow the streaming tail.
  useEffect(() => {
    answerRef.current?.scrollTo({ top: answerRef.current.scrollHeight });
  }, [state.answer]);

  const submit = () => {
    const question = inputRef.current?.value ?? '';
    const started = startAsk(
      { type: 'SUBMIT', question, at: performance.now() },
      stateRef.current.hasHistory,
    );
    if (started && inputRef.current) inputRef.current.value = '';
  };

  const newChat = () => {
    dispatch({ type: 'NEW_CHAT' });
    inputRef.current?.focus();
  };

  const copyAnswer = () => {
    const done = () => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    };
    if (navigator.clipboard?.writeText) {
      void navigator.clipboard
        .writeText(state.answer)
        .then(done, () => copyFallback(state.answer, done));
    } else {
      copyFallback(state.answer, done);
    }
  };

  const busy = state.phase === 'asking' || state.phase === 'streaming';
  const showAnswer = state.answer !== '' || busy;
  // Round 2: actions belong to the displayed answer, not to a phase —
  // dismiss → re-summon keeps the answer, so it keeps Copy too.
  const showActions = state.answer !== '' && !busy && state.phase !== 'error';
  const showNewChat = !busy && (state.hasHistory || state.answer !== '' || state.error !== null);

  return (
    <div className="overlay" ref={cardRef}>
      <header className="ov-head" data-tauri-drag-region>
        <span
          className="ov-dot"
          data-busy={busy || undefined}
          data-recording={recording || undefined}
          title={recording ? 'recording' : 'idle'}
        />
        <span className="ov-title" data-tauri-drag-region>
          Auricle Copilot
        </span>
        {!connected && <span className="ov-badge ov-badge-warn">engine offline</span>}
        {showNewChat && (
          <button className="ov-icon-btn" title="New chat" aria-label="New chat" onClick={newChat}>
            +
          </button>
        )}
        {/* What the next ask may capture; the chips report what one did. */}
        <button
          className="ov-icon-btn ov-toggle"
          title={
            includeScreen
              ? 'Screen on: the next ask reads the active window'
              : 'Screen off: the next ask will not read your screen'
          }
          aria-label="Include screen"
          aria-pressed={includeScreen}
          onClick={() => setIncludeScreen((on) => !on)}
        >
          🖥
        </button>
        <button
          className="ov-icon-btn"
          title="Dismiss (Esc)"
          onClick={() => {
            dispatch({ type: 'DISMISS' });
            void invoke('hide_overlay');
          }}
        >
          ×
        </button>
      </header>

      {init && init.hotkey_warnings.length > 0 && (
        <div className="ov-warning">
          {init.hotkey_warnings.map((w, i) => (
            <div key={i}>⚠ {w}</div>
          ))}
        </div>
      )}

      {toast && <div className="ov-warning">⚠ {toast}</div>}

      {state.quick && showAnswer && (
        <div className="ov-quick">
          <span className="ov-quick-bolt">⚡</span>
          Quick assist
          <span className="ov-quick-q">— what's happening right now?</span>
        </div>
      )}

      {state.chips.length > 0 && (
        <div className="ov-chips">
          {state.chips.map((c, i) => (
            <span key={i} className="ov-chip">
              <span className="ov-chip-ico">{c.icon === 'screen' ? '🖥' : '🎙'}</span>
              {c.label}
            </span>
          ))}
        </div>
      )}

      {showAnswer && (
        <div className="ov-answer" ref={answerRef}>
          {state.answer === '' && state.phase === 'asking' ? (
            <div className="ov-pending">
              gathering context<span className="ov-ellipsis" />
            </div>
          ) : state.answer === '' && state.phase === 'streaming' ? (
            // Context is in; the model hasn't produced its first token
            // yet (a local model can sit here for a while).
            <div className="ov-pending">
              <span className="ov-caret" /> {state.provider ?? 'the model'} is thinking
              <span className="ov-ellipsis" />
            </div>
          ) : (
            <>
              <Markdown source={state.answer} />
              {state.phase === 'streaming' && <span className="ov-caret" />}
            </>
          )}
        </div>
      )}

      {state.phase === 'error' && <div className="ov-error">⚠ {state.error}</div>}

      {showActions && (
        <div className="ov-actions">
          <button className="ov-btn" onClick={copyAnswer}>
            {copied ? 'Copied ✓' : 'Copy'}
          </button>
          {state.provider && state.elapsedMs !== null && (
            <span
              className="ov-usage"
              title={state.usage ? `${state.usage.total_tokens} tokens` : undefined}
            >
              {state.provider} · {(state.elapsedMs / 1000).toFixed(1)}s
            </span>
          )}
        </div>
      )}

      <footer className="ov-input-row">
        <textarea
          ref={inputRef}
          className="ov-input"
          rows={1}
          placeholder={
            busy
              ? 'answering…'
              : state.hasHistory
                ? 'Follow up… (Enter · Esc)'
                : includeScreen
                  ? 'Ask about the meeting or your screen… (Enter · Esc)'
                  : 'Ask about the meeting… (Enter · Esc)'
          }
          disabled={busy}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
        />
      </footer>
    </div>
  );
}
