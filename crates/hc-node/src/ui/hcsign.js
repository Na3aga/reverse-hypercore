// Browser/Node signer for hc-node core actions.
//
// Hyperliquid's L1 action signing, mirrored exactly by hc-core:
// 1. action_hash = keccak256(msgpack(action) || nonce_be64 || 0x00), where the
//    msgpack map keeps the action's field order and leaves out null fields,
//    and 0x00 means "no vault";
// 2. the key signs EIP-712 typed data Agent{source: "b", connectionId: action_hash}
//    in the domain {name: "Exchange", version: "1", chainId: 1337, verifyingContract: 0x0}.
// The signature is `r || s || v` (v = 27/28). The signer's address is its
// Ethereum address, the same account on the EVM side.

import { keccak256, toHex, hexToBytes, concat, hashTypedData } from 'viem';
import { privateKeyToAccount, sign } from 'viem/accounts';
import { blake3 } from '@noble/hashes/blake3';

const enc = new TextEncoder();

// ---- MessagePack, as Hyperliquid's SDKs pack actions --------------------------------
function be(n, bytes) {
  const out = new Uint8Array(bytes);
  let v = BigInt.asUintN(bytes * 8, BigInt(n));
  for (let i = bytes - 1; i >= 0; i--) { out[i] = Number(v & 0xffn); v >>= 8n; }
  return out;
}
function lenPrefix(n, fix, fixMax, [c8, c16, c32]) {
  if (n <= fixMax) return Uint8Array.of(fix | n);
  if (c8 && n <= 0xff) return Uint8Array.of(c8, n);
  if (n <= 0xffff) return concat([Uint8Array.of(c16), be(n, 2)]);
  return concat([Uint8Array.of(c32), be(n, 4)]);
}
function int(n) {
  const v = BigInt(n);
  if (v >= 0n) {
    if (v <= 0x7fn) return Uint8Array.of(Number(v));
    if (v <= 0xffn) return Uint8Array.of(0xcc, Number(v));
    if (v <= 0xffffn) return concat([Uint8Array.of(0xcd), be(v, 2)]);
    if (v <= 0xffffffffn) return concat([Uint8Array.of(0xce), be(v, 4)]);
    return concat([Uint8Array.of(0xcf), be(v, 8)]);
  }
  if (v >= -32n) return Uint8Array.of(Number(v) & 0xff);
  if (v >= -128n) return Uint8Array.of(0xd0, Number(v) & 0xff);
  if (v >= -32768n) return concat([Uint8Array.of(0xd1), be(v, 2)]);
  if (v >= -2147483648n) return concat([Uint8Array.of(0xd2), be(v, 4)]);
  return concat([Uint8Array.of(0xd3), be(v, 8)]);
}
export function msgpack(v) {
  if (v === null || v === undefined) return Uint8Array.of(0xc0);
  if (typeof v === 'boolean') return Uint8Array.of(v ? 0xc3 : 0xc2);
  if (typeof v === 'number' || typeof v === 'bigint') {
    if (typeof v === 'number' && !Number.isInteger(v)) throw new Error('floats are not used in actions');
    return int(v);
  }
  if (typeof v === 'string') { const b = enc.encode(v); return concat([lenPrefix(b.length, 0xa0, 31, [0xd9, 0xda, 0xdb]), b]); }
  if (Array.isArray(v)) return concat([lenPrefix(v.length, 0x90, 15, [0, 0xdc, 0xdd]), ...v.map(msgpack)]);
  const fields = Object.entries(v).filter(([, x]) => x !== null && x !== undefined);
  return concat([lenPrefix(fields.length, 0x80, 15, [0, 0xde, 0xdf]), ...fields.flatMap(([k, x]) => [msgpack(k), msgpack(x)])]);
}

// ---- Hashing and signing ---------------------------------------------------------------
// keccak256(msgpack(action) || nonce || vault || expiry): vault is 0x00, or
// 0x01 and the vault address; expiry is empty, or 0x00 and expiresAfter.
export function actionHash(action, nonce, vaultAddress, expiresAfter) {
  const vault = vaultAddress ? concat([Uint8Array.of(1), hexToBytes(vaultAddress)]) : Uint8Array.of(0);
  const expiry = expiresAfter != null ? concat([Uint8Array.of(0), be(expiresAfter, 8)]) : new Uint8Array();
  return keccak256(concat([msgpack(action), be(nonce, 8), vault, expiry]));
}

export const DOMAIN = { name: 'Exchange', version: '1', chainId: 1337, verifyingContract: '0x0000000000000000000000000000000000000000' };
export const AGENT_TYPES = { Agent: [{ name: 'source', type: 'string' }, { name: 'connectionId', type: 'bytes32' }] };

export function signingHash(action, nonce, vaultAddress, expiresAfter) {
  return hashTypedData({ domain: DOMAIN, types: AGENT_TYPES, primaryType: 'Agent', message: { source: 'b', connectionId: actionHash(action, nonce, vaultAddress, expiresAfter) } });
}

