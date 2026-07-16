// Context chips — the honesty affordance: they state exactly what the
// ask actually captured, derived from the engine's ask_started SSE event
// (never from what the overlay merely intended to send).

import type { Chip } from './state';

export interface AskStartedInfo {
  screen: { window_title: string; app_name: string } | null;
  transcript_segments: number | null;
}

const TITLE_MAX = 34;

/**
 * `windowMin` is the transcript window length shown on the chip. The
 * engine does not expose `copilot.transcript_window_min` over the API,
 * so the overlay carries its own copy in overlay.toml (default 10,
 * matching the engine default).
 */
export function deriveChips(info: AskStartedInfo, windowMin: number): Chip[] {
  const chips: Chip[] = [];
  if (info.screen) {
    let title = info.screen.window_title.trim();
    if (title.length > TITLE_MAX) title = `${title.slice(0, TITLE_MAX - 1)}…`;
    // The window title is what the user recognizes; the app name only
    // adds signal when the title doesn't already carry it.
    const app = info.screen.app_name.trim();
    const label =
      title === '' ? app : title.toLowerCase().includes(app.toLowerCase()) ? title : `${app} — ${title}`;
    chips.push({ icon: 'screen', label });
  }
  if (info.transcript_segments !== null) {
    chips.push({
      icon: 'transcript',
      label:
        info.transcript_segments === 0
          ? 'transcript: empty window'
          : `transcript: last ${windowMin} min · ${info.transcript_segments} segments`,
    });
  }
  return chips;
}
