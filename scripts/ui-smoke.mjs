// Headless UI smoke test against a running devnet, driving the real page:
// the chart fills with candles from the background market simulator, alice
// quotes, bob takes at market, bob bridges core->EVM->core, then the explorer
// shows the activity. Screenshots go to the output directory.
//
// Usage: node scripts/ui-smoke.mjs <node_modules dir with playwright-core> [outdir]
import { createRequire } from 'node:module';
import { join } from 'node:path';

const require = createRequire(join(process.argv[2], 'x.js'));
const { chromium } = require('playwright-core');
const out = process.argv[3] ?? '.';
const URL = process.env.HC_URL ?? 'http://127.0.0.1:8080';

const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
page.on('dialog', (d) => d.dismiss());

const lastToast = async () => (await page.locator('.toast').last().textContent()) ?? '';
async function waitToast(re, timeout = 15000) {
  await page.waitForFunction((src) => [...document.querySelectorAll('.toast')].some((t) => new RegExp(src).test(t.textContent)), re.source, { timeout });
  return lastToast();
}
async function wallet(name) { await page.selectOption('#wallet', name); await page.waitForTimeout(300); }
async function limit(side, px, sz, tif = 'Gtc') {
  await page.click(`.seg.side button[data-v=${side}]`);
  await page.click('.seg.kind button[data-v=limit]');
  await page.fill('#px', px); await page.fill('#sz', sz); await page.selectOption('#tif', tif);
  await page.click('#submit');
}

await page.goto(URL);
await page.waitForSelector('#markets button');
await wallet('alice');
await page.waitForFunction(() => document.querySelector('#height').textContent.startsWith('#'));
console.log('blocks streaming:', await page.textContent('#height'), await page.textContent('#blockrate'));
await page.waitForFunction(() => document.querySelector('#chartempty').hidden, null, { timeout: 15000 });
await page.waitForTimeout(3000);
console.log('chart:', (await page.textContent('#ohlc')).replace(/\s+/g, ' ').trim());
await page.click('[data-pane=trades]');
await page.waitForTimeout(1500);
console.log('trade rate:', await page.textContent('#tradecount'));
await page.screenshot({ path: join(out, 'ui-chart.png') });
await page.click('[data-pane=book]');

// The Mid button fills the price box from the live book.
await page.click('.seg.kind button[data-v=limit]');
await page.click('#pxmid');
const mid = Number(await page.inputValue('#px'));
console.log('mid button:', mid);
if (!(mid > 0)) throw new Error('Mid button left the price empty');
const at = (f) => String(Math.round(mid * f));
// Just above mid, so simulated takers fill it: the fills land in the bell, not as toasts.
await limit('sell', at(1.0002), '0.5');
console.log('alice limit:', await waitToast(/resting as #\d+|filled/));
const seen = await page.locator('.toast').count();
await limit('sell', at(1.004), '0.5', 'Alo');
await page.waitForFunction((n) => document.querySelectorAll('.toast').length > n || document.querySelectorAll('.toast').length === 3, seen, { timeout: 15000 });
console.log('alice post-only:', await lastToast());
await page.waitForFunction(() => !document.querySelector('#badge').hidden, null, { timeout: 30000 });
console.log('unread notifications:', await page.textContent('#badge'), '| toasts on screen:', await page.locator('.toast').count());
await page.waitForTimeout(1500);
await page.screenshot({ path: join(out, 'ui-markers.png') });
await page.click('#bell');
console.log('first notification:', (await page.textContent('.nitem'))?.replace(/\s+/g, ' ').trim());
await page.screenshot({ path: join(out, 'ui-notifications.png') });
await page.keyboard.press('Escape');
await page.waitForSelector('#asks tr[data-px]');

await wallet('bob');
await page.click('.seg.side button[data-v=buy]');
await page.click('.seg.kind button[data-v=market]');
await page.fill('#sz', '0.2');
await page.click('#submit');
console.log('bob market:', await waitToast(/filled 0.2 at/));

await page.fill('#amt', '250');
await page.click('#toevm');
console.log('bob to EVM:', await waitToast(/Move 250 USDC to EVM: done/));
await page.waitForFunction(() => document.querySelector('#evmbal').textContent.startsWith('250'), null, { timeout: 15000 });
await page.fill('#amt', '100');
await page.click('#tocore');
await waitToast(/Moving 100 USDC to core/);
await page.waitForFunction(() => document.querySelector('#evmbal').textContent.startsWith('150'), null, { timeout: 15000 });
console.log('bob EVM balance after moving 100 back:', await page.textContent('#evmbal'));

await page.waitForSelector('#tabpanel td');
console.log('bob position row:', (await page.textContent('#tabpanel tbody tr'))?.replace(/\s+/g, ' ').trim());
await page.screenshot({ path: join(out, 'ui-trade.png') });

await page.click('[data-tab=activity]');
await page.waitForTimeout(300);
await page.screenshot({ path: join(out, 'ui-activity.png') });

await page.goto(`${URL}/explorer`);
await page.waitForSelector('#blocks tr');
console.log('explorer supply check:', await page.locator('.stat', { hasText: 'Supply check' }).textContent());
await page.screenshot({ path: join(out, 'ui-explorer.png') });

console.log(errors.length ? `PAGE ERRORS:\n${errors.join('\n')}` : 'no page errors');
await browser.close();
process.exitCode = errors.length ? 1 : 0;