// The node's JSON field order for each action (it serializes them this way,
// and the hash covers that order).
function canonical(action) {
  const pick = (o, keys) => Object.fromEntries(keys.map((k) => [k, o[k] ?? null]));
  const orderType = (t) => t.limit ? { limit: { tif: t.limit.tif } } : { trigger: pick(t.trigger, ['isMarket', 'triggerPx', 'tpsl']) };
  switch (action.type) {
    case 'order': return { type: 'order', orders: action.orders.map((o) => ({ ...pick(o, ['a', 'b', 'p', 's', 'r']), t: orderType(o.t), c: o.c ?? null })), grouping: action.grouping ?? 'na' };
    case 'cancel': return { type: 'cancel', cancels: action.cancels.map((c) => pick(c, ['a', 'o'])) };
    case 'cancelByCloid': return { type: 'cancelByCloid', cancels: action.cancels.map((c) => pick(c, ['asset', 'cloid'])) };
    case 'updateLeverage': return pick(action, ['type', 'asset', 'isCross', 'leverage']);
    case 'updateIsolatedMargin': return pick(action, ['type', 'asset', 'isBuy', 'ntli']);
    case 'twapOrder': return { type: 'twapOrder', twap: pick(action.twap, ['a', 'b', 's', 'r', 'm', 't']) };
    case 'twapCancel': return pick(action, ['type', 'a', 't']);
    case 'evmUserModify': return pick(action, ['type', 'usingBigBlocks']);
    case 'approveAgent': return { ...pick(action, ['type', 'signatureChainId', 'hyperliquidChain', 'agentAddress']), agentName: action.agentName ?? '', nonce: action.nonce };
    case 'createVault': return pick(action, ['type', 'name', 'description', 'initialUsd', 'nonce']);
    case 'vaultTransfer': return pick(action, ['type', 'vaultAddress', 'isDeposit', 'usd']);
    case 'usdSend': return pick(action, ['type', 'signatureChainId', 'hyperliquidChain', 'destination', 'amount', 'time']);
    case 'faucet': case 'sendToEvm': return pick(action, ['type', 'usd']);
    case 'setOracle': return pick(action, ['type', 'pxs']);
    case 'setRiskLimits': return pick(action, ['type', 'maxPositionUsd', 'maxLeverage', 'dailyLossUsd', 'closeAtMarginPct']);
    default: throw new Error(`unknown action type ${action.type}`);
  }
}

// ---- User-signed actions (approveAgent, usdSend) --------------------------------------
// Their own fields are the EIP-712 message, in the HyperliquidSignTransaction
// domain on the chain the wallet is on (signatureChainId).
export const USER_SIGNED_TYPES = {
  approveAgent: {
    'HyperliquidTransaction:ApproveAgent': [
      { name: 'hyperliquidChain', type: 'string' }, { name: 'agentAddress', type: 'address' },
      { name: 'agentName', type: 'string' }, { name: 'nonce', type: 'uint64' },
    ],
  },
  usdSend: {
    'HyperliquidTransaction:UsdSend': [
      { name: 'hyperliquidChain', type: 'string' }, { name: 'destination', type: 'string' },
      { name: 'amount', type: 'string' }, { name: 'time', type: 'uint64' },
    ],
  },
};
/** The nonce field each user-signed action carries; it must equal the transaction nonce. */
export const USER_SIGNED_NONCE = { approveAgent: 'nonce', usdSend: 'time' };

/** EIP-712 typed data of a user-signed action, as a wallet signs it (uint64 fields as BigInt). */
export function userSignedTypedData(action) {
  const types = USER_SIGNED_TYPES[action.type];
  const [primaryType] = Object.keys(types);
  const message = Object.fromEntries(types[primaryType].map(({ name, type }) => [name, type === 'uint64' ? BigInt(action[name]) : action[name] ?? '']));
  return {
    domain: { name: 'HyperliquidSignTransaction', version: '1', chainId: parseInt(action.signatureChainId), verifyingContract: '0x0000000000000000000000000000000000000000' },
    types, primaryType, message,
  };
}

/** Signs a core action; returns the JSON body for /exchange or a WS post. */
// `opts.vaultAddress`: trade for a vault the key leads; `opts.expiresAfter`:
// reject after this time (ms). Both are signed.
export async function signAction(privateKey, action, nonce, opts = {}) {
  action = canonical(action);
  const userSigned = USER_SIGNED_TYPES[action.type];
  nonce ??= userSigned ? action[USER_SIGNED_NONCE[action.type]] : action.type === 'createVault' ? action.nonce : Date.now();
  const { vaultAddress = null, expiresAfter = null } = opts;
  const hash = userSigned ? hashTypedData(userSignedTypedData(action)) : signingHash(action, nonce, vaultAddress, expiresAfter);
  const sig = await sign({ hash, privateKey, to: 'object' });
  const v = Number(sig.v ?? 27n + BigInt(sig.yParity));
  const signature = concat([hexToBytes(sig.r, { size: 32 }), hexToBytes(sig.s, { size: 32 }), Uint8Array.of(v)]);
  return { action, nonce, signature: toHex(signature), vaultAddress, expiresAfter };
}

/** Deterministic devnet key: blake3("hc-dev-key:" + label), as in hc-core. */
export function devKey(label) {
  return toHex(blake3(enc.encode(`hc-dev-key:${label}`)));
}

export function addressOf(privateKey) {
  return privateKeyToAccount(privateKey).address.toLowerCase();
}
