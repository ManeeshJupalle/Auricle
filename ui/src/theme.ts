import { api } from './api';

export type Theme = 'dark' | 'light';

const KEY = 'auricle-theme';

export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
  localStorage.setItem(KEY, theme);
}

/** Boot: localStorage wins for instant paint, then the server setting. */
export function initTheme(): void {
  const stored = localStorage.getItem(KEY);
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
