//! Transactions: user and validator actions, their signatures, and the
//! in-block priority category used to order a block before execution.
//!
//! The JSON shapes follow the public exchange API
//! (`{"type":"order","orders":[{"a","b","p","s","r","t","c"}],"grouping":"na"}`).
//!
//! Signing is Hyperliquid's L1 action signing, as implemented by its public
//! SDKs:
//! 1. `action_hash = keccak256(msgpack(action) || nonce_be64 || 0x00)`, where
//!    the msgpack map keeps the JSON field order and leaves out null fields,
//!    and `0x00` says no vault is involved;
//! 2. the signer signs the EIP-712 struct `Agent(string source, bytes32
//!    connectionId)` with `connectionId = action_hash` and `source = "b"`
//!    (Hyperliquid's testnet value; mainnet is `"a"`), in the domain
//!    `{name: "Exchange", version: "1", chainId: 1337, verifyingContract: 0x0}`.
//!
//! So a wallet shows a typed-data request, and a Hyperliquid client library
//! pointed at this node signs valid transactions. The signer's address is
//! `keccak256(pubkey)[12..]`, the same address that key has on the EVM side,
//! so one key controls both the core account and the EVM account.

use crate::types::{Address, AssetId, Cloid, Hash32, Oid, Sig, Tif, Tpsl};
pub use k256::ecdsa::SigningKey;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

