// Phase 5 self-verification: drive the embedded UI end-to-end with real
// audio, capture screenshots, and prove reconnect recovery.
//
// Usage: node ui_drive.mjs <auricle.exe path> <shots dir> [--skip-reconnect]
// Assumes: daemon NOT running (script owns its lifecycle); DEEPGRAM_API_KEY
// available for the deepgram session; AURICLE_DRIVE_WAV pointing at a WAV
// with clear speech (generate one with your OS text-to-speech).

import { chromium } from 'playwright';
import { execFile, spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';

const EXE = process.argv[2];
const SHOTS = process.argv[3];
const SKIP_RECONNECT = process.argv.includes('--skip-reconnect');
const BASE = 'http://127.0.0.1:4820';
const TTS = process.env.AURICLE_DRIVE_WAV ?? `${process.env.TEMP}\\auricle_drive_tts.wav`;

mkdirSync(SHOTS, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function startDaemon() {
  const child = spawn(EXE, ['serve'], { stdio: 'ignore', detached: true });
  child.unref();
  return child;
}

async function waitHealthy() {
  for (let i = 0; i < 40; i++) {
    try {
      const r = await fetch(`${BASE}/api/v1/health`);
      if (r.ok) return;
    } catch {}
    await sleep(250);
  }
  throw new Error('daemon did not become healthy');
}

function killDaemon() {
  return new Promise((resolve) => {
    execFile('taskkill', ['/F', '/IM', 'auricle.exe'], () => resolve());
  });
}

function playTts() {
  const ps = spawn(
    'powershell',
    ['-NoProfile', '-c', `$p = New-Object Media.SoundPlayer "${TTS}"; $p.PlaySync()`],
    { stdio: 'ignore' },
  );
  return ps;
}

const log = (m) => console.log(`[drive] ${m}`);

startDaemon();
await waitHealthy();
log('daemon healthy (single binary, clean dir)');

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
page.on('pageerror', (e) => console.log(`[pageerror] ${e.message}`));
await page.goto(BASE);
await page.waitForSelector('.topbar');
await page.waitForSelector('.conn.open', { timeout: 10_000 });
log('UI loaded from embedded assets, WS connected');

// ---------- Settings ----------
await page.click('.nav-btn:has-text("settings")');
await page.waitForSelector('.settings .panel');
await sleep(400);
await page.screenshot({ path: `${SHOTS}/settings.png` });
log('settings screenshot');

// ---------- Sessions (pre-populated from earlier phases) ----------
await page.click('.nav-btn:has-text("sessions")');
await sleep(500);
await page.screenshot({ path: `${SHOTS}/sessions_list.png` });
log('sessions list screenshot');

// ---------- Live: full session from the browser ----------
await page.click('.nav-btn:has-text("live")');
await page.waitForSelector('select');
await page.selectOption('.live-controls select', 'deepgram');
const tts = playTts();
await sleep(700);
await page.click('.btn.start');
await page.waitForSelector('.rec', { timeout: 15_000 });
log('session started from the browser');

// Catch a partial row on screen, then screenshot immediately.
await page.waitForSelector('.t-row.partial', { timeout: 30_000 });
await page.screenshot({ path: `${SHOTS}/live_partial.png` });
log('live view with a visible partial captured');

// Let more of the transcript accumulate, take the general mid-session shot.
await sleep(14_000);
await page.screenshot({ path: `${SHOTS}/live_recording.png` });
log('mid-session screenshot');

// Stop from the browser.
await page.click('.btn.stop');
await page.waitForSelector('.btn.start', { timeout: 30_000 });
log('session stopped from the browser');
tts.kill();

// Sessions list should now include it; open the newest detail.
await page.click('.nav-btn:has-text("sessions")');
await page.waitForSelector('.session-card');
await sleep(400);
await page.screenshot({ path: `${SHOTS}/sessions_after.png` });
await page.click('.session-card >> nth=0');
await page.waitForSelector('.sessions-detail .t-row', { timeout: 10_000 });
await sleep(300);
await page.screenshot({ path: `${SHOTS}/session_detail.png` });
log('session detail screenshot');

// ---------- Reconnect: kill + restart under the open page ----------
if (!SKIP_RECONNECT) {
  await page.click('.nav-btn:has-text("live")');
  await killDaemon();
  await page.waitForSelector('.conn.reconnecting', { timeout: 15_000 });
  log('daemon killed: UI shows reconnecting');
  await page.screenshot({ path: `${SHOTS}/reconnecting.png` });
  startDaemon();
  await waitHealthy();
  await page.waitForSelector('.conn.open', { timeout: 20_000 });
  log('daemon restarted: UI reconnected without refresh');
  // Sessions still reachable through the same page after recovery.
  await page.click('.nav-btn:has-text("sessions")');
  await page.waitForSelector('.session-card', { timeout: 10_000 });
  log('post-reconnect data loads');
}

await browser.close();
await killDaemon();
log('done');
