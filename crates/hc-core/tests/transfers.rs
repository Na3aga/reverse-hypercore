//! usdSend: a user-signed transfer of USDC between core accounts.

mod common;

use common::*;
use hc_core::action::{dev_key, key_address, HYPERLIQUID_CHAIN};
use hc_core::events::TxResponse;
use hc_core::fixed::Usd;
use hc_core::types::{hex_encode, Address};
use hc_core::{Action, SignedTx, State};

fn send(key: &hc_core::action::SigningKey, to: Address, amount: &str, time: u64) -> SignedTx {
    let action = Action::UsdSend {
        signature_chain_id: "0x66eee".into(),
        hyperliquid_chain: HYPERLIQUID_CHAIN.into(),
        destination: format!("0x{}", hex_encode(&to.0)),
        amount: amount.into(),
        time,
    };
    SignedTx::sign(key, action, time)
}

fn err(r: &TxResponse) -> &str {
    match r {
        TxResponse::Error { error } => error,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn usd_send_moves_withdrawable_usdc_and_conserves_supply() {
    let mut s = State::genesis(&genesis());
    let (ka, a) = user("alice");
    let fresh = key_address(&dev_key("someone-new"));
    let (a0, minted) = (s.users[&a].balance, s.total_minted);

    let out = s.apply_block(1, T0 + 10, &[send(&ka, fresh, "1234.5", T0 + 10)]);
    assert!(matches!(out.responses[0].2, TxResponse::Default), "{:?}", out.responses);
    assert_eq!(s.users[&a].balance, a0 - Usd(1_234_500_000));
    assert_eq!(s.users[&fresh].balance, Usd(1_234_500_000));
    assert_eq!(s.total_minted, minted, "nothing is created");

    // More than the account can withdraw.
    let out = s.apply_block(2, T0 + 20, &[send(&ka, fresh, "5000000", T0 + 20)]);
    assert!(err(&out.responses[0].2).contains("Insufficient"));
    // time must be the transaction nonce.
    let mut tx = send(&ka, fresh, "1", T0 + 30);
    tx.nonce = T0 + 31;
    let out = s.apply_block(3, T0 + 40, &[tx]);
    assert!(matches!(out.responses[0].2, TxResponse::Error { .. }));
}

#[test]
fn an_agent_cannot_send_funds() {
    let mut s = State::genesis(&genesis());
    let (ka, a) = user("alice");
    let agent_key = dev_key("alice-agent");
    let approve = Action::ApproveAgent {
        signature_chain_id: "0x66eee".into(),
        hyperliquid_chain: HYPERLIQUID_CHAIN.into(),
        agent_address: key_address(&agent_key),
        agent_name: String::new(),
        nonce: T0 + 1,
    };
    s.apply_block(1, T0 + 1, &[SignedTx::sign(&ka, approve, T0 + 1)]);
    let thief = key_address(&dev_key("thief"));
    let before = s.users[&a].balance;
    let out = s.apply_block(2, T0 + 2, &[send(&agent_key, thief, "100", T0 + 2)]);
    assert!(err(&out.responses[0].2).contains("cannot move funds"));
    assert_eq!(s.users[&a].balance, before);
}