/// Ethereum address of a public key.
pub fn address_of(vk: &VerifyingKey) -> Address {
    let point = vk.to_encoded_point(false);
    let h = keccak256(&point.as_bytes()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

pub fn key_address(key: &SigningKey) -> Address {
    address_of(key.verifying_key())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitWire {
    pub tif: Tif,
}

/// A trigger (TP/SL) order: rests off the book until the mark price crosses
/// `trigger_px`, then executes as a market order (`is_market`) or as a limit
/// order at the order's price.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerWire {
    pub is_market: bool,
    pub trigger_px: String,
    pub tpsl: Tpsl,
}

/// `{"limit":{"tif":"Gtc"}}` or `{"trigger":{"isMarket":true,"triggerPx":"…","tpsl":"sl"}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderTypeWire {
    #[serde(rename = "limit")]
    Limit(LimitWire),
    #[serde(rename = "trigger")]
    Trigger(TriggerWire),
}

impl OrderTypeWire {
    pub fn limit(tif: Tif) -> OrderTypeWire {
        OrderTypeWire::Limit(LimitWire { tif })
    }

    /// Time in force of a limit order; `None` for trigger orders.
    pub fn tif(&self) -> Option<Tif> {
        match self {
            OrderTypeWire::Limit(l) => Some(l.tif),
            OrderTypeWire::Trigger(_) => None,
        }
    }
}

/// One order inside an `order` action. Prices and sizes are decimal strings,
/// exactly as on the public API, and are validated against the asset's
/// `sz_decimals` during execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderWire {
    /// Asset index.
    pub a: AssetId,
    /// Is buy.
    pub b: bool,
    /// Limit price.
    pub p: String,
    /// Size in coins.
    pub s: String,
    /// Reduce only.
    pub r: bool,
    /// Order type: limit (market = IOC at a slippage price) or trigger (TP/SL).
    pub t: OrderTypeWire,
    // No `skip_serializing_if`: the binary encoding is positional.
    #[serde(default)]
    pub c: Option<Cloid>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelWire {
    pub a: AssetId,
    pub o: Oid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelByCloidWire {
    pub asset: AssetId,
    pub cloid: Cloid,
}

/// How the orders of one `order` action relate: independent (`na`), an entry
/// followed by its TP/SL (`normalTpsl`, which wait for the entry to fill), or
/// TP/SL for an existing position (`positionTpsl`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Grouping {
    #[default]
    Na,
    NormalTpsl,
    PositionTpsl,
}

/// Declares [`Action`] once and derives two serde representations from it:
/// internally tagged (`{"type":"order",...}`, the public API shape) for
/// human-readable formats, and externally tagged for the binary encoding, since
/// postcard cannot decode internally tagged enums.
macro_rules! actions {
    ($( $(#[$vm:meta])* $variant:ident { $( $(#[$fm:meta])* $field:ident : $ty:ty ),* $(,)? } ),* $(,)?) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum Action {
            // Field attributes are serde attributes (and docs): they apply to the representations below.
            $( $(#[$vm])* $variant { $( $field: $ty ),* } ),*
        }

        mod repr {
            use super::*;

            #[derive(Serialize, Deserialize)]
            #[serde(tag = "type", rename_all = "camelCase")]
            pub enum Tagged {
                $( #[serde(rename_all = "camelCase")] $variant { $( $(#[$fm])* $field: $ty ),* } ),*
            }

            #[derive(Serialize, Deserialize)]
            #[serde(rename_all = "camelCase")]
            pub enum Plain {
                $( #[serde(rename_all = "camelCase")] $variant { $( $(#[$fm])* $field: $ty ),* } ),*
            }

            impl From<Action> for Tagged {
                fn from(a: Action) -> Tagged {
                    match a { $( Action::$variant { $($field),* } => Tagged::$variant { $($field),* } ),* }
                }
            }
            impl From<Tagged> for Action {
                fn from(a: Tagged) -> Action {
                    match a { $( Tagged::$variant { $($field),* } => Action::$variant { $($field),* } ),* }
                }
            }
            impl From<Action> for Plain {
                fn from(a: Action) -> Plain {
                    match a { $( Action::$variant { $($field),* } => Plain::$variant { $($field),* } ),* }
                }
            }
            impl From<Plain> for Action {
                fn from(a: Plain) -> Action {
                    match a { $( Plain::$variant { $($field),* } => Action::$variant { $($field),* } ),* }
                }
            }
        }

        impl Serialize for Action {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    repr::Tagged::from(self.clone()).serialize(s)
                } else {
                    repr::Plain::from(self.clone()).serialize(s)
                }
            }
        }

        impl<'de> Deserialize<'de> for Action {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Action, D::Error> {
                if d.is_human_readable() {
                    repr::Tagged::deserialize(d).map(Action::from)
                } else {
                    repr::Plain::deserialize(d).map(Action::from)
                }
            }
        }
    };
}

actions! {
    Order {
        orders: Vec<OrderWire>,
        #[serde(default)]
        grouping: Grouping,
    },
    Cancel {
        cancels: Vec<CancelWire>,
    },
    CancelByCloid {
        cancels: Vec<CancelByCloidWire>,
    },
    UpdateLeverage {
        asset: AssetId,
        is_cross: bool,
        leverage: u32,
    },
    /// Moves margin into an isolated position from the cross balance
    /// (`ntli` > 0, in µUSDC) or back out of it (`ntli` < 0).
    UpdateIsolatedMargin {
        asset: AssetId,
        is_buy: bool,
        ntli: i64,
    },
    /// A TWAP order: `{"type":"twapOrder","twap":{"a","b","s","r","m","t"}}`.
    TwapOrder {
        twap: crate::twap::TwapWire,
    },
    /// `{"type":"twapCancel","a":asset,"t":twapId}`.
    TwapCancel {
        a: AssetId,
        t: u64,
    },
    /// Sends the signer's EVM transactions to big blocks (one per minute,
    /// 30M gas) instead of small ones (one per second, 2M gas).
    EvmUserModify {
        using_big_blocks: bool,
    },
    /// Devnet faucet: credits the signer with USDC (bounded by genesis params).
    Faucet {
        usd: String,
    },
    /// Bridge into the EVM: moves USDC from the core balance to the signer's
    /// native EVM balance (same address).
    SendToEvm {
        usd: String,
    },
    /// Validator oracle vote: one price per asset (decimal strings), signed by a
    /// validator key. Aggregated with a stake-weighted median at end of block.
    SetOracle {
        pxs: Vec<String>,
    },
    /// The signer's own risk limits, enforced by the engine. Each field
    /// replaces the current setting; `null` removes that limit.
    SetRiskLimits {
        /// Largest position per market, in USD notional.
        max_position_usd: Option<String>,
        /// Highest leverage the account may use on any market.
        max_leverage: Option<u32>,
        /// Loss since 00:00 UTC at which only reducing orders are accepted.
        daily_loss_usd: Option<String>,
        /// Close all positions at market when maintenance margin reaches this
        /// percent of account value (liquidation happens at 100).
        close_at_margin_pct: Option<u32>,
    },
}

impl Action {
    /// An `order` action of independent orders.
    pub fn order(orders: Vec<OrderWire>) -> Action {
        Action::Order { orders, grouping: Grouping::Na }
    }

    /// An entry order followed by its TP/SL triggers.
    pub fn order_with_tpsl(orders: Vec<OrderWire>) -> Action {
        Action::Order { orders, grouping: Grouping::NormalTpsl }
    }
}

/// In-block priority category, from the public order-book documentation:
/// 0. actions that do not send GTC or IOC orders,
/// 1. cancels,
/// 2. actions that send at least one GTC or IOC order.
///
/// The sort is stable, so proposer order is kept within each category.
pub fn priority_category(action: &Action) -> u8 {
    match action {
        Action::Cancel { .. } | Action::CancelByCloid { .. } => 1,
        Action::Order { orders, .. } if orders.iter().any(|o| o.t.tif().is_some_and(|t| t != Tif::Alo)) => 2,
        _ => 0,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTx {
    pub action: Action,
    /// Millisecond timestamp nonce, unique per signer.
    pub nonce: u64,
    /// `r || s || v` with `v` in {27, 28}; the signer is recovered from it.
    pub signature: Sig,
}

/// MessagePack of a JSON value, as Hyperliquid's SDKs pack actions: maps
/// keep their field order and drop null fields, integers use their shortest
/// encoding, strings use str8 from 32 bytes.
pub fn msgpack(v: &serde_json::Value, out: &mut Vec<u8>) {
    use serde_json::Value;
    fn len_prefix(out: &mut Vec<u8>, n: usize, fix: u8, fix_max: usize, codes: [u8; 3], has8: bool) {
        if n <= fix_max {
            out.push(fix | n as u8);
        } else if has8 && n <= 0xff {
            out.extend_from_slice(&[codes[0], n as u8]);
        } else if n <= 0xffff {
            out.push(codes[1]);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(codes[2]);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
    match v {
        Value::Null => out.push(0xc0),
        Value::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                match u {
                    0..=0x7f => out.push(u as u8),
                    0x80..=0xff => out.extend_from_slice(&[0xcc, u as u8]),
                    0x100..=0xffff => {
                        out.push(0xcd);
                        out.extend_from_slice(&(u as u16).to_be_bytes());
                    }
                    0x1_0000..=0xffff_ffff => {
                        out.push(0xce);
                        out.extend_from_slice(&(u as u32).to_be_bytes());
                    }
                    _ => {
                        out.push(0xcf);
                        out.extend_from_slice(&u.to_be_bytes());
                    }
                }
            } else if let Some(i) = n.as_i64() {
                match i {
                    -32..=-1 => out.push(i as u8),
                    -128..=-33 => out.extend_from_slice(&[0xd0, i as u8]),
                    -32_768..=-129 => {
                        out.push(0xd1);
                        out.extend_from_slice(&(i as i16).to_be_bytes());
                    }
                    -2_147_483_648..=-32_769 => {
                        out.push(0xd2);
                        out.extend_from_slice(&(i as i32).to_be_bytes());
                    }
                    _ => {
                        out.push(0xd3);
                        out.extend_from_slice(&i.to_be_bytes());
                    }
                }
            } else {
                out.push(0xcb);
                out.extend_from_slice(&n.as_f64().unwrap_or_default().to_be_bytes());
            }
        }
        Value::String(s) => {
            len_prefix(out, s.len(), 0xa0, 31, [0xd9, 0xda, 0xdb], true);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(items) => {
            len_prefix(out, items.len(), 0x90, 15, [0, 0xdc, 0xdd], false);
            items.iter().for_each(|x| msgpack(x, out));
        }
        Value::Object(m) => {
            let fields: Vec<_> = m.iter().filter(|(_, x)| !x.is_null()).collect();
            len_prefix(out, fields.len(), 0x80, 15, [0, 0xde, 0xdf], false);
            for (k, x) in fields {
                msgpack(&Value::String(k.clone()), out);
                msgpack(x, out);
            }
        }
    }
}

/// Hyperliquid's action hash: `keccak256(msgpack(action) || nonce_be64 || 0x00)`
/// (the last byte: no vault).
pub fn action_hash(action: &Action, nonce: u64) -> [u8; 32] {
    let mut buf = Vec::with_capacity(256);
    msgpack(&serde_json::to_value(action).expect("action encodes"), &mut buf);
    buf.extend_from_slice(&nonce.to_be_bytes());
    buf.push(0);
    keccak256(&buf)
}

/// EIP-712 `source` of the phantom agent: `"a"` on Hyperliquid mainnet,
/// `"b"` on its testnet. This devnet uses the testnet value.
pub const AGENT_SOURCE: &str = "b";
/// Chain id in the EIP-712 domain of L1 actions (Hyperliquid uses 1337 on
/// every network; it is not the EVM chain id).
pub const SIGNING_CHAIN_ID: u64 = 1337;

/// The EIP-712 domain separator of `{name: "Exchange", version: "1",
/// chainId: 1337, verifyingContract: 0x0}`.
fn domain_separator() -> [u8; 32] {
    static DS: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *DS.get_or_init(|| {
        let mut b = Vec::with_capacity(160);
        b.extend_from_slice(&keccak256(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"));
        b.extend_from_slice(&keccak256(b"Exchange"));
        b.extend_from_slice(&keccak256(b"1"));
        let mut chain = [0u8; 32];
        chain[24..].copy_from_slice(&SIGNING_CHAIN_ID.to_be_bytes());
        b.extend_from_slice(&chain);
        b.extend_from_slice(&[0u8; 32]);
        keccak256(&b)
    })
}

/// The digest the signer signs: EIP-712 typed data of the phantom agent
/// `Agent{source, connectionId: action_hash}`.
pub fn signing_hash(action: &Action, nonce: u64) -> [u8; 32] {
    let mut agent = Vec::with_capacity(96);
    agent.extend_from_slice(&keccak256(b"Agent(string source,bytes32 connectionId)"));
    agent.extend_from_slice(&keccak256(AGENT_SOURCE.as_bytes()));
    agent.extend_from_slice(&action_hash(action, nonce));
    let mut msg = Vec::with_capacity(66);
    msg.extend_from_slice(&[0x19, 0x01]);
    msg.extend_from_slice(&domain_separator());
    msg.extend_from_slice(&keccak256(&agent));
    keccak256(&msg)
}

impl SignedTx {
    pub fn sign(key: &SigningKey, action: Action, nonce: u64) -> SignedTx {
        let msg = signing_hash(&action, nonce);
        let (sig, rid) = key.sign_prehash_recoverable(&msg).expect("signing succeeds");
        let mut bytes = [0u8; 65];
        bytes[..64].copy_from_slice(&sig.to_bytes());
        bytes[64] = 27 + rid.to_byte();
        SignedTx { action, nonce, signature: Sig(bytes) }
    }

    /// Recovers the signer's address; `None` if the signature is invalid.
    /// Uses libsecp256k1. Only low-S signatures are accepted (EIP-2), so a
    /// transaction has exactly one valid encoding.
    pub fn signer(&self) -> Option<Address> {
        use secp256k1::ecdsa::{RecoverableSignature, RecoveryId as SecpRecoveryId, Signature as SecpSignature};
        let bytes = &self.signature.0;
        let rid = SecpRecoveryId::try_from(bytes[64].checked_sub(27)? as i32).ok()?;
        let plain = SecpSignature::from_compact(&bytes[..64]).ok()?;
        let mut low = plain;
        low.normalize_s();
        if low != plain {
            return None;
        }
        let sig = RecoverableSignature::from_compact(&bytes[..64], rid).ok()?;
        let msg = secp256k1::Message::from_digest(signing_hash(&self.action, self.nonce));
        let pk = secp256k1::SECP256K1.recover_ecdsa(&msg, &sig).ok()?;
        let h = keccak256(&pk.serialize_uncompressed()[1..]);
        let mut a = [0u8; 20];
        a.copy_from_slice(&h[12..]);
        Some(Address(a))
    }

    /// The previous pure-Rust (k256) recovery, kept to cross-check the fast path.
    pub fn signer_k256(&self) -> Option<Address> {
        let sig = Signature::from_slice(&self.signature.0[..64]).ok()?;
        let rid = RecoveryId::from_byte(self.signature.0[64].checked_sub(27)?)?;
        let vk = VerifyingKey::recover_from_prehash(&signing_hash(&self.action, self.nonce), &sig, rid).ok()?;
        Some(address_of(&vk))
    }

    pub fn verify(&self) -> bool {
        self.signer().is_some()
    }

    /// Transaction id: hash of the full signed encoding.
    pub fn hash(&self) -> Hash32 {
        Hash32(keccak256(&postcard::to_stdvec(self).expect("tx encodes")))
    }
}

/// Recovers every signer in parallel. The output is in input order, so it is
/// deterministic whatever the thread count.
pub fn recover_signers(txs: &[SignedTx]) -> Vec<Option<Address>> {
    use rayon::prelude::*;
    txs.par_iter().with_min_len(64).map(SignedTx::signer).collect()
}

/// Deterministic test/dev key from a seed label.
pub fn dev_key(label: &str) -> SigningKey {
    SigningKey::from_slice(blake3::hash(format!("hc-dev-key:{label}").as_bytes()).as_bytes()).expect("valid scalar")
}

/// The payload a proposer puts into a consensus block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockBody {
    /// Proposer's wall-clock time in ms. Execution clamps it to be monotonic.
    pub time_ms: u64,
    /// Transactions in proposer order (before priority sorting).
    pub txs: Vec<SignedTx>,
    /// Raw signed Ethereum transactions (EIP-2718 bytes) for this block's EVM block.
    pub evm_txs: Vec<RawEvmTx>,
}

/// Raw Ethereum transaction bytes; hex in JSON.
#[derive(Clone, PartialEq, Eq)]
pub struct RawEvmTx(pub Vec<u8>);

impl std::fmt::Debug for RawEvmTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{}", crate::types::hex_encode(&self.0))
    }
}

impl Serialize for RawEvmTx {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&format!("0x{}", crate::types::hex_encode(&self.0)))
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for RawEvmTx {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<RawEvmTx, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            crate::types::hex_decode(s.strip_prefix("0x").unwrap_or(&s)).map(RawEvmTx).ok_or_else(|| serde::de::Error::custom("invalid hex"))
        } else {
            Ok(RawEvmTx(serde_bytes_buf(d)?))
        }
    }
}

fn serde_bytes_buf<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bytes")
        }
        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }
        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }
    }
    d.deserialize_byte_buf(V)
}

