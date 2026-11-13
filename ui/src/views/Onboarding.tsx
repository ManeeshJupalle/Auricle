import { useEffect, useState } from 'react';
import { api } from '../api';
import { SecretField } from '../components/SecretField';
import { TwoVoices } from '../components/TwoVoices';
import type { DeviceInfo, ProvidersResponse } from '../types';

type Step = 'welcome' | 'devices' | 'mode' | 'keys' | 'done';

/**
 * First-run setup. Shown until the `onboarded` setting is true; writes the
 * user's device, provider, and mode choices through PUT /api/v1/settings
 * and stores any keys via the secrets API. "Skip" just marks onboarding
 * done so nothing is forced.
 */
export function Onboarding({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState<Step>('welcome');
  const [devices, setDevices] = useState<DeviceInfo[]>([]);
  const [mic, setMic] = useState('default');
  const [loop, setLoop] = useState('default');
  const [mode, setMode] = useState<'local' | 'cloud'>('local');
  const [prov, setProv] = useState<ProvidersResponse | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    Promise.all([api.devices(), api.providers()])
      .then(([d, p]) => {
        setDevices(d);
        setProv(p);
      })
      .catch((e) => setErr((e as Error).message));
  }, []);

  const reloadProviders = () => {
    api.providers().then(setProv).catch(() => {});
  };

  // The last step says whether Ollama is there, and someone may have just
  // installed it in another window.
  useEffect(() => {
    if (step === 'done') reloadProviders();
  }, [step]);

  const mics = devices.filter((d) => d.kind === 'input');
  const loops = devices.filter((d) => d.kind === 'loopback');

  const sttReady = (id: string) => prov?.providers.find((p) => p.id === id)?.ready ?? false;
  const llmReady = (id: string) => prov?.llm.find((p) => p.id === id)?.ready ?? false;
  // Set only when the engine asked Ollama and it wasn't there, or lacked the model.
  const ollamaMissing = prov?.llm.find((p) => p.id === 'ollama' && !p.ready)?.detail;
  const keySlots = [
    { id: 'deepgram', label: 'Deepgram', used: 'live transcription, lowest delay', ready: sttReady('deepgram') },
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

  const commit = async (extra: Record<string, unknown>) => {
    setSaving(true);
    setErr(null);
    try {
      await api.putSettings({ onboarded: true, ...extra });
      onDone();
    } catch (e) {
      setErr((e as Error).message);
      setSaving(false);
    }
  };

  const finish = () =>
    commit({
      mic_device: mic,
      loopback_device: loop,
      default_provider: mode === 'cloud' ? 'deepgram' : 'whisper-local',
      llm_provider: mode === 'cloud' ? 'groq' : 'ollama',
    });

  const order: Step[] = ['welcome', 'devices', 'mode', ...(mode === 'cloud' ? ['keys' as const] : []), 'done'];
  // Every step stays on the line, so choosing the cloud doesn't add one mid-way.
  const allSteps: Step[] = ['welcome', 'devices', 'mode', 'keys', 'done'];
  const idx = order.indexOf(step);
  const back = () => idx > 0 && setStep(order[idx - 1]);
  const next = () => idx < order.length - 1 && setStep(order[idx + 1]);

  return (
    <div className="onboarding">
      <div className="onboarding-rule">
        <div className="steps" role="img" aria-label={`Step ${idx + 1} of ${order.length}`}>
          {allSteps.map((s, i) => (
            <span
              key={s}
              className={`step-dot ${s === step ? 'active' : ''} ${
                i < allSteps.indexOf(step) ? 'done' : ''
              } ${order.includes(s) ? '' : 'skipped'}`}
            />
          ))}
        </div>
      </div>
      <div className="onboarding-card">
        <div className="status-slot" role="status">
          {err && <div className="banner">{err}</div>}
        </div>

        {step === 'welcome' && (
          <div className="ob-step">
            <h1>Auricle writes down your meetings, on your machine</h1>
            <TwoVoices />
            <p>
              No bot joins your calls. Nothing leaves this computer unless you choose a cloud
              provider. Setup takes a minute.
            </p>
            <div className="onboarding-nav">
              <button className="btn subtle" onClick={() => void commit({})} disabled={saving}>
                Skip setup
              </button>
              <button className="btn accent" onClick={next}>
                Get started
              </button>
            </div>
          </div>
        )}

        {step === 'devices' && (
          <div className="ob-step">
            <h2>What should Auricle listen to?</h2>
            <label className="ob-field">
              <span>Your microphone is You</span>
              <select value={mic} onChange={(e) => setMic(e.target.value)}>
                <option value="default">System default</option>
                {mics.map((d) => (
                  <option key={d.name} value={d.name}>
                    {d.name} {d.is_default ? '(default)' : ''}
                  </option>
                ))}
              </select>
            </label>
            <label className="ob-field">
              <span>What your computer plays is Them</span>
              <select value={loop} onChange={(e) => setLoop(e.target.value)}>
                <option value="default">System default</option>
                {loops.map((d) => (
                  <option key={d.name} value={d.name}>
                    {d.name} {d.is_default ? '(default)' : ''}
                  </option>
                ))}
              </select>
            </label>
            <p>Them stays quiet while nothing is playing. That is normal.</p>
            <div className="onboarding-nav">
              <button className="btn subtle" onClick={back}>
                Back
              </button>
              <button className="btn accent" onClick={next}>
                Continue
              </button>
            </div>
          </div>
        )}

        {step === 'mode' && (
          <div className="ob-step">
            <h2>Where should the transcribing happen?</h2>
            <div className="choice-cards">
              <button
                className={`choice-card ${mode === 'local' ? 'selected' : ''}`}
                onClick={() => setMode('local')}
              >
                <span className="choice-title">On this machine</span>
                <span className="dim">
                  Nothing leaves this computer. Transcription works as installed, and its speech
                  model downloads the first time you record. Summaries and answers need Ollama, a
                  free app you install yourself. Slower on modest processors.
                </span>
              </button>
              <button
                className={`choice-card ${mode === 'cloud' ? 'selected' : ''}`}
                onClick={() => setMode('cloud')}
              >
                <span className="choice-title">In the cloud</span>
                <span className="dim">
                  Deepgram and Groq, for the lowest delay. Your audio and prompts go to them, and
                  you need API keys for both.
                </span>
              </button>
            </div>
            <div className="onboarding-nav">
              <button className="btn subtle" onClick={back}>
                Back
              </button>
              <button className="btn accent" onClick={next}>
                Continue
              </button>
            </div>
          </div>
        )}

        {step === 'keys' && (
          <div className="ob-step">
            <h2>Add your cloud keys</h2>
            <p>
              Keys go in Windows Credential Manager, never in a config file. You can change them
              later in Settings.
            </p>
            <div className="ob-keys">
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
            </div>
            <div className="onboarding-nav">
              <button className="btn subtle" onClick={back}>
                Back
              </button>
              <button className="btn accent" onClick={next}>
                Continue
              </button>
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="ob-step">
            <h2>You&rsquo;re ready</h2>
            <p>
              {mode === 'cloud'
                ? 'Deepgram and Groq will transcribe and summarize. You can add keys at any time in Settings.'
                : ollamaMissing
                  ? 'Whisper will transcribe here, and nothing leaves this computer.'
                  : 'Whisper and Ollama will transcribe and summarize here, and nothing leaves this computer.'}{' '}
              Press Start recording when your next meeting begins.
            </p>
            {mode === 'local' && ollamaMissing && (
              <p>
                Summaries and the copilot&rsquo;s answers won&rsquo;t work yet. {ollamaMissing}.{' '}
                <button className="link-btn" onClick={reloadProviders}>
                  Check again
                </button>
              </p>
            )}
            <div className="onboarding-nav">
              <button className="btn subtle" onClick={back}>
                Back
              </button>
              <button className="btn accent" onClick={() => void finish()} disabled={saving}>
                {saving ? 'Saving…' : 'Start using Auricle'}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
