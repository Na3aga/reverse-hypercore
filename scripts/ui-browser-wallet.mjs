// The browser-wallet flow: a stand-in for MetaMask (window.ethereum, signing
// in Node with a test key) approves an agent once, then orders are signed by
// the agent with no further wallet prompts and act for the wallet's account.
// Transfers (usdSend) are user-signed: the wallet signs each one.
// Usage: node scripts/ui-browser-wallet.mjs <node_modules with playwright-core and viem 2.21> [outdir]
import { createRequire } from 'node:module';
import { join } from 'node:path';

const require = createRequire(join(process.argv[2], 'x.js'));
const { chromium } = require('playwright-core');
const { privateKeyToAccount, generatePrivateKey } = require('viem/accounts');
const out = process.argv[3] ?? '.';
const URL = process.env.HC_URL ?? 'http://127.0.0.1:8080';
const info = async (body) => (await fetch(`${URL}/info`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })).json();

const wallet = privateKeyToAccount(generatePrivateKey()); // a fresh account nobody has used
const owner = wallet.address.toLowerCase();
const prompts = [];

const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
const fail = (msg) => { console.log('FAIL:', msg); errors.push(msg); };

// What MetaMask would do: return the account, the chain, and sign typed data.
await page.exposeFunction('walletRequest', async (method, params) => {
  prompts.push(method);
  if (method === 'eth_requestAccounts' || method === 'eth_accounts') return [owner];
  if (method === 'eth_chainId') return '0x1';
  if (method === 'eth_signTypedData_v4') {
    const { domain, types, primaryType, message } = JSON.parse(params[1]);
    delete types.EIP712Domain;
    const big = Object.fromEntries(types[primaryType].filter((f) => f.type === 'uint64').map((f) => [f.name, BigInt(message[f.name])]));
    return wallet.signTypedData({ domain, types, primaryType, message: { ...message, ...big } });
  }
  throw new Error(`unsupported ${method}`);
});
await page.addInitScript(() => {
  window.ethereum = { isMetaMask: true, request: ({ method, params }) => window.walletRequest(method, params) };
});

await page.goto(URL);
await page.waitForSelector('#markets button');
await page.waitForFunction(() => document.querySelector('#height').textContent.startsWith('#'));

// 1. Connect: one typed-data signature approves an agent for the new account.
await page.selectOption('#wallet', 'metamask');
await page.waitForFunction((o) => document.querySelector('#addr').title.startsWith(o), owner, { timeout: 20000 });
const acct = await info({ type: 'explorerAddress', user: owner });
console.log('connected:', await page.textContent('#addr'), '| agents:', acct.agents);
if (acct.agents.length !== 1 || acct.agents[0].name !== 'hc-web') fail('agent not approved');
const signs = () => prompts.filter((p) => p === 'eth_signTypedData_v4').length;
console.log('wallet signatures so far:', signs());
if (signs() !== 1) fail('connecting should take one signature');

// 2. Fund and trade: no more wallet prompts; the account (not the agent) holds the position.
await page.fill('#amt', '5000');
await page.click('#faucet');
await page.waitForTimeout(800);
await page.click('.seg.side button[data-v=buy]');
await page.click('.seg.kind button[data-v=market]');
await page.fill('#sz', '0.01');
await page.click('#submit');
const pos = await (async () => {
  for (let i = 0; i < 40; i++) {
    const p = (await info({ type: 'clearinghouseState', user: owner })).assetPositions[0]?.position;
    if (p) return p;
    await page.waitForTimeout(250);
  }
})();
console.log('position of the wallet account:', pos?.szi, pos?.coin);
if (pos?.szi !== '0.01') fail('the order did not act for the wallet account');
if (signs() !== 1) fail(`trading asked the wallet to sign (${signs()} signatures)`);
const agentAddr = acct.agents[0]?.address;
const agentState = await info({ type: 'clearinghouseState', user: agentAddr });
if (agentState.assetPositions.length) fail('the agent address holds a position');

// 3. Send USDC: a transfer is user-signed, so the wallet itself signs it (the agent may not).
const recipient = privateKeyToAccount(generatePrivateKey()).address.toLowerCase();
const balance = async (u) => +(await info({ type: 'clearinghouseState', user: u })).crossMarginSummary.accountValue;
await page.fill('#amt', '100');
await page.fill('#sendto', recipient);
await page.click('#sendusd');
for (let i = 0; i < 40 && (await balance(recipient)) !== 100; i++) await page.waitForTimeout(250);
console.log('recipient after wallet send:', await balance(recipient), '| wallet signatures:', signs());
if ((await balance(recipient)) !== 100) fail('usdSend from the browser wallet did not arrive');
if (signs() !== 2) fail(`the transfer should take exactly one more wallet signature, got ${signs()}`);

// 3b. Reload: the stored agent is reused, no new signature.
await page.reload();
await page.waitForSelector('#markets button');
await page.waitForFunction((o) => document.querySelector('#addr').title.startsWith(o), owner, { timeout: 20000 });
console.log('after reload, wallet signatures:', signs(), '| agents:', (await info({ type: 'explorerAddress', user: owner })).agents.length);
if (signs() !== 2) fail('reconnecting asked for another signature');

// 4. A dev wallet signs usdSend with its own key, no wallet involved.
await page.selectOption('#wallet', 'bob');
await page.waitForTimeout(1000);
await page.fill('#amt', '50');
await page.fill('#sendto', recipient);
await page.click('#sendusd');
for (let i = 0; i < 40 && (await balance(recipient)) !== 150; i++) await page.waitForTimeout(250);
console.log('recipient after bob sends 50:', await balance(recipient));
if ((await balance(recipient)) !== 150) fail('usdSend from a dev wallet did not arrive');
await page.screenshot({ path: join(out, 'ui-browser-wallet.png') });

console.log(errors.length ? `PAGE ERRORS:\n${errors.join('\n')}` : 'no page errors');
await browser.close();
process.exitCode = errors.length ? 1 : 0;
