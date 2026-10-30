// Partial TP/SL on a position: a 50% take profit is added next to a
// whole-size one, a whole-size stop loss leaves both alone, and ✕ removes one.
// Usage: node scripts/ui-tpsl-partial.mjs <node_modules with playwright-core> [outdir]
import { createRequire } from 'node:module';
import { join } from 'node:path';

const require = createRequire(join(process.argv[2], 'x.js'));
const { chromium } = require('playwright-core');
const out = process.argv[3] ?? '.';
const URL = process.env.HC_URL ?? 'http://127.0.0.1:8080';
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
const fail = (msg) => { console.log('FAIL:', msg); errors.push(msg); };

await page.goto(URL);
await page.waitForSelector('#markets button');
await page.selectOption('#wallet', 'dave');
await page.waitForFunction(() => document.querySelector('#height').textContent.startsWith('#'));
await page.waitForTimeout(1500);

// Triggers dave has on BTC, as "tp 0.05" strings, from the API.
const dave = await page.getAttribute('#addr', 'title');
const mids = async () => (await fetch(`${URL}/info`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ type: 'allMids' }) })).json();
async function triggers() {
  const r = await fetch(`${URL}/info`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ type: 'explorerAddress', user: dave }) });
  const os = (await r.json()).openOrders;
  return os.filter((o) => o.coin === 'BTC' && o.isTrigger).map((o) => `${o.tpsl} ${o.sz}`).sort();
}
async function waitTriggers(want) {
  for (let i = 0; i < 40; i++) {
    const got = await triggers();
    if (JSON.stringify(got) === JSON.stringify(want)) return got;
    await page.waitForTimeout(250);
  }
  fail(`triggers ${JSON.stringify(await triggers())}, want ${JSON.stringify(want)}`);
}

// Start flat, then buy 0.1 BTC at market.
await page.click('[data-tab=positions]');
if (await page.$('[data-close=BTC]')) { await page.click('[data-close=BTC]'); await page.waitForTimeout(1500); }
await page.click('.seg.side button[data-v=buy]');
await page.click('.seg.kind button[data-v=market]');
await page.uncheck('#tpslon');
await page.fill('#sz', '0.1');
await page.click('#submit');
await page.waitForSelector('[data-tpsl=BTC]', { timeout: 20000 });
await page.click('[data-tpsl=BTC]');
await page.waitForSelector('#psz');
const mark = +(await mids()).BTC;
const full = await page.inputValue('#psz');
console.log('position', full, 'BTC at mark', mark);

// 1. 50% take profit 5% up: added as a partial level.
await page.click('[data-pct="50"]');
console.log('size field', await page.inputValue('#psz'), '| button:', await page.textContent('#psave'));
await page.fill('#ptp', String(Math.round(mark * 1.05)));
await page.click('#psave');
const half = (+full / 2).toFixed(5).replace(/0+$/, '');
console.log('after 50% TP:', await waitTriggers([`tp ${half}`]));

// 2. Whole-size take profit 10% up: the partial one stays.
await page.click('[data-pct="100"]');
await page.fill('#ptp', String(Math.round(mark * 1.1)));
await page.click('#psave');
console.log('after whole TP:', await waitTriggers([`tp ${full}`, `tp ${half}`].sort()));

// 3. Whole-size stop loss: both take profits stay.
await page.click('[data-tpsl=BTC]');
await page.waitForSelector('#psl');
await page.fill('#psl', String(Math.round(mark * 0.95)));
await page.click('#psave');
console.log('after whole SL:', await waitTriggers([`sl ${full}`, `tp ${full}`, `tp ${half}`].sort()));

// 4. The positions table lists all three; ✕ removes the partial take profit.
const cell = await page.$eval('#tabpanel table', (t) => {
  const col = [...t.querySelectorAll('thead th')].findIndex((th) => th.textContent.trim() === 'TP / SL');
  return t.querySelector('tbody tr').children[col].textContent.replace(/\s+/g, ' ').trim();
});
console.log('TP/SL cell:', cell);
if (!cell.includes(`× ${half}`)) fail('partial size not shown in the positions table');
await page.click('[data-tpsl=BTC]');
await page.waitForSelector('[data-rmtrig]');
await page.screenshot({ path: join(out, 'ui-tpsl-partial.png') });
const rm = await page.$$eval('[data-rmtrig]', (bs) => bs.map((b) => ({ oid: b.dataset.rmtrig, text: b.parentElement.textContent })));
await page.click(`[data-rmtrig="${rm.find((r) => r.text.includes(`× ${half}`)).oid}"]`);
console.log('after ✕:', await waitTriggers([`sl ${full}`, `tp ${full}`].sort()));

// Clean up: closing the position cancels what is left.
await page.click('#pcancel');
await page.click('[data-close=BTC]');
console.log('after close:', await waitTriggers([]));

console.log(errors.length ? `PAGE ERRORS:\n${errors.join('\n')}` : 'no page errors');
await browser.close();
process.exitCode = errors.length ? 1 : 0;
