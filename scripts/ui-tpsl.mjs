// Headless check of TP/SL and risk limits in the trading UI, against a running devnet.
// Usage: node scripts/ui-tpsl.mjs <node_modules dir with playwright-core> [outdir]
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

const toasts = () => page.$$eval('.toast', (ts) => ts.map((t) => t.textContent));
const notes = () => page.$$eval('.nitem', (ns) => ns.map((n) => n.textContent));
async function waitText(re, timeout = 20000) {
  await page.waitForFunction((src) => [...document.querySelectorAll('.toast, .nitem')].some((t) => new RegExp(src).test(t.textContent)), re.source, { timeout });
}

await page.goto(URL);
await page.waitForSelector('#markets button');
await page.selectOption('#wallet', 'carol');
await page.waitForFunction(() => document.querySelector('#height').textContent.startsWith('#'));
await page.waitForTimeout(1500);

// 1. Market buy with TP/SL 5 bps either side of the mid.
await page.click('.seg.side button[data-v=buy]');
await page.click('.seg.kind button[data-v=market]');
await page.fill('#sz', '0.05');
await page.check('#tpslon');
const mid = await page.evaluate(() => {
  const b = document.querySelector('#bids tr[data-px]')?.dataset.px, a = document.querySelector('#asks tr[data-px]:last-child')?.dataset.px;
  return (+a + +b) / 2;
});
await page.fill('#tppx', String(Math.round(mid * 1.0005)));
await page.fill('#slpx', String(Math.round(mid * 0.9995)));
console.log('mid', mid, '| hints:', await page.textContent('#tphint'), '/', await page.textContent('#slhint'));
await page.click('#submit');
await waitText(/with TP \+ SL: filled/);
console.log('order:', (await toasts()).find((t) => /TP \+ SL/.test(t)));

// 2. The two triggers are listed (unless one already fired) and drawn on the chart.
await page.click('[data-tab=orders]');
await page.waitForTimeout(800);
const rows = await page.$$eval('#tabpanel tbody tr', (rs) => rs.map((r) => r.textContent.replace(/\s+/g, ' ').trim()).filter((t) => /Take Profit|Stop/.test(t)));
console.log('trigger orders:', rows.length ? rows : 'none left (already fired)');
await page.screenshot({ path: join(out, 'ui-tpsl-orders.png') });

// 3. One fires as the price moves; the other is cancelled; the position closes.
await waitText(/(Take profit|Stop loss) sell 0\.05 BTC triggered/, 120000);
const fired = [...(await toasts()), ...(await notes())].find((t) => /triggered/.test(t));
console.log('fired:', fired.replace(/\s+/g, ' '));
await page.click('[data-tab=positions]');
await page.waitForFunction(() => !/BTC/.test(document.querySelector('#tabpanel').textContent), null, { timeout: 15000 });
await page.click('#bell');
console.log('bell:', (await notes()).slice(0, 4).map((n) => n.replace(/\s+/g, ' ')));
await page.keyboard.press('Escape');
await page.click('[data-tab=orders]');
await page.waitForTimeout(500);
const left = await page.$$eval('#tabpanel tbody tr', (rs) => rs.filter((r) => /Take Profit|Stop/.test(r.textContent)).length);
console.log('triggers left after one fired:', left);

// 4. Risk limits: a 5,000 USD position cap rejects a 0.1 BTC order.
await page.click('[data-tab=risk]');
await page.fill('#rk-pos', '5000');
await page.click('#rk-save');
await waitText(/Risk limits: done/);
await page.waitForTimeout(800);
await page.uncheck('#tpslon');
await page.fill('#sz', '0.1');
await page.click('#submit');
await waitText(/above your limit/);
console.log('risk reject:', (await toasts()).find((t) => /above your limit/.test(t)));
await page.screenshot({ path: join(out, 'ui-risk.png') });
await page.click('#rk-clear');
await waitText(/Remove risk limits: done/);

console.log(errors.length ? `PAGE ERRORS:\n${errors.join('\n')}` : 'no page errors');
await browser.close();
process.exitCode = errors.length || left ? 1 : 0;
