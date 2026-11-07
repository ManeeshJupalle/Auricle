// The dictation pill's page. The Rust side decides every word and when the
// window shows; this only renders the latest state it sends.
import '@fontsource/geist-sans/400.css';
import '@fontsource/geist-sans/500.css';
import './styles.css';
import './pill.css';
import { listen } from '@tauri-apps/api/event';

interface PillView {
  kind: 'listening' | 'working' | 'done' | 'failed';
  text: string;
  hint: string | null;
  theme: string;
}

const pill = document.getElementById('pill')!;
const text = pill.querySelector<HTMLElement>('.pill-text')!;
const hint = pill.querySelector<HTMLElement>('.pill-hint')!;

void listen<PillView>('pill', ({ payload }) => {
  document.documentElement.dataset.theme = payload.theme;
  pill.dataset.kind = payload.kind;
  text.textContent = payload.text;
  hint.textContent = payload.hint ?? '';
  hint.hidden = payload.hint === null;
  pill.hidden = false;
});
