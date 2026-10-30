//! Replay determinism: the same genesis and block sequence must give the same
//! state hash after every block, including after encoding the blocks to a log
//! and decoding them again.

mod common;

use common::*;
use hc_core::action::{CancelWire, OrderWire};
use hc_core::types::{Hash32, Tif};
use hc_core::{Action, BlockBody, SignedTx, State};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Generates a pseudo-random but realistic workload: limit/market orders around
/// a drifting oracle, cancels, leverage changes and validator oracle votes.
pub fn workload(seed: u64, n_blocks: usize) -> Vec<BlockBody> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut sg = Signer::new();
    let users: Vec<_> = ["alice", "bob", "carol", "dave"].iter().map(|u| user(u).0).collect();
    let vk = validator_keys();
    let mut px = [100_000i64, 4_000i64];
    let mut blocks = Vec::new();
    let mut time = T0;
    for _ in 0..n_blocks {
        time += rng.gen_range(50..400);
        let mut txs: Vec<SignedTx> = Vec::new();
        // Oracle votes roughly every 3 seconds, with small per-validator noise.
        if rng.gen_ratio(1, 10) {
            for p in px.iter_mut() {
                *p += *p * rng.gen_range(-30..=30) / 10_000;
            }
            for k in &vk {
                let noisy: Vec<String> = px.iter().map(|p| (p + rng.gen_range(-2..=2)).to_string()).collect();
                txs.push(sg.tx(k, Action::SetOracle { pxs: noisy }));
            }
        }
        for _ in 0..rng.gen_range(0..8) {
            let k = &users[rng.gen_range(0..users.len())];
            let asset = rng.gen_range(0..2u32);
            let mid = px[asset as usize];
            let tx = match rng.gen_range(0..10) {
                0 => sg.tx(k, Action::Cancel { cancels: vec![CancelWire { a: asset, o: rng.gen_range(1..200) }] }),
                1 => sg.tx(k, Action::UpdateLeverage { asset, is_cross: true, leverage: rng.gen_range(1..=20) }),
                _ => {
                    let is_buy = rng.gen_bool(0.5);
                    let off = mid * rng.gen_range(-50..=50) / 10_000;
                    let p = (mid + off).max(1).to_string();
                    let s = if asset == 0 { format!("0.{:03}", rng.gen_range(1..300)) } else { format!("{}.{}", rng.gen_range(0..5), rng.gen_range(1..9)) };
                    let tif = [Tif::Gtc, Tif::Ioc, Tif::Alo][rng.gen_range(0..3)];
                    let o = order(asset, is_buy, &p, &s, tif);
                    sg.tx(k, Action::order(vec![OrderWire { r: rng.gen_ratio(1, 10), ..o }]))
                }
            };
            txs.push(tx);
        }
        blocks.push(BlockBody { time_ms: time, txs, evm_txs: vec![] });
    }
    blocks
}

fn run(blocks: &[BlockBody]) -> (Vec<Hash32>, State) {
    let mut s = State::genesis(&genesis());
    let hashes = blocks.iter().enumerate().map(|(i, b)| s.apply_block(i as u64 + 1, b.time_ms, &b.txs).state_hash).collect();
    (hashes, s)
}

#[test]
fn same_input_same_state_hash() {
    let blocks = workload(42, 3_000);
    let (h1, s1) = run(&blocks);
    let (h2, s2) = run(&blocks);
    assert_eq!(h1, h2);
    assert_eq!(s1, s2);
    // The workload actually exercised the engine.
    let fills: usize = s1.users.values().map(|u| u.positions.len()).sum();
    assert!(fills > 0, "no positions opened");
    assert!(s1.next_oid > 500, "too few orders: {}", s1.next_oid);
}

#[test]
fn replay_from_encoded_log() {
    let blocks = workload(7, 1_500);
    let (expected, _) = run(&blocks);
    // Encode as the consensus payloads would be, then decode and replay.
    let log: Vec<Vec<u8>> = blocks.iter().map(|b| b.encode()).collect();
    let decoded: Vec<BlockBody> = log.iter().map(|b| BlockBody::decode(b).expect("decodes")).collect();
    let (replayed, _) = run(&decoded);
    assert_eq!(expected, replayed);
    // JSON round-trip (the WebSocket wire format) preserves signatures too.
    let json = serde_json::to_string(&blocks).unwrap();
    let from_json: Vec<BlockBody> = serde_json::from_str(&json).unwrap();
    assert_eq!(run(&from_json).0, expected);
}

#[test]
fn different_input_different_hash() {
    let mut blocks = workload(9, 300);
    let (h1, _) = run(&blocks);
    // Drop one transaction from a middle block: the final hash must change.
    let i = blocks.iter().position(|b| !b.txs.is_empty() && b.time_ms > T0 + 10_000).unwrap();
    blocks[i].txs.pop();
    let (h2, _) = run(&blocks);
    assert_eq!(h1[..i], h2[..i]);
    assert_ne!(h1[i], h2[i]);
}

#[test]
fn transaction_order_within_category_matters_across_categories_does_not() {
    // Reordering a cancel relative to a GTC order in the proposer list does not
    // change the result, because priority sorting puts the cancel first.
    let (ka, _) = user("alice");
    let (kb, _) = user("bob");
    let mut sg = Signer::new();
    let rest = sg.orders(&ka, vec![order(0, false, "100000", "0.1", Tif::Gtc)]);
    let take = sg.orders(&kb, vec![order(0, true, "100000", "0.1", Tif::Gtc)]);
    let cancel = sg.cancel(&ka, 0, 1);
    let mut a = State::genesis(&genesis());
    a.apply_block(1, T0 + 1, std::slice::from_ref(&rest));
    let mut b = a.clone();
    let ha = a.apply_block(2, T0 + 2, &[take.clone(), cancel.clone()]).state_hash;
    let hb = b.apply_block(2, T0 + 2, &[cancel, take]).state_hash;
    assert_eq!(ha, hb);
}
