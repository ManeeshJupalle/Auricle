import { useEffect, useState } from 'react';
import { api } from '../api';
import { SecretField } from '../components/SecretField';
import { applyTheme, currentTheme, type Theme } from '../theme';
import type {
  DeviceInfo,
  LlmProviderInfo,
  ProviderInfo,
  Settings as SettingsMap,
} from '../types';

/**
 * Settings persist via PUT /api/v1/settings and overlay the daemon's TOML
 * config for subsequent sessions (see docs/API.md "Recognized settings").
 */
export function Settings() {
  const [devices, setDevices] = useState<DeviceInfo[]>([]);
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [llm, setLlm] = useState<LlmProviderInfo[]>([]);
  const [settings, setSettings] = useState<SettingsMap>({});
  const [saved, setSaved] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    Promise.all([api.devices(), api.providers(), api.settings()])
      .then(([d, p, s]) => {
        setDevices(d);
        setProviders(p.providers);
        setLlm(p.llm);
        setSettings(s);
      })
      .catch((e) => setError((e as Error).message));
  }, []);

  // Re-read provider/LLM readiness after a key is stored or removed.
  const reloadProviders = () => {
    api
      .providers()
      .then((p) => {
        setProviders(p.providers);
        setLlm(p.llm);
      })
      .catch((e) => setError((e as Error).message));
  };

  // One credential slot can back several providers (Groq and any
  // OpenAI-compatible endpoint serve both STT and the LLM). A slot is
  // "ready" when any provider that uses it reports a key present.
  const sttReady = (id: string) => providers.find((p) => p.id === id)?.ready ?? false;
  const llmReady = (id: string) => llm.find((p) => p.id === id)?.ready ?? false;
  const keySlots = [
    { id: 'deepgram', label: 'Deepgram', used: 'streaming STT', ready: sttReady('deepgram') },
    {
      id: 'groq',
      label: 'Groq',
      used: 'batch STT + summaries & answers',
      ready: sttReady('groq-whisper') || llmReady('groq'),
    },
    {
      id: 'openai',
      label: 'OpenAI-compatible',
      used: 'batch STT + summaries & answers',
      ready: sttReady('openai-compat') || llmReady('openai-compat'),
    },
  ];

  const save = (patch: SettingsMap) => {
    setError(null);
    api
      .putSettings(patch)
      .then((all) => {
        setSettings(all);
        const key = Object.keys(patch)[0];
        setSaved(key ?? null);
        setTimeout(() => setSaved(null), 1500);
      })
      .catch((e) => setError((e as Error).message));
  };

  const str = (key: string, fallback = 'default'): string =>
    typeof settings[key] === 'string' ? (settings[key] as string) : fallback;

  const mics = devices.filter((d) => d.kind === 'input');
  const loops = devices.filter((d) => d.kind === 'loopback');

  const [theme, setTheme] = useState<Theme>(currentTheme());
  const pickTheme = (t: Theme) => {
    setTheme(t);
    applyTheme(t);
    save({ ui_theme: t });
  };

  return (
    <div className="settings">
      {error && <div className="banner warn">{error}</div>}

      <section className="panel">
        <h2>Appearance</h2>
        <label>
          <span>Theme</span>
          <div className="theme-toggle">
            <button
              className={`btn ${theme === 'dark' ? 'accent' : ''}`}
              onClick={() => pickTheme('dark')}
            >
              Dark
            </button>
            <button
              className={`btn ${theme === 'light' ? 'accent' : ''}`}
              onClick={() => pickTheme('light')}
            >
              Light
            </button>
          </div>
          {saved === 'ui_theme' && <span className="saved">saved</span>}
        </label>
      </section>

      <section className="panel">
        <h2>Audio devices</h2>
        <label>
          <span>Microphone (“You”)</span>
          <select value={str('mic_device')} onChange={(e) => save({ mic_device: e.target.value })}>
            <option value="default">System default</option>
            {mics.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name} {d.is_default ? '(default)' : ''}
              </option>
            ))}
          </select>
          {saved === 'mic_device' && <span className="saved">saved</span>}
        </label>
        <label>
          <span>System audio (“Them”)</span>
          <select
            value={str('loopback_device')}
            onChange={(e) => save({ loopback_device: e.target.value })}
          >
            <option value="default">System default</option>
            {loops.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name} {d.is_default ? '(default)' : ''}
              </option>
            ))}
          </select>
          {saved === 'loopback_device' && <span className="saved">saved</span>}
        </label>
        <p className="dim note">
          System audio is captured via WASAPI loopback on the output device — no audio while
          nothing plays is normal.
        </p>
      </section>

      <section className="panel">
        <h2>Transcription</h2>
        <label>
          <span>Default provider</span>
          <select
            value={str('default_provider', providers.find((p) => p.default)?.id ?? '')}
            onChange={(e) => save({ default_provider: e.target.value })}
          >
            {providers.map((p) => (
              <option key={p.id} value={p.id} disabled={!p.ready}>
                {p.id} ({p.kind}){p.ready ? '' : ' — not ready'}
              </option>
            ))}
          </select>
          {saved === 'default_provider' && <span className="saved">saved</span>}
        </label>
        <label>
          <span>Local whisper model</span>
          <select
            value={str('whisper_model', 'base.en')}
            onChange={(e) => save({ whisper_model: e.target.value })}
          >
            <option value="base.en">base.en — keeps up with live speech</option>
            <option value="small.en">small.en — more accurate; too slow for live on most laptops</option>
          </select>
          {saved === 'whisper_model' && <span className="saved">saved</span>}
        </label>
        <label>
          <span>Summaries & titles LLM</span>
          <select
            value={str('llm_provider', 'ollama')}
            onChange={(e) => save({ llm_provider: e.target.value })}
          >
            <option value="ollama">ollama — local, private</option>
            <option value="groq">groq — cloud, fast</option>
            <option value="openai-compat">openai-compat</option>
          </select>
          {saved === 'llm_provider' && <span className="saved">saved</span>}
        </label>
        <label>
          <span>Ollama model (summaries)</span>
          <input
            type="text"
            defaultValue={str('ollama_model', '')}
            placeholder="e.g. qwen3:8b — a model from `ollama list`"
            onBlur={(e) => {
              if (e.target.value.trim()) save({ ollama_model: e.target.value.trim() });
            }}
          />
          {saved === 'ollama_model' && <span className="saved">saved</span>}
        </label>
        <div className="keys">
          <span>Cloud API keys</span>
          {keySlots.map((slot) => (
            <SecretField
              key={slot.id}
              id={slot.id}
              label={slot.label}
              used={slot.used}
              ready={slot.ready}
              onChanged={reloadProviders}
            />
          ))}
          <p className="dim note">
            Keys are stored in the OS credential store (Windows Credential Manager), never in
            config files, and are never displayed. An environment variable of the same name still
            takes precedence if set.
          </p>
        </div>
      </section>

      <section className="panel">
        <h2>Privacy</h2>
        <label className="toggle">
          <input
            type="checkbox"
            checked={settings['retain_raw_audio'] === true}
            onChange={(e) => save({ retain_raw_audio: e.target.checked })}
          />
          <span>Keep raw audio recordings (per-channel 16 kHz WAVs beside the database)</span>
          {saved === 'retain_raw_audio' && <span className="saved">saved</span>}
        </label>
        <p className="dim note">
          Off by default. When enabled, future sessions store their audio locally and note the
          paths in session metadata.
        </p>
        <label className="toggle">
          <input
            type="checkbox"
            checked={settings['redact_pii'] === true}
            onChange={(e) => save({ redact_pii: e.target.checked })}
          />
          <span>Redact PII from transcripts (emails, cards, SSNs, phone numbers, IPs, API keys)</span>
          {saved === 'redact_pii' && <span className="saved">saved</span>}
        </label>
        <p className="dim note">
          Off by default. When enabled, matches are replaced with a visible label (e.g. [email])
          before the text is stored or sent to any cloud provider. Applies to future sessions and
          covers transcript text only.
        </p>
      </section>
    </div>
  );
}
