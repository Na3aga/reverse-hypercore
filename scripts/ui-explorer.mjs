// Headless check of the explorer's transaction and block pages, against a running devnet.
// Usage: node scripts/ui-explorer.mjs <node_modules dir with playwright-core> [outdir]
import { createRequire } from 'node:module';
import { join } from 'node:path';

const require = createRequire(join(process.argv[2], 'x.js'));
const { chromium } = require('playwright-core');
const out = process.argv[3] ?? '.';
const URL = process.env.HC_URL ?? 'http://127.0.0.1:8080';
const info = async (b) => (await fetch(URL + '/info', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(b) })).json();

// A recent transaction with trades, and the busiest end-of-block.
const blocks = await info({ type: 'explorerBlocks', limit: 20 });
let tradeTx = null, eob = null;
for (const b of blocks.slice(1)) {
  const d = await info({ type: 'explorerBlock', height: b.height });
  if (!eob || (d.endOfBlock?.nEvents ?? 0) > eob.n) eob = { h: b.height, n: d.endOfBlock?.nEvents ?? 0 };
  for (const t of d.txs) {
    if (tradeTx || t.kind !== 'core' || t.action.type !== 'order') continue;
    const x = (await info({ type: 'explorerTx', hash: t.hash })).execution;
    if (x?.nTrades > 0 && x.nTrades < 40) tradeTx = t.hash;
  }
}
console.log('tx with trades:', tradeTx, '| block', eob);

const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1100 } });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));

await page.goto(`${URL}/explorer#/tx/${tradeTx}`);
await page.waitForSelector('#txtabs');
console.log('overview:', (await page.textContent('.kv')).replace(/\s+/g, ' ').slice(0, 400));
await page.screenshot({ path: join(out, 'explorer-tx.png'), fullPage: false });
for (const k of ['trades', 'changes', 'events']) {
  await page.click(`#txtabs button[data-k=${k}]`);
  const txt = await page.textContent(`#txtabs [data-p=${k}]`);
  console.log(`${k}:`, txt.replace(/\s+/g, ' ').slice(0, 260));
  await page.screenshot({ path: join(out, `explorer-tx-${k}.png`) });
}
await page.goto(`${URL}/explorer#/block/${eob.h}`);
await page.waitForSelector('#eobtabs');
console.log('end of block:', (await page.textContent('#eobtabs [data-p=events]')).replace(/\s+/g, ' ').slice(0, 300));
await page.screenshot({ path: join(out, 'explorer-block.png'), fullPage: true });

console.log(errors.length ? `PAGE ERRORS:\n${errors.join('\n')}` : 'no page errors');
await browser.close();
process.exitCode = errors.length || !tradeTx ? 1 : 0;
