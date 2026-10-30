//! EVM transactions inside core blocks: the small-block (1 s, 2M gas) and
//! big-block (1 min, 30M gas) queues, the CoreWriter order delay, and the
//! event spans the explorer uses to attribute events to transactions.

mod common;

use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address as EvmAddress, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use common::*;
use hc_core::events::{Event, OrderResponse};
use hc_core::types::Hash32;
use hc_core::{Action, BlockOutput, State};

/// Genesis time rounded to a minute, so EVM block slots are easy to reason about.
const T: u64 = (T0 / 60_000 + 1) * 60_000;

fn evm_key(name: &str) -> PrivateKeySigner {
    PrivateKeySigner::from_slice(&user(name).0.to_bytes()).unwrap()
}

/// A gas-price-0 legacy transaction calling `to` with `input`.
fn evm_tx(key: &PrivateKeySigner, nonce: u64, to: [u8; 20], input: Vec<u8>, gas_limit: u64) -> Vec<u8> {
    let tx = TxLegacy {
        chain_id: Some(hc_evm::CHAIN_ID),
        nonce,
        gas_price: 0,
        gas_limit,
        to: TxKind::Call(EvmAddress::from(to)),
        value: U256::ZERO,
        input: Bytes::from(input),
    };
    let sig = key.sign_hash_sync(&tx.signature_hash()).unwrap();
    TxEnvelope::from(tx.into_signed(sig)).encoded_2718()
}

