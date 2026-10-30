// Vaults in the trading UI: alice creates a vault, bob deposits, alice
// chooses the vault in "Trade as" and buys for it (signed with vaultAddress).
// Usage: node scripts/ui-vaults.mjs <node_modules with playwright-core> [outdir]
import { createRequire } from 'node:module';
import { join } from 'node:path';

const require = createRequire(join(process.argv[2], 'x.js'));
const { chromium } = require('playwright-core');
const out = process.argv[3] ?? '.';
const URL = process.env.HC_URL ?? 'http://127.0.0.1:8080';
const info = async (body) => (await fetch(`${URL}/info`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })).json();
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
const fail = (msg) => { console.log('FAIL:', msg); errors.push(msg); };
async function until(f, what, ms = 20000) {
  for (const t0 = Date.now(); Date.now() - t0 < ms; await page.waitForTimeout(250)) { const v = await f(); if (v) return v; }
  fail(`timed out waiting for ${what}`);
}

await page.goto(URL);
await page.waitForSelector('#markets button');
await page.waitForFunction(() => document.querySelector('#height').textContent.startsWith('#'));
const name = `v${Date.now() % 100000}`;

// 1. Alice creates a vault with 1,000 USDC.
await page.selectOption('#wallet', 'alice');
await page.waitForTimeout(800);
await page.click('[data-tab=vaults]');
await page.fill('#vname', name);
await page.fill('#vinit', '1000');
await page.click('#vcreate');
const v = await until(async () => (await info({ type: 'vaults' })).find((x) => x.name === name), 'vault created');
console.log('vault', v?.vaultAddress, 'tvl', v?.tvl, 'leader', v?.leader);

// 2. Bob deposits 500.
await page.selectOption('#wallet', 'bob');
await page.waitForTimeout(800);
await page.click('[data-tab=vaults]');
await page.waitForSelector(`#vamt-${v.vaultAddress}`);
await page.fill(`#vamt-${v.vaultAddress}`, '500');
await page.click(`[data-vdep="${v.vaultAddress}"]`);
const v2 = await until(async () => { const x = (await info({ type: 'vaultDetails', vaultAddress: v.vaultAddress })); return x.followers.length === 2 && x; }, 'bob deposited');
console.log('after bob: tvl', v2?.tvl, 'followers', v2?.followers.map((f) => `${f.user.slice(0, 6)} ${f.vaultEquity}`));
// Withdrawing at once is refused: deposits are locked for a day.
await page.fill(`#vamt-${v.vaultAddress}`, '100');
await page.click(`[data-vwd="${v.vaultAddress}"]`);
await until(async () => (await page.$$eval('.toast', (ts) => ts.map((t) => t.textContent).join(' '))).includes('a day'), 'lockup refusal');

// 3. Alice trades as the vault: the position is the vault's, not hers.
await page.selectOption('#wallet', 'alice');
await page.waitForFunction(() => !document.querySelector('#tradeas').hidden, null, { timeout: 15000 });
await page.selectOption('#tradeas', v.vaultAddress);
await page.click('.seg.side button[data-v=buy]');
await page.click('.seg.kind button[data-v=market]');
await page.fill('#sz', '0.01');
await page.click('#submit');
const pos = await until(async () => (await info({ type: 'clearinghouseState', user: v.vaultAddress })).assetPositions[0]?.position, 'vault position');
console.log('vault position:', pos?.szi, pos?.coin);
await page.click('[data-tab=positions]');
await page.waitForTimeout(800);
const row = await page.$eval('#tabpanel', (el) => el.textContent.replace(/\s+/g, ' '));
if (!row.includes('0.01 BTC')) fail('positions tab does not show the vault position');
await page.screenshot({ path: join(out, 'ui-vaults.png') });
await page.selectOption('#tradeas', '');

console.log(errors.length ? `PAGE ERRORS:\n${errors.join('\n')}` : 'no page errors');
await browser.close();
process.exitCode = errors.length ? 1 : 0;
