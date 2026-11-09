// The dictation setup check, as lines the overlay can show. Pure, so every
// combination is tested without Tauri.

export interface DictationProvider {
  id: string;
  local: boolean;
  ready: boolean;
  detail: string | null;
  is_default: boolean;
}

export interface DictationCheck {
  hotkey: string;
  mode: 'hold' | 'toggle';
  registered: boolean;
  hotkey_error: string | null;
  engine_up: boolean;
  provider: DictationProvider | null;
}

export interface CheckLine {
  ok: boolean;
  text: string;
}

const LOCAL_DEFAULT = 'Local (whisper-local, on this machine) is the default.';

export function checkLines(check: DictationCheck): CheckLine[] {
  const how =
    check.mode === 'hold'
      ? 'hold it while you talk, let go to paste'
      : 'press it to start, press it again to paste';
  const hotkey: CheckLine = check.registered
    ? { ok: true, text: `${check.hotkey} is ready: ${how}.` }
    : {
        ok: false,
        text:
          `${check.hotkey} isn't available${check.hotkey_error ? ` (${check.hotkey_error})` : ''}. ` +
          'Set another under [hotkeys] dictate in overlay.toml.',
      };
  const engine: CheckLine = check.engine_up
    ? { ok: true, text: 'The engine is running.' }
    : { ok: false, text: "The engine isn't answering, so dictation can't start yet." };
  return [hotkey, engine, providerLine(check.provider)];
}

function providerLine(p: DictationProvider | null): CheckLine {
  if (p === null) {
    return { ok: false, text: `The provider shows once the engine answers. ${LOCAL_DEFAULT}` };
  }
  const where = p.local
    ? `Dictation uses ${p.id}, which runs on this machine.`
    : `Dictation uses ${p.id}, a cloud service: your voice leaves this machine.`;
  const notReady = p.ready ? '' : ` It isn't ready${p.detail ? `: ${p.detail}` : ''}.`;
  const backToLocal = p.is_default ? '' : ' Set dictation_provider to whisper-local to go back.';
  return { ok: p.local && p.ready, text: `${where}${notReady} ${LOCAL_DEFAULT}${backToLocal}` };
}
