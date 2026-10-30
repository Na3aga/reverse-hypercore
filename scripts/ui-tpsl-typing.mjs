// Typing into TP/SL fields while account updates stream in must not lose
// focus or the typed text. Usage: node scripts/ui-tpsl-typing.mjs <node_modules with playwright-core>
import { createRequire } from 'node:module';
import { join } from 'node:path';

const require = createRequire(join(process.argv[2], 'x.js'));
const { chromium } = require('playwright-core');
const URL = process.env.HC_URL ?? 'http://127.0.0.1:8080';
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));

await page.goto(URL);
await page.waitForSelector('#markets button');
await page.selectOption('#wallet', 'carol');
await page.waitForFunction(() => document.querySelector('#height').textContent.startsWith('#'));
await page.waitForTimeout(1500);

// Slow typing: ~3 s per field, across many account refreshes.
async function slowType(sel, text) {
  await page.click(sel);
  for (const ch of text) { await page.keyboard.type(ch); await page.waitForTimeout(250); }
  return page.evaluate((s) => ({ value: document.querySelector(s).value, focused: document.activeElement === document.querySelector(s) }), sel);
}

// 1. Order ticket TP/SL.
await page.check('#tpslon');
console.log('ticket TP:', await slowType('#tppx', '123456'));
console.log('ticket SL:', await slowType('#slpx', '654321'));
await page.uncheck('#tpslon');

// 2. Position editor: open a small position, then edit its TP/SL.
await page.click('.seg.side button[data-v=buy]');
await page.click('.seg.kind button[data-v=market]');
await page.fill('#sz', '0.01');
await page.click('#submit');
await page.click('[data-tab=positions]');
await page.waitForSelector('[data-tpsl=BTC]', { timeout: 20000 });
await page.click('[data-tpsl=BTC]');
await page.waitForSelector('#ptp');
const tp = await slowType('#ptp', '999999');
const sl = await slowType('#psl', '1111');
console.log('position TP:', tp, '| position SL:', sl);
// Clean up: close the position.
await page.click('#pcancel');
await page.click('[data-close=BTC]');
await page.waitForTimeout(1500);

const ok = tp.value === '999999' && sl.value === '1111' && tp.focused && sl.focused;
console.log(ok ? 'typing kept' : 'TYPING LOST', errors.length ? `PAGE ERRORS: ${errors}` : 'no page errors');
await browser.close();
process.exitCode = ok && !errors.length ? 0 : 1;
