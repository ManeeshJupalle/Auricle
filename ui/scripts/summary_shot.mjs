// Phase 6A self-verification: summarize a real stored session from the
// browser (template + provider pickers) and screenshot the rendered result.
// Usage: node scripts/summary_shot.mjs <shots dir>   (daemon must be running)

import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';

const SHOTS = process.argv[2];
mkdirSync(SHOTS, { recursive: true });

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
page.on('pageerror', (e) => console.log(`[pageerror] ${e.message}`));
await page.goto('http://127.0.0.1:4820');
await page.waitForSelector('.conn.open', { timeout: 10_000 });

const VIEW_ONLY = process.argv.includes('--view-only');

await page.click('.nav-btn:has-text("sessions")');
await page.waitForSelector('.session-card');
// The newest session with a real transcript (first card).
await page.click('.session-card >> nth=0');
await page.waitForSelector('.detail-actions select');

if (!VIEW_ONLY) {
  // Pick template + provider explicitly, then summarize.
  await page.selectOption('.detail-actions select >> nth=0', 'minutes');
  await page.selectOption('.detail-actions select >> nth=1', 'groq');
  await page.click('.detail-actions button:has-text("Summarize")');
  console.log('[shot] summarize clicked');
}

// Wait for the expected number of summary cards (pre-existing ones render
// immediately; a fresh summarize adds one more via the detail refetch).
const expected = VIEW_ONLY ? 1 : 2;
await page.waitForFunction(
  (n) => document.querySelectorAll('.summary').length >= n,
  expected,
  { timeout: 60_000 },
);
await page.waitForTimeout(400);
await page.screenshot({ path: `${SHOTS}/session_summary.png` });
console.log('[shot] summary rendered and captured');

const text = await page.textContent('.summary-body');
console.log(`[shot] summary text head: ${text.slice(0, 120).replace(/\n/g, ' ')}`);

await browser.close();

