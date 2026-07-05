// UI redesign round-1 screenshots: all views, both themes, live and
// completed sessions, with and without the audio bar.
//
// Usage: node scripts/redesign_shots.mjs <shots dir>
// Assumes: daemon running on :4820 with keys available; a speech WAV at
// AURICLE_DRIVE_WAV (or %TEMP%\auricle_drive_tts.wav) for live audio.

import { chromium } from 'playwright';
import { spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';

const SHOTS = process.argv[2];
const BASE = 'http://127.0.0.1:4820';
const TTS = process.env.AURICLE_DRIVE_WAV ?? `${process.env.TEMP}\\auricle_drive_tts.wav`;
mkdirSync(SHOTS, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (m) => console.log(`[shots] ${m}`);

const setTheme = (theme) =>
  fetch(`${BASE}/api/v1/settings`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ ui_theme: theme }),
  });

function playTts() {
  return spawn(
    'powershell',
    ['-NoProfile', '-c', `$p = New-Object Media.SoundPlayer "${TTS}"; $p.PlaySync()`],
    { stdio: 'ignore' },
  );
}

await setTheme('dark');
// Retained audio for the session we record here → audio bar demo.
await fetch(`${BASE}/api/v1/settings`, {
  method: 'PUT',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ retain_raw_audio: true }),
});

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
page.on('pageerror', (e) => console.log(`[pageerror] ${e.message}`));
await page.goto(BASE);
await page.waitForSelector('.conn.open', { timeout: 10_000 });

// ---------- home / empty state (dark) ----------
await sleep(600);
await page.screenshot({ path: `${SHOTS}/r1_home_dark.png` });
log('home (dark)');

// ---------- live session with partials (dark) ----------
const tts = playTts();
await sleep(500);
await page.selectOption('.provider-pick', 'deepgram');
await page.click('.record-btn');
await page.waitForSelector('.rec', { timeout: 15_000 });
await page.waitForSelector('.seg.partial', { timeout: 30_000 });
await page.screenshot({ path: `${SHOTS}/r1_live_partial_dark.png` });
log('live with partial (dark)');
await sleep(12_000);
await page.screenshot({ path: `${SHOTS}/r1_live_dark.png` });
log('live mid-session (dark)');

// stop from the sidebar
await page.click('.record-btn.stop');
await page.waitForSelector('.record-btn:not(.stop)', { timeout: 30_000 });
tts.kill();
await sleep(1500);

// ---------- stored session: transcript + audio bar (dark) ----------
await page.click('.side-item-main >> nth=0');
await page.waitForSelector('.turn');
await page.waitForSelector('.audio-bar', { timeout: 10_000 });
await sleep(400);
await page.screenshot({ path: `${SHOTS}/r1_transcript_audio_dark.png` });
log('stored transcript with audio bar (dark)');

// ---------- summary tab (dark) ----------
await page.click('.tab:has-text("AI Summary")');
await page.selectOption('.summary-controls select >> nth=1', 'groq');
await page.click('.summary-controls button');
await page.waitForSelector('.summary', { timeout: 60_000 });
await sleep(400);
await page.screenshot({ path: `${SHOTS}/r1_summary_dark.png` });
log('summary tab (dark)');

// ---------- sidebar search ----------
await page.fill('.sidebar-search', 'engineering');
await sleep(700);
await page.screenshot({ path: `${SHOTS}/r1_search_dark.png` });
log('sidebar search (dark)');
await page.fill('.sidebar-search', '');
await sleep(500);

// ---------- light theme: transcript + settings + about ----------
await setTheme('light');
await page.reload();
await page.waitForSelector('.conn.open', { timeout: 10_000 });
await page.click('.side-item-main >> nth=0');
await page.waitForSelector('.turn');
await page.waitForSelector('.audio-bar', { timeout: 10_000 });
await sleep(400);
await page.screenshot({ path: `${SHOTS}/r1_transcript_light.png` });
log('stored transcript (light)');

await page.click('.side-nav-btn:has-text("Settings")');
await page.waitForSelector('.settings .panel');
await sleep(300);
await page.screenshot({ path: `${SHOTS}/r1_settings_light.png` });
log('settings (light)');

await page.click('.side-nav-btn:has-text("About")');
await page.waitForSelector('.about .panel');
await sleep(300);
await page.screenshot({ path: `${SHOTS}/r1_about_light.png` });
log('about (light)');

// back to dark default + retention off again
await setTheme('dark');
await fetch(`${BASE}/api/v1/settings`, {
  method: 'PUT',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ retain_raw_audio: false }),
});

await browser.close();
log('done');
