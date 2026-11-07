import { useEffect, useRef, useState, type FormEvent } from 'react';
import { saveToken, validToken } from '../auth';
import { useAuricle } from '../store';

/**
 * Opens when the daemon answers 401, which only a token-protected bind does.
 * Modal, and with no way to dismiss it: nothing behind it can load without
 * the token.
 */
export function TokenPrompt() {
  const reason = useAuricle((s) => s.tokenPrompt);
  const [value, setValue] = useState('');
  const dialog = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    if (reason !== null) dialog.current?.showModal();
  }, [reason]);

  if (reason === null) return null;

  const token = value.trim();
  const submit = (e: FormEvent) => {
    e.preventDefault();
    saveToken(token);
    // Every boot request and the socket have already failed; a reload reruns
    // them all, and sessionStorage survives it.
    location.reload();
  };

  return (
    <dialog
      ref={dialog}
      className="token-dialog"
      aria-labelledby="token-title"
      onCancel={(e) => e.preventDefault()}
      // Browsers let a repeated Escape through the cancel above.
      onClose={(e) => e.currentTarget.showModal()}
    >
      <form onSubmit={submit}>
        <h2 id="token-title">Access token</h2>
        <p className="note">
          This copy of Auricle is protected by a token: the value of the{' '}
          <span className="mono">AURICLE_TOKEN</span> environment variable it was started with,
          unless your config names another. Paste it here to connect. It is kept for this tab only
          and forgotten when the tab closes.
        </p>
        {reason === 'rejected' && (
          <div className="banner warn">That token was not accepted. Check it and try again.</div>
        )}
        <input
          type="password"
          aria-label="Access token"
          autoComplete="off"
          value={value}
          onChange={(e) => setValue(e.target.value)}
        />
        <div className="onboarding-nav">
          <button className="btn accent" disabled={!validToken(token)}>
            Connect
          </button>
        </div>
      </form>
    </dialog>
  );
}
