//! Vaults (create, deposit, leader trading with vaultAddress, withdraw with
//! lockup and profit share) and expiresAfter.

mod common;

use common::*;
use hc_core::events::TxResponse;
use hc_core::fixed::Usd;
use hc_core::types::Tif;
use hc_core::vault::{vault_address, LOCKUP_MS};
use hc_core::{Action, SignedTx, State};

fn err(r: &TxResponse) -> &str {
    match r {
        TxResponse::Error { error } => error,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn ok(r: &TxResponse) {
    assert!(!matches!(r, TxResponse::Error { .. }), "{r:?}");
    if let TxResponse::Order { statuses } = r {
        assert!(statuses.iter().all(|s| !matches!(s, hc_core::events::OrderResponse::Error(_))), "{r:?}");
    }
}

fn conserved(s: &State) {
    let total: i64 = s.users.values().map(|u| u.balance.0 + u.isolated_margin().0 - u.positions.values().map(|p| p.entry_ntl).sum::<i64>()).sum::<i64>() + s.net_bridged_to_evm.0;
    assert_eq!(Usd(total), s.total_minted);
}

const M: u64 = 1_000_000; // µUSDC per USDC

#[test]
fn vault_lifecycle() {
    let mut s = State::genesis(&genesis());
    let (ka, a) = user("alice");
    let (kb, b) = user("bob");
    let (kc, _) = user("carol");
    let (kd, _) = user("dave");
    let t = T0 + 10;

    // Alice opens a vault with 1,000 USDC; Bob deposits 1,000.
    let create = Action::CreateVault { name: "trend".into(), description: "BTC trend".into(), initial_usd: 1_000 * M, nonce: t };
    let out = s.apply_block(1, t, &[SignedTx::sign(&ka, create, t)]);
    let TxResponse::Vault { vault_address: v } = out.responses[0].2 else { panic!("{:?}", out.responses) };
    assert_eq!(v, vault_address(a, t));
    let deposit = Action::VaultTransfer { vault_address: v, is_deposit: true, usd: 1_000 * M };
    let out = s.apply_block(2, t + 1, &[SignedTx::sign(&kb, deposit, t + 1)]);
    ok(&out.responses[0].2);
    assert_eq!(s.users[&v].balance, Usd::from_whole(2_000));
    assert_eq!(s.vaults[&v].followers[&b].shares, s.vaults[&v].followers[&a].shares);

    // Only the leader can trade for it: Alice signs with vaultAddress; Bob cannot.
    s.apply_block(3, t + 2, &[SignedTx::sign(&kc, Action::order(vec![order(0, false, "100000", "0.1", Tif::Gtc)]), t + 2)]);
    let buy = Action::order(vec![order(0, true, "100000", "0.1", Tif::Ioc)]);
    let out = s.apply_block(4, t + 3, &[SignedTx::sign_with(&kb, buy.clone(), t + 3, Some(v), None), SignedTx::sign_with(&ka, buy, t + 4, Some(v), None)]);
    assert!(err(&out.responses[0].2).contains("not the leader"));
    ok(&out.responses[1].2);
    assert_eq!(out.responses[1].1, v, "the vault's account acted");
    assert_eq!(s.users[&v].positions[&0].szi, 10_000);
    assert!(s.users[&a].positions.is_empty());
    // A vault cannot move funds, even when its leader signs.
    let send_out = Action::VaultTransfer { vault_address: v, is_deposit: true, usd: M };
    let out = s.apply_block(5, t + 5, &[SignedTx::sign_with(&ka, send_out, t + 5, Some(v), None)]);
    assert!(err(&out.responses[0].2).contains("Only trading"));

    // Sell at 110k: the vault makes ~1,000 USDC (less fees).
    s.apply_block(6, t + 6, &[SignedTx::sign(&kd, Action::order(vec![order(0, true, "110000", "0.1", Tif::Gtc)]), t + 6)]);
    let sell = Action::order(vec![order(0, false, "110000", "0.1", Tif::Ioc)]);
    ok(&s.apply_block(7, t + 7, &[SignedTx::sign_with(&ka, sell, t + 7, Some(v), None)]).responses[0].2);
    let equity = s.vault_equity(v);
    assert!(equity > Usd::from_whole(2_990), "{equity}");

    // Bob's deposit is locked for a day.
    let withdraw = |usd| Action::VaultTransfer { vault_address: v, is_deposit: false, usd };
    let out = s.apply_block(8, t + 8, &[SignedTx::sign(&kb, withdraw(500 * M), t + 8)]);
    assert!(err(&out.responses[0].2).contains("a day"));

    // After a day Bob takes out his whole share: half the vault's equity.
    // Alice gets 10% of Bob's profit.
    let later = t + LOCKUP_MS + 10;
    let bob_value = equity.0 / 2;
    let (a0, b0) = (s.users[&a].balance, s.users[&b].balance);
    let out = s.apply_block(9, later, &[SignedTx::sign(&kb, withdraw(bob_value as u64), later)]);
    ok(&out.responses[0].2);
    let fee = (bob_value - 1_000 * M as i64) / 10;
    assert_eq!(s.users[&b].balance, b0 + Usd(bob_value - fee));
    assert_eq!(s.users[&a].balance, a0 + Usd(fee));
    assert!(!s.vaults[&v].followers.contains_key(&b));
    conserved(&s);

    // With followers gone the leader may take everything... but not with Bob still in.
    let out = s.apply_block(10, later + 1, &[SignedTx::sign(&kb, Action::VaultTransfer { vault_address: v, is_deposit: true, usd: 100 * M }, later + 1)]);
    ok(&out.responses[0].2);
    let all = s.vault_equity(v).0 as u64 - 100 * M;
    let out = s.apply_block(11, later + 2, &[SignedTx::sign(&ka, withdraw(all), later + 2)]);
    assert!(err(&out.responses[0].2).contains("at least 5%"));
    conserved(&s);
}

#[test]
fn expires_after_is_signed_and_enforced() {
    let mut s = State::genesis(&genesis());
    let (ka, a) = user("alice");
    let faucet = || Action::Faucet { usd: "1".into() };
    // Expired before its block: rejected.
    let out = s.apply_block(1, T0 + 1_000, &[SignedTx::sign_with(&ka, faucet(), T0 + 1, None, Some(T0 + 500))]);
    assert!(err(&out.responses[0].2).contains("expired"));
    // In time: accepted.
    let out = s.apply_block(2, T0 + 2_000, &[SignedTx::sign_with(&ka, faucet(), T0 + 2, None, Some(T0 + 5_000))]);
    ok(&out.responses[0].2);
    // The expiry is part of what was signed: changing it changes the signer.
    let mut tx = SignedTx::sign_with(&ka, faucet(), T0 + 3, None, Some(T0 + 5_000));
    tx.expires_after = Some(T0 + 9_999_999);
    assert_ne!(tx.signer(), Some(a));
}
