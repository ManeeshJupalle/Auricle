import { useState } from 'react';
import { api } from '../api';

/**
 * One credential slot: readiness dot + a write-only key input backed by
 * PUT/DELETE /api/v1/secrets/{id}. The value is never read back from the
 * server; `ready` is derived by the parent from GET /api/v1/providers, and
 * `onChanged` asks the parent to re-fetch it after a save or removal.
 */
export function SecretField({
  id,
  label,
  used,
  ready,
  onChanged,
}: {
  id: string;
  label: string;
  used: string;
  ready: boolean;
  onChanged: () => void;
}) {
  const [value, setValue] = useState('');
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const flash = (m: string) => {
    setMsg(m);
    setTimeout(() => setMsg(null), 1500);
  };

  const save = async () => {
    if (!value.trim() || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await api.putSecret(id, value.trim());
      setValue('');
      flash('saved');
      onChanged();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setBusy(true);
    setErr(null);
    try {
      await api.deleteSecret(id);
      flash('removed');
      onChanged();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="secret-field">
      <div className="secret-head">
        <span className={`dot ${ready ? 'ok' : 'missing'}`} />
        <span className="secret-label">{label}</span>
        <span className="dim secret-use">{used}</span>
        {ready && (
          <button className="secret-remove" onClick={() => void remove()} disabled={busy}>
            Remove
          </button>
        )}
      </div>
      <div className="secret-row">
        <input
          type="password"
          value={value}
          placeholder={ready ? 'Replace stored key…' : 'Paste API key…'}
          autoComplete="off"
          spellCheck={false}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') void save();
          }}
        />
        <button className="btn accent" onClick={() => void save()} disabled={busy || !value.trim()}>
          {busy ? '…' : 'Save'}
        </button>
        {msg && <span className="saved">{msg}</span>}
      </div>
      {err && <div className="secret-err">{err}</div>}
    </div>
  );
}
