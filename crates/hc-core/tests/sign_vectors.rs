//! Cross-language signing vectors: `crates/hc-node/src/ui/hcsign.js` must
//! reproduce `tests/fixtures/sign_vectors.json` byte for byte, and so must an
//! independent Hyperliquid SDK (both checked by `scripts/check-js-signer.mjs`).
//! This test pins the Rust side.

mod common;

use common::*;
use hc_core::action::{dev_key, CancelWire, OrderTypeWire, TriggerWire};
use hc_core::types::{Cloid, Tif, Tpsl};
use hc_core::{Action, SignedTx};

fn vectors() -> Vec<SignedTx> {
    let k = dev_key("alice");
    let mut with_cloid = order(1, false, "3999.5", "1.25", Tif::Alo);
    with_cloid.c = Some(Cloid([0xab; 16]));
    let mut sl = order(0, false, "90000", "0.1", Tif::Gtc);
    sl.r = true;
    sl.t = OrderTypeWire::Trigger(TriggerWire { is_market: true, trigger_px: "95000".into(), tpsl: Tpsl::Sl });
    let mut tp = order(0, false, "110000", "0.1", Tif::Gtc);
    tp.r = true;
    tp.t = OrderTypeWire::Trigger(TriggerWire { is_market: false, trigger_px: "110000".into(), tpsl: Tpsl::Tp });
    let actions = vec![
        Action::order(vec![order(0, true, "100000", "0.1", Tif::Gtc), with_cloid]),
        Action::order_with_tpsl(vec![order(0, true, "100000", "0.1", Tif::Gtc), tp, sl]),
        Action::SetRiskLimits { max_position_usd: Some("250000".into()), max_leverage: Some(10), daily_loss_usd: None, close_at_margin_pct: Some(80) },
        Action::Cancel { cancels: vec![CancelWire { a: 0, o: 300 }] },
        Action::UpdateLeverage { asset: 1, is_cross: true, leverage: 10 },
        Action::SendToEvm { usd: "250.5".into() },
        Action::Faucet { usd: "1000".into() },
        Action::UpdateIsolatedMargin { asset: 0, is_buy: true, ntli: -2_500_000_000 },
        Action::TwapOrder { twap: hc_core::twap::TwapWire { a: 1, b: false, s: "12.5".into(), r: false, m: 30, t: true } },
        Action::TwapCancel { a: 1, t: 7 },
    ];
    actions.into_iter().enumerate().map(|(i, a)| SignedTx::sign(&k, a, 1_760_000_000_000 + i as u64)).collect()
}

#[test]
fn signing_vectors_are_stable() {
    let json = serde_json::to_string_pretty(&vectors()).unwrap();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sign_vectors.json");
    if std::env::var("UPDATE_VECTORS").is_ok() || !std::path::Path::new(path).exists() {
        std::fs::write(path, &json).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), json, "signing changed; rerun with UPDATE_VECTORS=1 and update hcsign.js");
    for tx in vectors() {
        assert!(tx.verify());
    }
}

/// The libsecp256k1 fast path recovers the same signer as the k256 path, and
/// rejects the same malformed signatures. High-S twins are rejected by both.
#[test]
fn fast_recovery_matches_k256() {
    use hc_core::action::{dev_key, key_address, recover_signers};
    use hc_core::{Action, SignedTx};
    let mut txs = Vec::new();
    for i in 0..300u64 {
        let key = dev_key(&format!("x-{i}"));
        let tx = SignedTx::sign(&key, Action::Faucet { usd: format!("{}", i + 1) }, 1_000 + i);
        assert_eq!(tx.signer(), Some(key_address(&key)));
        assert_eq!(tx.signer(), tx.signer_k256());
        // Corruptions: flip a bit in r, in s, bad recovery byte, wrong nonce.
        let mut bad = tx.clone();
        bad.signature.0[(i % 64) as usize] ^= 1 << (i % 8);
        assert_eq!(bad.signer(), bad.signer_k256(), "corrupted sig {i}");
        let mut bad = tx.clone();
        bad.signature.0[64] = [0, 26, 31, 255][(i % 4) as usize];
        assert_eq!(bad.signer(), None);
        assert_eq!(bad.signer_k256(), None);
        let mut other = tx.clone();
        other.nonce += 1;
        assert_eq!(other.signer(), other.signer_k256());
        // High-S twin: s' = n - s, recovery parity flipped. Same signer, other encoding.
        let mut hi = tx.clone();
        let n = <k256::Secp256k1 as k256::elliptic_curve::Curve>::ORDER;
        let s = k256::U256::from_be_slice(&tx.signature.0[32..64]);
        hi.signature.0[32..64].copy_from_slice(&k256::elliptic_curve::bigint::Encoding::to_be_bytes(&n.wrapping_sub(&s)));
        hi.signature.0[64] ^= 1;
        assert_eq!(hi.signer(), None, "high-S must be rejected");
        assert_eq!(hi.signer_k256(), None, "k256 rejects high-S too");
        txs.push(tx);
    }
    let par = recover_signers(&txs);
    assert_eq!(par, txs.iter().map(SignedTx::signer).collect::<Vec<_>>());
}
