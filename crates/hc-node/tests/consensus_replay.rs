//! End-to-end determinism: workload -> 4-validator simplex consensus
//! (deterministic runtime) -> execution on every validator -> identical state
//! hashes; and the same seed and workload -> the same chain and hashes again.

use hc_consensus::{run_deterministic, Finalized, PayloadSource, SimConfig};
use hc_core::action::{CancelWire, OrderTypeWire, OrderWire};
use hc_core::types::{Hash32, Tif};
use hc_core::{Action, SignedTx, State};
use hc_node::feeder::{oracle_tx, PriceWalk};
use hc_node::mempool::{Mempool, ValidatorSource};
use hc_node::node::execute;
use hc_node::{dev_user, devnet_genesis, validator_key, DEV_USERS, N_VALIDATORS};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const T0: u64 = 1_760_000_000_000;

/// Transactions arriving at given (virtual) times, gossiped to every mempool.
struct Scheduled {
    schedule: Arc<Mutex<VecDeque<(u64, SignedTx)>>>,
    mempools: Vec<Arc<Mempool>>,
    inner: ValidatorSource,
}

impl PayloadSource for Scheduled {
    fn build(&self, view: u64, parent: [u8; 32], now_ms: u64) -> Vec<u8> {
        let now = now_ms + self.inner.time_offset_ms;
        let mut s = self.schedule.lock().unwrap();
        while s.front().is_some_and(|(t, _)| *t <= now) {
            let (_, tx) = s.pop_front().unwrap();
            for m in &self.mempools {
                m.add(tx.clone());
            }
        }
        drop(s);
        self.inner.build(view, parent, now_ms)
    }
    fn validate(&self, view: u64, payload: &[u8]) -> bool {
        self.inner.validate(view, payload)
    }
}

fn workload(seed: u64, duration_ms: u64) -> VecDeque<(u64, SignedTx)> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let users: Vec<_> = DEV_USERS.iter().map(|u| dev_user(u).0).collect();
    let genesis = devnet_genesis(T0);
    let mut walk = PriceWalk::new(seed, &genesis.oracle_pxs, 10);
    let mut out = VecDeque::new();
    let mut nonce = T0;
    let mut t = T0;
    while t < T0 + duration_ms {
        t += rng.gen_range(20..120);
        nonce += 1;
        if rng.gen_ratio(1, 30) {
            walk.step();
            for v in 0..N_VALIDATORS {
                nonce += 1;
                out.push_back((t, oracle_tx(&validator_key(v), &walk.validator_prices(v), nonce)));
            }
            continue;
        }
        let k = &users[rng.gen_range(0..users.len())];
        let a = rng.gen_range(0..2u32);
        let mid = walk.pxs[a as usize] / 1_000_000;
        let action = match rng.gen_range(0..12) {
            0 => Action::Cancel { cancels: vec![CancelWire { a, o: rng.gen_range(1..300) }] },
            1 => Action::UpdateLeverage { asset: a, is_cross: true, leverage: rng.gen_range(1..=20) },
            _ => {
                let px = (mid + mid * rng.gen_range(-30..=30) / 10_000).max(1).to_string();
                let sz = if a == 0 { format!("0.{:03}", rng.gen_range(1..200)) } else { format!("{}.{}", rng.gen_range(0..3), rng.gen_range(1..9)) };
                let tif = [Tif::Gtc, Tif::Ioc, Tif::Alo][rng.gen_range(0..3)];
                Action::order(vec![OrderWire { a, b: rng.gen_bool(0.5), p: px, s: sz, r: false, t: OrderTypeWire::limit(tif), c: None }])
            }
        };
        out.push_back((t, SignedTx::sign(k, action, nonce)));
    }
    out
}

/// Runs consensus over the workload; returns each validator's finalized chain.
fn run_chain(seed: u64, n_blocks: u64, offline: Vec<usize>) -> Vec<Vec<Finalized>> {
    let schedule = Arc::new(Mutex::new(workload(seed, 60_000)));
    let mempools: Vec<Arc<Mempool>> = (0..N_VALIDATORS).map(|_| Mempool::new(500)).collect();
    let sources: Vec<Arc<dyn PayloadSource>> = mempools
        .iter()
        .map(|m| {
            Arc::new(Scheduled {
                schedule: schedule.clone(),
                mempools: mempools.clone(),
                inner: ValidatorSource { mempool: m.clone(), time_offset_ms: T0, max_payload_txs: 500, max_payload_bytes: hc_node::mempool::DEFAULT_MAX_BLOCK_BYTES, signers: Default::default(), checked: None },
            }) as Arc<dyn PayloadSource>
        })
        .collect();
    let cfg = SimConfig {
        seed,
        max_finalized: n_blocks,
        link_latency_ms: 30,
        link_jitter_ms: 10,
        packet_success_rate: 0.98,
        offline,
        block_interval_ms: 100,
        ..SimConfig::default()
    };
    run_deterministic(cfg, sources)
}

/// Executes a finalized chain from genesis; returns per-height state hashes.
fn execute_chain(chain: &[Finalized]) -> (Vec<Hash32>, State) {
    let mut state = State::genesis(&devnet_genesis(T0));
    let hashes = chain.iter().map(|f| execute(&mut state, f, None).1.state_hash).collect();
    (hashes, state)
}

#[test]
fn four_validators_agree_on_state_hash() {
    let chains = run_chain(11, 150, vec![]);
    assert!(chains.iter().all(|c| c.len() == 150), "every validator finalized 150 blocks");
    let (h0, s0) = execute_chain(&chains[0]);
    for c in &chains[1..] {
        assert_eq!(execute_chain(c).0, h0, "validators diverged");
    }
    // The chain carried real trading activity.
    let n_txs: usize = chains[0].iter().map(|f| hc_core::BlockBody::decode(&f.payload).unwrap().txs.len()).sum();
    assert!(n_txs > 200, "only {n_txs} txs finalized");
    assert!(s0.users.values().any(|u| !u.positions.is_empty()), "no positions opened");
    assert_ne!(s0.assets[0].prices.oracle_px, devnet_genesis(T0).oracle_pxs[0], "oracle never updated");
}

#[test]
fn same_seed_same_chain_same_hashes() {
    let a = run_chain(5, 80, vec![]);
    let b = run_chain(5, 80, vec![]);
    assert_eq!(a[0], b[0]);
    assert_eq!(execute_chain(&a[0]).0, execute_chain(&b[0]).0);
}

#[test]
fn one_validator_down_still_progresses_identically() {
    let chains = run_chain(3, 60, vec![1]);
    assert!(chains[1].is_empty());
    let (h0, _) = execute_chain(&chains[0]);
    assert_eq!(h0.len(), 60);
    assert_eq!(execute_chain(&chains[2]).0, h0);
    assert_eq!(execute_chain(&chains[3]).0, h0);
}