fn word(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

/// CoreWriter calldata for a GTC limit order (action 1; prices and sizes ×1e8).
fn core_writer_order(asset: u32, is_buy: bool, px: u128, sz: u128) -> Vec<u8> {
    let mut data = vec![1u8, 0, 0, 1];
    for w in [asset as u128, is_buy as u128, px * 100_000_000, sz, 0, 2, 0] {
        data.extend_from_slice(&word(w));
    }
    hc_evm::encode_send_raw_action(&data)
}

fn core_writer_cancel(asset: u32, oid: u64) -> Vec<u8> {
    let mut data = vec![1u8, 0, 0, 10];
    data.extend_from_slice(&word(asset as u128));
    data.extend_from_slice(&word(oid as u128));
    hc_evm::encode_send_raw_action(&data)
}

fn block(s: &mut State, h: u64, t: u64, txs: &[hc_core::SignedTx], evm: &[Vec<u8>]) -> BlockOutput {
    s.apply_block_with_evm(h, t, txs, evm)
}

fn evm_blocks(out: &BlockOutput) -> Vec<bool> {
    out.events.iter().filter_map(|e| match e { Event::EvmBlock { big, .. } => Some(*big), _ => None }).collect()
}

/// The events in `hash`'s span.
fn span(out: &BlockOutput, hash: [u8; 32]) -> &[Event] {
    let (_, a, b) = out.spans.iter().find(|(h, _, _)| h.0 == hash).expect("tx has a span");
    &out.events[*a as usize..*b as usize]
}

#[test]
fn small_blocks_run_each_second_and_core_writer_orders_wait() {
    let mut g = genesis();
    g.time_ms = T;
    let mut s = State::genesis(&g);
    let alice = evm_key("alice");
    let order = evm_tx(&alice, 0, hc_evm::CORE_WRITER, core_writer_order(0, true, 90_000, 1_000_000), 200_000);
    let hash = hc_evm::decode_tx(&order).unwrap().tx_hash;

    // Finalized in a core block within the same second: queued, no EVM block yet.
    let out = block(&mut s, 1, T + 100, &[], &[order]);
    assert!(evm_blocks(&out).is_empty());
    assert!(matches!(span(&out, hash), [Event::EvmTxQueued { big: false, .. }]));

    // The next second starts: a small block runs it. The order is queued, not placed.
    let out = block(&mut s, 2, T + 1_000, &[], &[]);
    assert_eq!(evm_blocks(&out), vec![false]);
    let events = span(&out, hash);
    assert!(matches!(events, [Event::EvmTx(r)] if r.success), "{events:?}");
    let queued = out.events.iter().find_map(|e| match e { Event::CoreWriterQueued { tx_hash, execute_at_ms, .. } if tx_hash.0 == hash => Some(*execute_at_ms), _ => None });
    assert_eq!(queued, Some(T + 1_000 + hc_core::evm_bridge::CORE_WRITER_DELAY_MS));
    assert!(s.books[0].orders.is_empty());

    // Not before the delay has passed...
    let out = block(&mut s, 3, T + 3_999, &[], &[]);
    assert!(!out.events.iter().any(|e| matches!(e, Event::CoreWriter { .. })));
    // ...then it rests on the book, as an end-of-block event pointing at its EVM tx.
    let out = block(&mut s, 4, T + 4_000, &[], &[]);
    let (i, oid) = out
        .events
        .iter()
        .enumerate()
        .find_map(|(i, e)| match e {
            Event::CoreWriter { tx_hash, response: OrderResponse::Resting { oid }, .. } if tx_hash.0 == hash => Some((i, *oid)),
            _ => None,
        })
        .expect("order placed");
    assert!(out.spans.iter().all(|(_, a, b)| !(*a as usize..*b as usize).contains(&i)), "delayed actions are end-of-block events");
    assert_eq!(s.books[0].orders[&oid].user, user("alice").1);

    // A CoreWriter cancel is not delayed: it runs right after its EVM block.
    let cancel = evm_tx(&alice, 1, hc_evm::CORE_WRITER, core_writer_cancel(0, oid), 200_000);
    block(&mut s, 5, T + 4_100, &[], &[cancel]);
    let out = block(&mut s, 6, T + 5_000, &[], &[]);
    assert!(out.events.iter().any(|e| matches!(e, Event::CoreWriter { response: OrderResponse::Success, .. })), "{:?}", out.events);
    assert!(s.books[0].orders.is_empty());
}

#[test]
fn big_blocks_take_heavy_transactions_once_a_minute() {
    let mut g = genesis();
    g.time_ms = T;
    let mut s = State::genesis(&g);
    let mut sg = Signer::new();
    sg.nonce = T;
    let bob = evm_key("bob");
    let heavy = |nonce| evm_tx(&bob, nonce, [0x44; 20], vec![], 5_000_000);

    // Over the small block's 2M gas: skipped, with a hint.
    let tx = heavy(0);
    let out = block(&mut s, 1, T + 100, &[], std::slice::from_ref(&tx));
    let hash = hc_evm::decode_tx(&tx).unwrap().tx_hash;
    assert!(matches!(span(&out, hash), [Event::EvmTxSkipped { reason, .. }] if reason.contains("usingBigBlocks")), "{:?}", out.events);

    // Bob opts into big blocks; the same transaction now waits for the next minute.
    let opt_in = sg.tx(&user("bob").0, Action::EvmUserModify { using_big_blocks: true });
    let out = block(&mut s, 2, T + 200, &[opt_in], &[tx]);
    assert!(matches!(span(&out, hash), [Event::EvmTxQueued { big: true, .. }]));
    for (h, t) in [(3, T + 1_000), (4, T + 30_000), (5, T + 59_999)] {
        assert!(evm_blocks(&block(&mut s, h, t, &[], &[])).is_empty(), "no big block before the minute");
    }
    let out = block(&mut s, 6, T + 60_000, &[], &[]);
    assert_eq!(evm_blocks(&out), vec![true]);
    assert!(matches!(span(&out, hash), [Event::EvmTx(r)] if r.success));
    assert_eq!(s.evm.nonce(user("bob").1 .0), 1);
}

#[test]
fn a_small_block_holds_at_most_2m_gas_and_keeps_order() {
    let mut g = genesis();
    g.time_ms = T;
    let mut s = State::genesis(&g);
    let carol = evm_key("carol");
    // Three 900k-gas transactions: two fit in one small block, the third waits.
    let txs: Vec<Vec<u8>> = (0..3).map(|n| evm_tx(&carol, n, [0x55; 20], vec![], 900_000)).collect();
    let hashes: Vec<Hash32> = txs.iter().map(|t| Hash32(hc_evm::decode_tx(t).unwrap().tx_hash)).collect();
    block(&mut s, 1, T + 100, &[], &txs);
    let mined = |out: &BlockOutput| out.events.iter().filter_map(|e| match e { Event::EvmTx(r) => Some(r.tx_hash), _ => None }).collect::<Vec<_>>();
    assert_eq!(mined(&block(&mut s, 2, T + 1_000, &[], &[])), hashes[..2]);
    assert_eq!(mined(&block(&mut s, 3, T + 2_000, &[], &[])), hashes[2..]);
}
