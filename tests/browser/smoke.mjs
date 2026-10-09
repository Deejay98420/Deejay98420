// Browser smoke test: starts a DEMO server on a temporary book, drives the real UI, asserts that
// backend updates reach the page, and saves desktop + mobile screenshots.
//   cd tests/browser && npm ci && npx playwright install chromium && node smoke.mjs
// Env: CHROMIUM_PATH to use an existing Chromium, SHOT_DIR for screenshot output.
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const shots = process.env.SHOT_DIR || path.join(root, 'docs', 'screenshots');
mkdirSync(shots, { recursive: true });
const port = 8800 + Math.floor(Math.random() * 150);
const data = mkdtempSync(path.join(tmpdir(), 'pb-smoke-'));
const srv = spawn(process.env.PYTHON || 'python3', ['-m', 'pandabrain', 'serve', '--demo', '--port', String(port)], {
  cwd: root, env: { ...process.env, PANDABRAIN_DATA_DIR: data, PANDABRAIN_NO_OUTBOUND: '1' }, stdio: ['ignore', 'pipe', 'inherit'],
});
const fail = (m) => { console.error('SMOKE FAIL:', m); srv.kill('SIGTERM'); process.exit(1); };
await new Promise((res, rej) => { srv.stdout.on('data', d => { if (String(d).includes('http://')) res(); }); setTimeout(() => rej(new Error('server did not start')), 15000); }).catch(e => fail(e.message));

const url = `http://127.0.0.1:${port}/`;
const browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {});
const errors = [];
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  page.on('pageerror', e => errors.push(String(e)));
  page.on('console', m => { if (m.type() === 'error') errors.push(m.text()); });
  await page.goto(url);
  await page.waitForSelector('#seats .seat');
  const badge = await page.textContent('#modeBadge');
  if (!badge.includes('SHADOW')) fail('mode badge missing');
  if (await page.isHidden('#demoBadge')) fail('DEMO badge hidden in demo mode');
  if (!(await page.textContent('#engineBadge')).includes('RULES')) fail('engine badge');
  // before: no position
  if (!(await page.textContent('#tickerLine')).includes('NO POSITION')) fail('expected empty book at start');
  await page.click('#btnRun');
  await page.waitForFunction(() => document.querySelector('#tickerLine').textContent.includes('OPEN '), null, { timeout: 20000 });
  await page.waitForFunction(() => document.querySelector('#seats .seat.holding') !== null, null, { timeout: 10000 });
  const scanPill = await page.textContent('#seats .seat:nth-child(2) .pill');
  if (scanPill.trim() !== 'blocked') fail('SCAN should be blocked while holding, got ' + scanPill);
  await page.waitForFunction(() => document.querySelectorAll('#wlTable tbody tr').length >= 10, null, { timeout: 15000 });
  await page.waitForTimeout(2500);
  await page.screenshot({ path: path.join(shots, 'desktop.png'), fullPage: true });
  // drawer from a real row
  await page.click('#wlTable tbody tr:first-child');
  await page.waitForSelector('#drawer.open');
  await page.waitForTimeout(400);
  await page.screenshot({ path: path.join(shots, 'drawer.png') });
  await page.keyboard.press('Escape');
  // tabs
  await page.click('.tab[data-tab="book"]');
  await page.waitForSelector('#tab-book .card');
  await page.screenshot({ path: path.join(shots, 'book.png'), fullPage: false, clip: await page.locator('.tabs-panel').boundingBox() });
  await page.click('.tab[data-tab="log"]');
  await page.waitForSelector('#log .log-row');
  await page.click('.tab[data-tab="settings"]');
  await page.waitForSelector('#tab-settings .field');
  await page.click('.tab[data-tab="health"]');
  await page.waitForSelector('#tab-health .card');
  await page.screenshot({ path: path.join(shots, 'health.png'), clip: await page.locator('.tabs-panel').boundingBox() });

  // mobile
  const m = await browser.newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  m.on('pageerror', e => errors.push(String(e)));
  await m.goto(url);
  await m.waitForFunction(() => document.querySelector('#tickerLine').textContent.includes('OPEN '), null, { timeout: 15000 });
  await m.waitForTimeout(2500);
  const [sw, iw] = await m.evaluate(() => [document.documentElement.scrollWidth, window.innerWidth]);
  if (sw > iw) fail(`mobile horizontal overflow: ${sw} > ${iw}`);
  await m.screenshot({ path: path.join(shots, 'mobile.png'), fullPage: true });
  if (errors.length) fail('page errors: ' + errors.join(' | '));
  console.log('SMOKE OK — screenshots in', shots);
} finally {
  await browser.close();
  srv.kill('SIGTERM');
}
