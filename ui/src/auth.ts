import { useAuricle } from './store';

const KEY = 'auricle-token';

/**
 * The daemon's bearer token, for a bind beyond localhost. sessionStorage and
 * never localStorage: it dies with the tab. Storage can be blocked outright,
 * which only means running tokenless.
 */
export function readToken(): string | null {
  try {
    return sessionStorage.getItem(KEY);
  } catch {
    return null;
  }
}

export function saveToken(token: string): void {
  sessionStorage.setItem(KEY, token);
}

/**
 * What a header can carry. Anything else makes fetch throw, and some browsers
 * quote the offending value in that error.
 */
export const validToken = (token: string): boolean => /^[\x20-\x7e]+$/.test(token);

export function withBearer(init: RequestInit | undefined, token: string | null) {
  if (!token) return init;
  const headers = new Headers(init?.headers);
  headers.set('Authorization', `Bearer ${token}`);
  return { ...init, headers };
}

/** For URLs the browser loads itself: a socket or <audio> cannot set a header. */
export function withToken(url: string): string {
  const token = readToken();
  if (!token) return url;
  return `${url}${url.includes('?') ? '&' : '?'}token=${encodeURIComponent(token)}`;
}

/** fetch, with the token on the way out and the token prompt on a 401. */
export async function authFetch(url: string, init?: RequestInit): Promise<Response> {
  const token = readToken();
  const resp = await fetch(url, withBearer(init, token));
  if (resp.status === 401) {
    if (token) sessionStorage.removeItem(KEY);
    // First one wins: the requests behind a refused token go out with none,
    // and must not turn "refused" back into a plain ask.
    if (useAuricle.getState().tokenPrompt === null) {
      useAuricle.setState({ tokenPrompt: token ? 'rejected' : 'required' });
    }
  }
  return resp;
}
