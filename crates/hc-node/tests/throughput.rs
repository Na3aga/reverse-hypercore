//! Engine throughput breakdown (run explicitly):
//!   cargo test --release -p hc-node --test throughput -- --ignored --nocapture
//!
//! Pre-signs blocks of order flow from many accounts, then times signature
//! recovery (old k256 vs libsecp256k1, one core vs all cores), execution with
//! known signers, and the node path where submission fills a signer cache.

use hc_core::action::{dev_key, key_address, CancelWire, OrderTypeWire, OrderWire, SigningKey};
use hc_core::fixed::Usd;
use hc_core::types::{Address, Tif};
use hc_core::action::recover_signers;
use hc_core::{Action, SignedTx, State};
use hc_node::sigcache::SignerCache;
use rayon::prelude::*;
use hc_node::devnet_genesis;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::time::{Duration, Instant};

const T0: u64 = 1_760_000_000_000;
const USERS: usize = 2_000;
const BLOCKS: u64 = 20;
const TXS_PER_BLOCK: usize = 10_000;

fn order(a: u32, b: bool, px: u64, sz: &str, tif: Tif) -> OrderWire {
    OrderWire { a, b, p: px.to_string(), s: sz.into(), r: false, t: OrderTypeWire::limit(tif), c: None }
}

#[test]
#[ignore]
fn throughput_breakdown() {
    let keys: Vec<SigningKey> = (0..USERS).map(|i| dev_key(&format!("bench-{i}"))).collect();
    let addrs: Vec<Address> = keys.iter().map(key_address).collect();
    let mut g = devnet_genesis(T0);
    g.balances.extend(addrs.iter().map(|a| (*a, Usd::from_whole(10_000_000))));
    let st = State::genesis(&g);

    // Order flow: 60% resting limits within ±20 ticks of 100,000, 30% crossing
    // IOCs, 10% cancels of the user's oldest order. One action per transaction.
    // Resting orders land on 1..=TICKS ticks either side of 100,000 (env, default 20).
    let spread: u64 = std::env::var("TICKS").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    let mut rng = ChaCha8Rng::seed_from_u64(1);
    let mut nonce = vec![0u64; USERS];
    let t = Instant::now();
    let mut blocks: Vec<Vec<SignedTx>> = Vec::new();
    for _ in 0..BLOCKS {
        let txs = (0..TXS_PER_BLOCK)
            .map(|_| {
                let u = rng.gen_range(0..USERS);
                nonce[u] += 1;
                let r: f64 = rng.gen();
                let action = if r < 0.6 {
                    let buy = rng.gen_bool(0.5);
                    let off = rng.gen_range(1..=spread);
                    let px = if buy { 100_000 - off } else { 100_000 + off };
                    Action::order(vec![order(0, buy, px, "0.01", Tif::Gtc)])
                } else if r < 0.9 {
                    let buy = rng.gen_bool(0.5);
                    Action::order(vec![order(0, buy, if buy { 100_000 + spread + 10 } else { 100_000 - spread - 10 }, "0.02", Tif::Ioc)])
                } else {
                    // Oids are global and increasing; a stale one just returns an error.
                    Action::Cancel { cancels: vec![CancelWire { a: 0, o: rng.gen_range(1..=(st.next_oid + 5_000)) }] }
                };
                SignedTx::sign(&keys[u], action, T0 + nonce[u])
            })
            .collect();
        blocks.push(txs);
    }
    println!("signed {} txs in {:?} (client side, not counted)", BLOCKS as usize * TXS_PER_BLOCK, t.elapsed());

    let n = (BLOCKS as usize * TXS_PER_BLOCK) as f64;
    let per = |d: Duration| d.as_secs_f64() * 1e6 / n;
    let rate = |d: Duration| n / d.as_secs_f64();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());

    // 1. Signature recovery alone.
    let sample: Vec<&SignedTx> = blocks.iter().flatten().take(20_000).collect();
    let t = Instant::now();
    sample.iter().for_each(|tx| assert!(tx.signer_k256().is_some()));
    let k256 = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
    let t = Instant::now();
    sample.iter().for_each(|tx| assert!(tx.signer().is_some()));
    let secp = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
    let t = Instant::now();
    let signers: Vec<Vec<Option<Address>>> = blocks.iter().map(|b| recover_signers(b)).collect();
    let par = t.elapsed();
    println!("recovery, one core:  k256 {k256:.1} µs/tx ({:.0} tx/s) | libsecp256k1 {secp:.1} µs/tx ({:.0} tx/s)", 1e6 / k256, 1e6 / secp);
    println!("recovery, parallel on {cores} cores (libsecp256k1): {:.2} µs/tx => {:.0} tx/s", per(par), rate(par));

    // 2. Execution with signers already known (cache hits): matching, margin,
    //    responses, and one state hash per block.
    let mut exec_state = st.clone();
    let (mut exec, mut hash, mut fills) = (Duration::ZERO, Duration::ZERO, 0usize);
    for (i, (txs, sg)) in blocks.iter().zip(&signers).enumerate() {
        let h = i as u64 + 1;
        let t = Instant::now();
        let out = exec_state.apply_block_with_signers(h, T0 + h * 100, txs, sg, &[]);
        exec += t.elapsed();
        fills += out.events.iter().filter(|e| matches!(e, hc_core::events::Event::Fill(f) if f.crossed)).count();
        let t = Instant::now();
        std::hint::black_box(exec_state.hash());
        hash += t.elapsed();
    }
    let state_bytes = postcard::to_stdvec(&exec_state).unwrap().len();
    println!(
        "txs: {n}, taker fills: {fills}, resting orders: {}, state: {:.1} MB",
        exec_state.books[0].orders.len(),
        state_bytes as f64 / 1e6
    );
    println!(
        "execution (signers cached): {:.2} µs/tx => {:.0} tx/s (state hash {:.1} ms/block included)",
        per(exec),
        rate(exec),
        hash.as_secs_f64() * 1e3 / BLOCKS as f64
    );

    // 3. Whole block from scratch: parallel recovery inside apply_block.
    let mut full_state = st.clone();
    let t = Instant::now();
    for (i, txs) in blocks.iter().enumerate() {
        let h = i as u64 + 1;
        std::hint::black_box(full_state.apply_block(h, T0 + h * 100, txs));
    }
    let full = t.elapsed();
    assert_eq!(full_state.hash(), exec_state.hash(), "cached and recovered paths diverged");
    println!("apply_block, no cache (parallel recovery + execution): {:.2} µs/tx => {:.0} tx/s", per(full), rate(full));

    // 4. Node path: submission fills the cache, execution reads it.
    let cache = SignerCache::default();
    let t = Instant::now();
    blocks.iter().flatten().collect::<Vec<_>>().par_iter().for_each(|tx| assert!(cache.signer(tx).is_some()));
    let submit = t.elapsed();
    let mut node_state = st.clone();
    let t = Instant::now();
    for (i, txs) in blocks.iter().enumerate() {
        let h = i as u64 + 1;
        let sg = cache.signers(txs);
        std::hint::black_box(node_state.apply_block_with_signers(h, T0 + h * 100, txs, &sg, &[]));
    }
    let node = t.elapsed();
    assert_eq!(node_state.hash(), exec_state.hash());
    println!("node path: submission (parallel, fills cache) {:.0} tx/s | executor (cache lookups + execution) {:.0} tx/s", rate(submit), rate(node));
}
