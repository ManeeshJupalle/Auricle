import { api } from './api';

export type Theme = 'dark' | 'light';

const KEY = 'auricle-theme';

/**
 * A browser set to block site data throws on any touch of localStorage, and
 * this runs before the app mounts: unguarded, the page stays blank. The
 * server setting still carries the choice.
 */
export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // Blocked: the next load paints dark first, then takes the setting.
  }
}

/** Boot: localStorage wins for instant paint, then the server setting. */
export function initTheme(): void {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(KEY);
  } catch {
    // Blocked, as above.
  }
  applyTheme(stored === 'light' ? 'light' : 'dark');
  api
    .settings()
    .then((s) => {
      if (s['ui_theme'] === 'light' || s['ui_theme'] === 'dark') {
        applyTheme(s['ui_theme']);
      }
    })
    .catch(() => {});
}

export function currentTheme(): Theme {
  return document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
}