impl BlockBody {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("block encodes")
    }
    pub fn decode(bytes: &[u8]) -> Option<BlockBody> {
        postcard::from_bytes(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(tif: Tif) -> OrderWire {
        OrderWire { a: 0, b: true, p: "100".into(), s: "1".into(), r: false, t: OrderTypeWire::limit(tif), c: None }
    }

    #[test]
    fn categories_follow_public_rule() {
        assert_eq!(priority_category(&Action::order(vec![order(Tif::Alo)])), 0);
        assert_eq!(priority_category(&Action::Cancel { cancels: vec![] }), 1);
        assert_eq!(priority_category(&Action::order(vec![order(Tif::Alo), order(Tif::Gtc)])), 2);
        assert_eq!(priority_category(&Action::order(vec![order(Tif::Ioc)])), 2);
        assert_eq!(priority_category(&Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 3 }), 0);
    }

    #[test]
    fn sign_verify_and_json_shape() {
        let k = dev_key("alice");
        let tx = SignedTx::sign(&k, Action::order(vec![order(Tif::Gtc)]), 7);
        assert!(tx.verify());
        let json = serde_json::to_string(&tx).unwrap();
        assert!(json.contains(r#""type":"order""#), "{json}");
        assert!(json.contains(r#""t":{"limit":{"tif":"Gtc"}}"#), "{json}");
        let back: SignedTx = serde_json::from_str(&json).unwrap();
        assert!(back.verify());
        assert_eq!(tx.signer(), Some(key_address(&k)));
        // A different nonce recovers a different (wrong) signer.
        let mut bad = tx.clone();
        bad.nonce = 8;
        assert_ne!(bad.signer(), Some(key_address(&k)));
    }
}
