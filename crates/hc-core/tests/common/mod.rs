#![allow(dead_code)]

use hc_core::action::{dev_key, key_address, CancelWire, OrderTypeWire, OrderWire, SigningKey};
use hc_core::clearinghouse::{AssetMeta, MarginTier};
use hc_core::exchange::Validator;
use hc_core::fixed::{Px, Usd};
use hc_core::types::{Address, Tif};
use hc_core::{Action, Genesis, Params, SignedTx};

pub const T0: u64 = 1_760_000_000_000;

pub fn user(name: &str) -> (SigningKey, Address) {
    let k = dev_key(name);
    let a = key_address(&k);
    (k, a)
}

pub fn validator_keys() -> Vec<SigningKey> {
    (0..4).map(|i| dev_key(&format!("validator-{i}"))).collect()
}

pub fn genesis() -> Genesis {
    let users = ["alice", "bob", "carol", "dave"];
    Genesis {
        time_ms: T0,
        assets: vec![
            AssetMeta {
                name: "BTC".into(),
                sz_decimals: 5,
                max_leverage: 40,
                margin_tiers: vec![
                    MarginTier { lower_bound: Usd::ZERO, max_leverage: 40 },
                    MarginTier { lower_bound: Usd::from_whole(150_000_000), max_leverage: 20 },
                ],
                impact_ntl: Usd::from_whole(20_000),
            },
            AssetMeta {
                name: "ETH".into(),
                sz_decimals: 4,
                max_leverage: 25,
                margin_tiers: vec![MarginTier { lower_bound: Usd::ZERO, max_leverage: 25 }],
                impact_ntl: Usd::from_whole(20_000),
            },
        ],
        oracle_pxs: vec![Px(100_000_000_000), Px(4_000_000_000)],
        validators: validator_keys().iter().map(|k| Validator { address: key_address(k), stake: 1 }).collect(),
        balances: users.iter().map(|u| (user(u).1, Usd::from_whole(1_000_000))).collect(),
        liquidator_balance: Usd::from_whole(10_000_000),
        params: Params::default(),
    }
}

pub fn order(asset: u32, is_buy: bool, px: &str, sz: &str, tif: Tif) -> OrderWire {
    OrderWire { a: asset, b: is_buy, p: px.into(), s: sz.into(), r: false, t: OrderTypeWire::limit(tif), c: None }
}

/// Signs with a nonce that is unique per call.
pub struct Signer {
    pub nonce: u64,
}

impl Signer {
    pub fn new() -> Signer {
        Signer { nonce: T0 }
    }
    pub fn tx(&mut self, key: &SigningKey, action: Action) -> SignedTx {
        self.nonce += 1;
        SignedTx::sign(key, action, self.nonce)
    }
    pub fn orders(&mut self, key: &SigningKey, orders: Vec<OrderWire>) -> SignedTx {
        self.tx(key, Action::order(orders))
    }
    /// An entry order followed by its TP/SL (`normalTpsl` grouping).
    pub fn orders_tpsl(&mut self, key: &SigningKey, orders: Vec<OrderWire>) -> SignedTx {
        self.tx(key, Action::order_with_tpsl(orders))
    }
    pub fn cancel(&mut self, key: &SigningKey, asset: u32, oid: u64) -> SignedTx {
        self.tx(key, Action::Cancel { cancels: vec![CancelWire { a: asset, o: oid }] })
    }
    pub fn oracle(&mut self, keys: &[SigningKey], pxs: &[&str]) -> Vec<SignedTx> {
        keys.iter().map(|k| self.tx(k, Action::SetOracle { pxs: pxs.iter().map(|s| s.to_string()).collect() })).collect()
    }
}
