import { useEffect, useState } from 'react';
import { api } from '../api';
import { SecretField } from '../components/SecretField';
import { canPick, firstUseNote } from '../components/Sidebar';
import { useAuricle } from '../store';
import { applyTheme, currentTheme, type Theme } from '../theme';
import { providerName } from '../words';
import type {
  DeviceInfo,
  LlmProviderInfo,
  ProviderInfo,
  Settings as SettingsMap,
} from '../types';

/** The days a retention field holds, or null for anything but a whole number. */
export function parseRetentionDays(text: string): number | null {
  const days = Number(text.trim());
  return /^\d+$/.test(text.trim()) && Number.isSafeInteger(days) ? days : null;
}

/** True when a change would delete meetings the current setting keeps. */
export function retentionDeletesMore(next: number, current: number): boolean {
  return next > 0 && (current === 0 || next < current);
}

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
  // What the field shows while it is being edited; null shows the saved value.
  const [retentionDraft, setRetentionDraft] = useState<string | null>(null);
  const [backingUp, setBackingUp] = useState(false);
  const [backupPath, setBackupPath] = useState<string | null>(null);
  const settingsChanged = useAuricle((s) => s.settingsChanged);

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
    settingsChanged();
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
    { id: 'deepgram', label: 'Deepgram', used: 'live transcription', ready: sttReady('deepgram') },
    {
      id: 'groq',
      label: 'Groq',
      used: 'transcription, summaries, and answers',
      ready: sttReady('groq-whisper') || llmReady('groq'),
    },
    {
      id: 'openai',
      label: 'OpenAI-compatible',
      used: 'transcription, summaries, and answers',
      ready: sttReady('openai-compat') || llmReady('openai-compat'),
    },
  ];

  const save = (patch: SettingsMap) => {
    setError(null);
    api
      .putSettings(patch)
      .then((all) => {
        setSettings(all);
        settingsChanged();
        const key = Object.keys(patch)[0];
        setSaved(key ?? null);
        setTimeout(() => setSaved(null), 1500);
      })
      .catch((e) => setError((e as Error).message));
  };

  const str = (key: string, fallback = 'default'): string =>
    typeof settings[key] === 'string' ? (settings[key] as string) : fallback;

  const retentionDays = typeof settings.retention_days === 'number' ? settings.retention_days : 0;

  const saveRetention = () => {
    const days = retentionDraft === null ? null : parseRetentionDays(retentionDraft);
    setRetentionDraft(null);
    if (days === null || days === retentionDays) return;
    if (
      retentionDeletesMore(days, retentionDays) &&
      !window.confirm(
        `Permanently delete every meeting older than ${days} ${days === 1 ? 'day' : 'days'}, ` +
          'with its transcript, summaries, and audio? This cannot be undone.',
      )
    ) {
      return;
    }
    save({ retention_days: days });
  };

  const backUp = () => {
    setBackingUp(true);
    setBackupPath(null);
    setError(null);
    api
      .backup()
      .then((b) => setBackupPath(b.path))
      .catch((e) => setError(`The backup failed: ${(e as Error).message}`))
      .finally(() => setBackingUp(false));
  };

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
      <h1 className="page-title">Settings</h1>
      <div className="status-slot" role="status">
        {error && <div className="banner">{error}</div>}
      </div>

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
          {saved === 'ui_theme' && <span className="saved">Saved</span>}
        </label>
      </section>

      <section className="panel">
        <h2>Audio devices</h2>
        <label>
          <span>Your microphone</span>
          <select value={str('mic_device')} onChange={(e) => save({ mic_device: e.target.value })}>
            <option value="default">System default</option>
            {mics.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name} {d.is_default ? '(default)' : ''}
              </option>
            ))}
          </select>
          {saved === 'mic_device' && <span className="saved">Saved</span>}
        </label>
        <label>
          <span>What your computer plays</span>
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
          {saved === 'loopback_device' && <span className="saved">Saved</span>}
        </label>
        <p className="note">
          The microphone is You and what your computer plays is Them. Them stays quiet while nothing
          is playing, and that is normal.
        </p>
      </section>

      <section className="panel">
        <h2>Transcription</h2>
        <label>
          <span>Transcribe with</span>
          <select
            value={str('default_provider', providers.find((p) => p.default)?.id ?? '')}
            onChange={(e) => save({ default_provider: e.target.value })}
          >
            {providers.map((p) => (
              <option key={p.id} value={p.id} disabled={!canPick(p)}>
                {providerName(p.id)} {p.kind === 'local' ? 'on this machine' : 'in the cloud'}
                {p.ready ? '' : firstUseNote(p) ? ', downloads on first use' : ', not ready'}
              </option>
            ))}
          </select>
          {saved === 'default_provider' && <span className="saved">Saved</span>}
        </label>
        {providers.some(firstUseNote) && (
          <p className="note">
            The local model isn&rsquo;t on this machine yet. It downloads the first time you record
            with Whisper, then transcription runs here with nothing sent out.
          </p>
        )}
        <label>
          <span>Local Whisper model</span>
          <select
            value={str('whisper_model', 'base.en')}
            onChange={(e) => save({ whisper_model: e.target.value })}
          >
            <option value="base.en">base.en: English, keeps up with live speech</option>
            <option value="small.en">small.en: English, more accurate, too slow for live on most laptops</option>
            <option value="base">base: any language</option>
            <option value="small">small: any language, too slow for live on most laptops</option>
          </select>
          {saved === 'whisper_model' && <span className="saved">Saved</span>}
        </label>
        <label>
          <span>Language</span>
          <select
            value={str('stt_language', 'en')}
            onChange={(e) => save({ stt_language: e.target.value })}
          >
            <option value="en">English</option>
            <option value="auto">Detect automatically</option>
            <option value="es">Spanish</option>
            <option value="fr">French</option>
            <option value="de">German</option>
            <option value="pt">Portuguese</option>
            <option value="hi">Hindi</option>
            <option value="ja">Japanese</option>
            <option value="zh">Chinese</option>
          </select>
          {saved === 'stt_language' && <span className="saved">Saved</span>}
        </label>
        {str('stt_language', 'en') !== 'en' && str('whisper_model', 'base.en').endsWith('.en') && (
          <p className="note">
            The .en models only understand English. With Whisper, pick base or small for other
            languages.
          </p>
        )}
        <label>
          <span>Summaries and titles</span>
          <select
            value={str('llm_provider', 'ollama')}
            onChange={(e) => save({ llm_provider: e.target.value })}
          >
            <option value="ollama">Ollama on this machine</option>
            <option value="groq">Groq in the cloud</option>
            <option value="openai-compat">OpenAI-compatible in the cloud</option>
          </select>
          {saved === 'llm_provider' && <span className="saved">Saved</span>}
        </label>
        <label>
          <span>Ollama model</span>
          <input
            type="text"
            defaultValue={str('ollama_model', '')}
            placeholder="A model you have installed, like qwen3:8b"
            onBlur={(e) => {
              if (e.target.value.trim()) save({ ollama_model: e.target.value.trim() });
            }}
          />
          {saved === 'ollama_model' && <span className="saved">Saved</span>}
        </label>
        <label className="toggle">
          <input
            type="checkbox"
            checked={settings['dictation_polish'] === true}
            onChange={(e) => save({ dictation_polish: e.target.checked })}
          />
          <span>Tidy dictation with the LLM above</span>
          {saved === 'dictation_polish' && <span className="saved">Saved</span>}
        </label>
        <p className="note">
          Off by default. The LLM fixes punctuation, capitals, and filler words before your
          dictation is pasted, and it goes in as heard if that fails or takes over 8 seconds. With a
          cloud LLM, what you dictate is sent there.
        </p>
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
          <p className="note">
            Keys are kept in Windows Credential Manager, never in a config file, and are never shown
            again. An environment variable of the same name still wins if one is set.
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
          <span>Keep audio recordings</span>
          {saved === 'retain_raw_audio' && <span className="saved">Saved</span>}
        </label>
        <p className="note">
          Off by default. Future meetings keep their audio on this machine, as two 16 kHz WAV files
          beside the database, and you can play them back from the transcript.
        </p>
        <label className="toggle">
          <input
            type="checkbox"
            checked={settings['redact_pii'] === true}
            onChange={(e) => save({ redact_pii: e.target.checked })}
          />
          <span>Remove personal details from transcripts</span>
          {saved === 'redact_pii' && <span className="saved">Saved</span>}
        </label>
        <p className="note">
          Off by default. Emails, card numbers, SSNs, phone numbers, IP addresses, and API keys are
          replaced with a label like [email] before the text is stored or sent to a cloud provider.
          Applies to future meetings, and to transcript text only.
        </p>
        <label className="toggle">
          <input
            type="checkbox"
            checked={settings['mcp_enabled'] === true}
            onChange={(e) => save({ mcp_enabled: e.target.checked })}
          />
          <span>Let local AI agents read your transcripts</span>
          {saved === 'mcp_enabled' && <span className="saved">Saved</span>}
        </label>
        <p className="note">
          Off by default. Any agent on this machine that speaks MCP can read your live and past
          transcripts at <span className="mono">/mcp</span>. Agents cannot start a recording, see
          your screen, or run a model. Every read appears on the Privacy page, but where an agent
          sends what it read is out of Auricle&rsquo;s sight.
        </p>
      </section>

      <section className="panel">
        <h2>Your data</h2>
        <label className="has-suffix">
          <span>Delete meetings after</span>
          <input
            type="text"
            inputMode="numeric"
            className="retention-days"
            value={retentionDraft ?? String(retentionDays)}
            onChange={(e) => setRetentionDraft(e.target.value)}
            onBlur={saveRetention}
            onKeyDown={(e) => {
              if (e.key === 'Enter') e.currentTarget.blur();
            }}
          />
          <span className="dim">days (0 keeps them forever)</span>
          {saved === 'retention_days' && <span className="saved">Saved</span>}
        </label>
        <p className="note">
          Off by default. With a number of days set, Auricle permanently deletes older meetings,
          with their transcripts, summaries, and audio, at every start and once a day. There is no
          undo and no trash. A meeting in progress is never touched.
        </p>
        <div className="backup-row">
          <button className="btn" onClick={backUp} disabled={backingUp}>
            {backingUp ? 'Backing up…' : 'Back up database'}
          </button>
          <span className="mono dim" role="status">
            {backupPath && `Saved to ${backupPath}`}
          </span>
        </div>
        <p className="note">
          Writes a copy of the database to a backups folder beside it. Audio files are not part of
          the backup.
        </p>
      </section>
    </div>
  );
}
