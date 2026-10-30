// Isolated margin, TWAP orders and the big-EVM-blocks switch in the trading UI,
// against a running devnet.
// Usage: node scripts/ui-margin-twap.mjs <node_modules with playwright-core> [outdir]
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

const info = async (body) => (await fetch(`${URL}/info`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })).json();
async function until(f, what, ms = 20000) {
  for (const t0 = Date.now(); Date.now() - t0 < ms; await page.waitForTimeout(250)) { const v = await f(); if (v) return v; }
  fail(`timed out waiting for ${what}`);
}

await page.goto(URL);
await page.waitForSelector('#markets button');
await page.selectOption('#wallet', 'carol');
await page.waitForFunction(() => document.querySelector('#height').textContent.startsWith('#'));
await page.waitForTimeout(1500);
const me = await page.getAttribute('#addr', 'title');
const acct = () => info({ type: 'explorerAddress', user: me });
const btc = async () => (await acct()).core.assetPositions.find((p) => p.position.coin === 'BTC')?.position;

// Start flat on BTC.
await page.click('[data-tab=positions]');
if (await page.$('[data-close=BTC]')) { await page.click('[data-close=BTC]'); await until(async () => !(await btc()), 'flat'); }

// 1. Isolated 10x, then a market buy: the position holds its own margin.
await page.selectOption('#mmode', 'isolated');
await page.fill('#lev', '10');
await page.click('#setlev');
await until(async () => (await acct()).core.assetLeverage.find((l) => l.coin === 'BTC')?.type === 'isolated', 'isolated mode');
await page.click('.seg.side button[data-v=buy]');
await page.click('.seg.kind button[data-v=market]');
await page.fill('#sz', '0.05');
await page.click('#submit');
const p0 = await until(btc, 'isolated position');
console.log('position:', p0.szi, 'BTC', p0.leverage, '| margin', p0.marginUsed, '| liq px', p0.liquidationPx);
if (p0.leverage.type !== 'isolated') fail('position is not isolated');

// 2. Add 100 USDC of margin, then take 50 back: the liquidation price moves away, then back.
await page.waitForSelector('[data-margin=BTC]');
await page.click('[data-margin=BTC]');
await page.fill('#imamt', '100');
await page.click('#imadd');
const p1 = await until(async () => { const p = await btc(); return +p.leverage.rawUsd > +p0.leverage.rawUsd + 99 && p; }, 'margin added');
console.log('after +100: margin', p1.marginUsed, '| liq px', p1.liquidationPx);
if (!(+p1.liquidationPx < +p0.liquidationPx)) fail('liquidation price did not move away');
await page.fill('#imamt', '50');
await page.click('#imremove');
const p2 = await until(async () => { const p = await btc(); return +p.leverage.rawUsd < +p1.leverage.rawUsd - 49 && p; }, 'margin removed');
console.log('after -50: margin', p2.marginUsed, '| liq px', p2.liquidationPx);
await page.screenshot({ path: join(out, 'ui-isolated.png') });
const row = await page.$eval('#tabpanel tbody tr', (r) => r.textContent.replace(/\s+/g, ' '));
if (!row.includes('isolated')) fail('positions row does not say isolated');

// Close it and go back to cross.
await page.click('#imclose');
await page.click('[data-close=BTC]');
await until(async () => !(await btc()), 'position closed');
await page.selectOption('#mmode', 'cross');
await page.fill('#lev', '20');
await page.click('#setlev');
await until(async () => (await acct()).core.assetLeverage.find((l) => l.coin === 'BTC')?.type === 'cross', 'cross mode');

// 3. TWAP: buy 0.02 BTC over 1 minute (2 slices). The first slice fills after 30 s.
await page.click('.seg.kind button[data-v=twap]');
await page.fill('#sz', '0.02');
await page.fill('#twapm', '1');
console.log('ticket:', await page.textContent('#submit'), '|', await page.textContent('#twaphint'));
await page.click('#submit');
const t = await until(async () => (await acct()).twaps[0], 'TWAP running');
console.log('twap:', t.twapId, t.coin, t.side, t.sz, `${t.slicesDone}/${t.slices} slices`);
await page.click('[data-tab=twap]');
await page.waitForSelector('[data-twapcancel]');
const sliced = await until(async () => { const x = (await acct()).twaps[0]; return x && +x.executedSz > 0 && x; }, 'first slice', 45000);
console.log('after first slice:', sliced?.executedSz, 'at', sliced?.avgPx);
await page.waitForTimeout(500);
await page.screenshot({ path: join(out, 'ui-twap.png') });
await page.click('[data-twapcancel]');
await until(async () => (await acct()).twaps.length === 0, 'TWAP canceled');
console.log('canceled; position', (await btc())?.szi);
await page.click('[data-tab=positions]');
if (await page.$('[data-close=BTC]')) await page.click('[data-close=BTC]');

// 4. Big EVM blocks switch.
await page.check('#bigblocks');
await until(async () => (await acct()).evm.usingBigBlocks, 'big blocks on');
await page.uncheck('#bigblocks');
await until(async () => !(await acct()).evm.usingBigBlocks, 'big blocks off');
console.log('big blocks switch ok');

console.log(errors.length ? `PAGE ERRORS:\n${errors.join('\n')}` : 'no page errors');
await browser.close();
process.exitCode = errors.length ? 1 : 0;
