//! `hc-node`: wires the execution engine (`hc-core`) to 4-validator BFT
//! consensus (`hc-consensus`, Commonware simplex), with per-validator mempools,
//! oracle feeders, a replayable block log, and a WebSocket/HTTP API.

pub mod api;
pub mod evm_rpc;
pub mod feeder;
pub mod gossip;
pub mod history;
pub mod mempool;
pub mod node;
pub mod price_feed;
pub mod sigcache;
pub mod sim;
pub mod views;

use hc_core::action::{dev_key, key_address, SigningKey};
use hc_core::clearinghouse::{AssetMeta, MarginTier};
use hc_core::exchange::Validator;
use hc_core::fixed::{Px, Usd};
use hc_core::types::Address;
use hc_core::{Genesis, Params};
use mempool::{Mempool, ValidatorSource};
use std::sync::Arc;

/// Default validator count: the smallest set that tolerates one fault (n ≥ 3f + 1).
pub const N_VALIDATORS: usize = 4;
pub const DEV_USERS: [&str; 4] = ["alice", "bob", "carol", "dave"];

/// Oracle (application-level) signing key of validator `i`.
pub fn validator_key(i: usize) -> SigningKey {
    dev_key(&format!("validator-{i}"))
}

pub fn dev_user(name: &str) -> (SigningKey, Address) {
    let k = dev_key(name);
    let a = key_address(&k);
    (k, a)
}

/// Devnet genesis: one collateral token (USDC), two perps (BTC, ETH), four
/// equally staked validators, and funded dev users.
pub fn devnet_genesis(time_ms: u64) -> Genesis {
    devnet_genesis_n(time_ms, N_VALIDATORS)
}

/// Devnet genesis with `n` equally staked validators.
pub fn devnet_genesis_n(time_ms: u64, n: usize) -> Genesis {
    Genesis {
        time_ms,
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
        validators: (0..n)
            .map(|i| Validator { address: key_address(&validator_key(i)), stake: 1 })
            .collect(),
        balances: DEV_USERS.iter().map(|u| (dev_user(u).1, Usd::from_whole(1_000_000))).collect(),
        liquidator_balance: Usd::from_whole(10_000_000),
        params: Params::default(),
    }
}

/// Payload sources for `n` in-process validators.
///
/// The validators share one mempool, one signer cache and one cache of
/// checked proposals. On real hardware every node does this per-transaction
/// work once, in parallel with the others; sharing it here makes one machine
/// carry about one node's worth of it instead of `n`. The consensus protocol
/// itself (votes, proposal broadcast to every peer, journals) still runs in
/// full for all `n` validators.
pub fn make_sources(
    n: usize,
    time_offset_ms: u64,
    max_txs: usize,
    max_bytes: usize,
    signers: Arc<sigcache::SignerCache>,
) -> (Vec<Arc<Mempool>>, Vec<Arc<dyn hc_consensus::PayloadSource>>) {
    let mempool = Mempool::with_limits(max_txs, max_bytes);
    let checked = Arc::new(mempool::CheckedPayloads::default());
    let sources = (0..n)
        .map(|_| {
            Arc::new(ValidatorSource {
                mempool: mempool.clone(),
                time_offset_ms,
                max_payload_txs: max_txs,
                max_payload_bytes: max_bytes,
                signers: signers.clone(),
                checked: Some(checked.clone()),
            }) as Arc<dyn hc_consensus::PayloadSource>
        })
        .collect();
    (vec![mempool], sources)
}
