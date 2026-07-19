import { useEffect, useState } from 'react';
import { api } from '../api';
import { SecretField } from '../components/SecretField';
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

  const mics = devices.filter((d) => d.kind === 'input');
  const loops = devices.filter((d) => d.kind === 'loopback');

  const sttReady = (id: string) => prov?.providers.find((p) => p.id === id)?.ready ?? false;
  const llmReady = (id: string) => prov?.llm.find((p) => p.id === id)?.ready ?? false;
  const keySlots = [
    { id: 'deepgram', label: 'Deepgram', used: 'streaming STT (lowest latency)', ready: sttReady('deepgram') },
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
  const idx = order.indexOf(step);
  const back = () => idx > 0 && setStep(order[idx - 1]);
  const next = () => idx < order.length - 1 && setStep(order[idx + 1]);

  return (
    <div className="onboarding">
      <div className="onboarding-card">
        <div className="steps">
          {order.map((s, i) => (
            <span key={s} className={`step-dot ${i === idx ? 'active' : ''} ${i < idx ? 'done' : ''}`} />
          ))}
        </div>

        {err && <div className="banner error">{err}</div>}

        {step === 'welcome' && (
          <div className="ob-step">
            <h1>
              auricle<span className="brand-dot">.</span>
            </h1>
            <p className="dim">
              A local-first meeting engine. Your microphone is <em>You</em>; everything your
              computer plays is <em>Them</em>. Nothing leaves your machine unless you choose a cloud
              provider. Let&rsquo;s get you set up.
            </p>
            <div className="onboarding-nav">
              <button className="btn subtle" onClick={() => void commit({})} disabled={saving}>
                Skip
              </button>
              <button className="btn accent" onClick={next}>
                Get started
              </button>
            </div>
          </div>
        )}

        {step === 'devices' && (
          <div className="ob-step">
            <h2>Choose your audio devices</h2>
            <label className="ob-field">
              <span>Microphone (“You”)</span>
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
              <span>System audio (“Them”)</span>
              <select value={loop} onChange={(e) => setLoop(e.target.value)}>
                <option value="default">System default</option>
                {loops.map((d) => (
                  <option key={d.name} value={d.name}>
                    {d.name} {d.is_default ? '(default)' : ''}
                  </option>
                ))}
              </select>
            </label>
            <p className="dim note">
              System audio is captured via WASAPI loopback on your output device — silence while
              nothing plays is normal.
            </p>
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
            <h2>How should Auricle transcribe?</h2>
            <div className="choice-cards">
              <button
                className={`choice-card ${mode === 'local' ? 'selected' : ''}`}
                onClick={() => setMode('local')}
              >
                <span className="choice-title">Fully local</span>
                <span className="dim">
                  Whisper + Ollama on your machine. Nothing leaves the device. Slower on modest
                  CPUs.
                </span>
              </button>
              <button
                className={`choice-card ${mode === 'cloud' ? 'selected' : ''}`}
                onClick={() => setMode('cloud')}
              >
                <span className="choice-title">Cloud</span>
                <span className="dim">
                  Deepgram + Groq for low latency. Audio and prompts go to those providers. Needs
                  API keys.
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
            <p className="dim">
              Stored in the OS credential store — never in config files. You can add or change these
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
            <p className="dim">
              {mode === 'cloud'
                ? 'Cloud transcription with Deepgram and Groq. Add keys anytime in Settings.'
                : 'Fully local transcription with Whisper and Ollama — nothing leaves your machine.'}{' '}
              Press <strong>Start Recording</strong> in the sidebar to capture your next meeting.
            </p>
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
