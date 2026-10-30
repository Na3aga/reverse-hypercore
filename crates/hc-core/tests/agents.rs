//! Agent (API) wallets: an account approves a key with a user-signed
//! `approveAgent`, and that key's L1 actions then act for the account.

mod common;

use common::*;
use hc_core::action::{dev_key, key_address, HYPERLIQUID_CHAIN};
use hc_core::events::{OrderResponse, TxResponse};
use hc_core::types::{Address, Tif};
use hc_core::{Action, SignedTx, State};

fn approve(owner: &hc_core::action::SigningKey, agent: Address, name: &str, nonce: u64) -> SignedTx {
    let action = Action::ApproveAgent {
        signature_chain_id: "0x66eee".into(),
        hyperliquid_chain: HYPERLIQUID_CHAIN.into(),
        agent_address: agent,
        agent_name: name.into(),
        nonce,
    };
    SignedTx::sign(owner, action, nonce)
}

fn err(r: &TxResponse) -> &str {
    match r {
        TxResponse::Error { error } => error,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn an_approved_agent_trades_for_its_owner() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kb, _) = user("bob");
    let agent_key = dev_key("alice-agent");
    let agent = key_address(&agent_key);

    // Before approval the agent's order acts for the agent's own (empty) account.
    let out = s.apply_block(1, T0 + 1, &[sg.orders(&kb, vec![order(0, false, "100000", "1", Tif::Gtc)]), sg.orders(&agent_key, vec![order(0, true, "100000", "0.1", Tif::Ioc)])]);
    assert_eq!(out.responses[1].1, agent);
    assert!(matches!(&out.responses[1].2, TxResponse::Order { statuses } if matches!(statuses[0], OrderResponse::Error(_))));

    let out = s.apply_block(2, T0 + 2, &[approve(&ka, agent, "bot", T0 + 2)]);
    assert!(matches!(out.responses[0].2, TxResponse::Default), "{:?}", out.responses);
    assert_eq!(s.agents[&agent].owner, a);

    // Now the agent's signature moves Alice's account.
    let out = s.apply_block(3, T0 + 3, &[sg.orders(&agent_key, vec![order(0, true, "100000", "0.1", Tif::Ioc)])]);
    assert_eq!(out.responses[0].1, a, "the response names the account acted for");
    assert!(matches!(&out.responses[0].2, TxResponse::Order { statuses } if matches!(statuses[0], OrderResponse::Filled { .. })));
    assert_eq!(s.users[&a].positions[&0].szi, 10_000);
    assert!(!s.users.contains_key(&agent) || s.users[&agent].positions.is_empty());

    // An agent cannot approve agents.
    let out = s.apply_block(4, T0 + 4, &[approve(&agent_key, key_address(&dev_key("x")), "", T0 + 4)]);
    assert!(err(&out.responses[0].2).contains("cannot approve"));

    // Approving another key under the same name replaces the first.
    let second = key_address(&dev_key("alice-agent-2"));
    s.apply_block(5, T0 + 5, &[approve(&ka, second, "bot", T0 + 5)]);
    assert!(!s.agents.contains_key(&agent));
    assert_eq!(s.agents[&second].owner, a);
}

#[test]
fn approve_agent_checks() {
    let mut s = State::genesis(&genesis());
    let (ka, _) = user("alice");
    let (_, b) = user("bob");
    let agent = key_address(&dev_key("agent"));
    // The action's nonce must be the transaction's.
    let mut tx = approve(&ka, agent, "", T0 + 1);
    tx.nonce = T0 + 9;
    let out = s.apply_block(1, T0 + 10, &[tx]);
    // The digest no longer matches, so another address is recovered; and the nonces differ.
    assert!(err(&out.responses[0].2).contains("nonce"));
    // An existing account cannot become an agent.
    let out = s.apply_block(2, T0 + 20, &[approve(&ka, b, "", T0 + 20)]);
    assert!(err(&out.responses[0].2).contains("already an account"));
    // Wrong network.
    let action = Action::ApproveAgent { signature_chain_id: "0x1".into(), hyperliquid_chain: "Mainnet".into(), agent_address: agent, agent_name: String::new(), nonce: T0 + 30 };
    let out = s.apply_block(3, T0 + 30, &[SignedTx::sign(&ka, action, T0 + 30)]);
    assert!(err(&out.responses[0].2).contains("hyperliquidChain"));
}
