// Checks core-action signing three ways, on the Rust signing vectors:
// 1. hcsign.js (the browser signer) reproduces every vector byte for byte;
// 2. an independent Hyperliquid SDK (@nktkas/hyperliquid, `signL1Action` with
//    isTestnet) produces the same signature for every action;
// 3. the recovered EIP-712 signer is the vector's key.
// Usage: node scripts/check-js-signer.mjs <dir with node_modules: viem, @noble/hashes, @nktkas/hyperliquid>
import { readFileSync, copyFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const work = process.argv[2];
copyFileSync(join(root, 'crates/hc-node/src/ui/hcsign.js'), join(work, 'hcsign.mjs'));
const { signAction, devKey, addressOf, signingHash, approveAgentTypedData, APPROVE_AGENT_TYPES } = await import(pathToFileURL(join(work, 'hcsign.mjs')));
const { hashTypedData } = await import(pathToFileURL(join(work, 'node_modules/viem/_esm/index.js')));
const { signL1Action, signUserSignedAction } = await import(pathToFileURL(join(work, 'node_modules/@nktkas/hyperliquid/esm/signing/mod.js')));
const { privateKeyToAccount } = await import(pathToFileURL(join(work, 'node_modules/viem/_esm/accounts/index.js')));
const { recoverAddress } = await import(pathToFileURL(join(work, 'node_modules/viem/_esm/index.js')));

const vectors = JSON.parse(readFileSync(join(root, 'crates/hc-core/tests/fixtures/sign_vectors.json'), 'utf8'));
const pk = devKey('alice');
const wallet = privateKeyToAccount(pk);
// Hyperliquid clients leave optional fields out instead of sending null.
const dropNulls = (v) => Array.isArray(v) ? v.map(dropNulls)
  : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).filter(([, x]) => x !== null).map(([k, x]) => [k, dropNulls(x)])) : v;

let fails = 0;
for (const v of vectors) {
  const ours = await signAction(pk, v.action, v.nonce);
  // approveAgent is user-signed (its own EIP-712 message); everything else is an L1 action.
  const userSigned = v.action.type === 'approveAgent';
  const sdk = userSigned
    ? await signUserSignedAction({ wallet, action: v.action, types: APPROVE_AGENT_TYPES })
    : await signL1Action({ wallet, action: dropNulls(v.action), nonce: v.nonce, isTestnet: true });
  const sdkSig = `${sdk.r.slice(2).padStart(64, '0')}${sdk.s.slice(2).padStart(64, '0')}${sdk.v.toString(16)}`;
  const digest = userSigned ? hashTypedData(approveAgentTypedData(v.action)) : signingHash(v.action, v.nonce);
  const signer = (await recoverAddress({ hash: digest, signature: v.signature })).toLowerCase();
  const checks = {
    'hcsign.js == Rust': ours.signature === v.signature && JSON.stringify(ours.action) === JSON.stringify(v.action),
    'Hyperliquid SDK == Rust': `0x${sdkSig}` === v.signature,
    'signer == alice': signer === addressOf(pk),
  };
  const bad = Object.entries(checks).filter(([, ok]) => !ok).map(([k]) => k);
  if (bad.length) {
    fails++;
    console.error(`MISMATCH ${v.action.type}: ${bad.join(', ')}\n rust ${v.signature}\n js   ${ours.signature}\n sdk  0x${sdkSig}`);
  } else console.log(`ok  ${v.action.type}${v.action.grouping ? ` (${v.action.grouping})` : ''}`);
}
console.log(`${vectors.length - fails}/${vectors.length} vectors match all three ways; alice = ${addressOf(pk)}`);
process.exitCode = fails ? 1 : 0;
