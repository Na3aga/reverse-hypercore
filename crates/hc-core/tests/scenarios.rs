//! Behavioural scenarios for matching, in-block priority, margin, funding,
//! oracle and liquidation.

mod common;

use common::*;
use hc_core::events::{Event, LiquidationMethod, OrderResponse, TxResponse};
use hc_core::exchange::{fee_collector, liquidator};
use hc_core::fixed::{Px, Usd};
use hc_core::types::{OrderStatus, Tif};
use hc_core::{Action, State};

fn statuses(r: &TxResponse) -> &Vec<OrderResponse> {
    match r {
        TxResponse::Order { statuses } | TxResponse::Cancel { statuses } => statuses,
        other => panic!("unexpected response {other:?}"),
    }
}

#[test]
fn limit_matching_price_time_priority_and_ioc() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kb, _) = user("bob");
    let (kc, c) = user("carol");
    // Alice and Carol rest asks at 100k; Alice first in time.
    let out = s.apply_block(
        1,
        T0 + 100,
        &[
            sg.orders(&ka, vec![order(0, false, "100000", "0.1", Tif::Gtc)]),
            sg.orders(&kc, vec![order(0, false, "100000", "0.1", Tif::Gtc)]),
            sg.orders(&ka, vec![order(0, false, "101000", "0.1", Tif::Gtc)]),
        ],
    );
    assert!(out.responses.iter().all(|(_, _, r)| matches!(statuses(r)[0], OrderResponse::Resting { .. })));
    // Bob IOC-buys 0.15 at 100500: fills 0.1 from Alice then 0.05 from Carol, not the 101k level.
    let out = s.apply_block(2, T0 + 200, &[sg.orders(&kb, vec![order(0, true, "100500", "0.15", Tif::Ioc)])]);
    match &statuses(&out.responses[0].2)[0] {
        OrderResponse::Filled { total_sz, avg_px, .. } => {
            assert_eq!(*total_sz, 15_000);
            assert_eq!(*avg_px, Px(100_000_000_000));
        }
        other => panic!("{other:?}"),
    }
    let makers: Vec<_> = out.events.iter().filter_map(|e| match e { Event::Fill(f) if !f.crossed => Some((f.user, f.sz)), _ => None }).collect();
    assert_eq!(makers, vec![(a, 10_000), (c, 5_000)]);
    // An IOC that cannot match is rejected with the public message.
    let out = s.apply_block(3, T0 + 300, &[sg.orders(&kb, vec![order(0, true, "99000", "0.1", Tif::Ioc)])]);
    assert_eq!(statuses(&out.responses[0].2)[0], OrderResponse::Error("Order could not immediately match against any resting orders.".into()));
    // Fees went to the fee collector.
    assert!(s.users[&fee_collector()].balance > Usd::ZERO);
}

#[test]
fn post_only_rejects_when_crossing_and_self_trade_expires_maker() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, _) = user("alice");
    let (kb, _) = user("bob");
    s.apply_block(1, T0 + 1, &[sg.orders(&ka, vec![order(0, false, "100000", "0.1", Tif::Gtc)])]);
    let out = s.apply_block(2, T0 + 2, &[sg.orders(&kb, vec![order(0, true, "100000", "0.1", Tif::Alo)])]);
    assert_eq!(statuses(&out.responses[0].2)[0], OrderResponse::Error("Post only order would have immediately matched.".into()));
    // Alice crosses her own ask: the resting ask is canceled, no fill.
    let out = s.apply_block(3, T0 + 3, &[sg.orders(&ka, vec![order(0, true, "100000", "0.1", Tif::Gtc)])]);
    assert!(out.events.iter().any(|e| matches!(e, Event::Order(o) if o.status == OrderStatus::SelfTradeCanceled)));
    assert!(!out.events.iter().any(|e| matches!(e, Event::Fill(_))));
}

#[test]
fn cancels_execute_before_gtc_in_same_block() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, _) = user("alice");
    let (kb, _) = user("bob");
    let out = s.apply_block(1, T0 + 1, &[sg.orders(&ka, vec![order(0, false, "100000", "0.1", Tif::Gtc)])]);
    let OrderResponse::Resting { oid } = statuses(&out.responses[0].2)[0] else { panic!() };
    // Proposer order: Bob's taker first, Alice's cancel second. Priority sorting
    // runs the cancel first, so Bob's IOC finds nothing.
    let bob_ioc = sg.orders(&kb, vec![order(0, true, "100000", "0.1", Tif::Ioc)]);
    let alice_cancel = sg.cancel(&ka, 0, oid);
    let out = s.apply_block(2, T0 + 2, &[bob_ioc.clone(), alice_cancel.clone()]);
    assert_eq!(out.responses[0].0, alice_cancel.hash(), "cancel delivered first");
    assert_eq!(statuses(&out.responses[0].2)[0], OrderResponse::Success);
    assert!(matches!(statuses(&out.responses[1].2)[0], OrderResponse::Error(_)));
    // ALO-only actions (category 0) run before cancels too.
    let alo = sg.orders(&kb, vec![order(0, true, "99000", "0.1", Tif::Alo)]);
    let gtc = sg.orders(&kb, vec![order(0, true, "98000", "0.1", Tif::Gtc)]);
    let out = s.apply_block(3, T0 + 3, &[gtc.clone(), alo.clone()]);
    assert_eq!(out.responses[0].0, alo.hash());
}

#[test]
fn margin_and_validation_rejections() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, _) = user("alice");
    let out = s.apply_block(
        1,
        T0 + 1,
        &[sg.orders(
            &ka,
            vec![
                order(0, true, "100000.5", "0.1", Tif::Gtc), // 7 sig figs, not an integer
                order(0, true, "100000", "0.000001", Tif::Gtc), // below lot size
                order(0, true, "1000", "0.001", Tif::Gtc),   // > 80% from oracle
                order(0, true, "99000", "0.0001", Tif::Gtc), // $9.9 < $10 min
                order(0, true, "99000", "500", Tif::Gtc),    // $49.5M at 20x needs 2.47M > 1M equity
            ],
        )],
    );
    let st = statuses(&out.responses[0].2);
    let errs: Vec<String> = st.iter().map(|r| match r { OrderResponse::Error(e) => e.clone(), o => format!("{o:?}") }).collect();
    assert_eq!(errs[0], "Price must be divisible by tick size.");
    assert_eq!(errs[1], "Order has invalid size.");
    assert!(errs[2].starts_with("Order price cannot be more than 80% away"));
    assert_eq!(errs[3], "Order must have minimum value of $10.");
    assert_eq!(errs[4], "Insufficient margin to place order.");
}

#[test]
fn replayed_nonce_and_bad_signature_are_rejected() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, _) = user("alice");
    let tx = sg.orders(&ka, vec![order(0, false, "100000", "0.1", Tif::Gtc)]);
    s.apply_block(1, T0 + 1, std::slice::from_ref(&tx));
    let out = s.apply_block(2, T0 + 2, std::slice::from_ref(&tx));
    assert_eq!(out.responses[0].2, TxResponse::err("Invalid nonce: duplicate nonce."));
    // A malformed signature (invalid recovery byte) is rejected outright.
    let mut forged = sg.orders(&ka, vec![order(0, false, "100000", "0.1", Tif::Gtc)]);
    forged.signature.0[64] = 0;
    let out = s.apply_block(3, T0 + 3, &[forged]);
    assert_eq!(out.responses[0].2, TxResponse::err("Invalid signature."));
    // A tampered payload recovers a *different* address, which owns nothing:
    // the order cannot be attributed to alice and fails the margin check.
    let (_, alice) = user("alice");
    let mut tampered = sg.orders(&ka, vec![order(0, false, "100000", "0.1", Tif::Gtc)]);
    tampered.nonce += 1000;
    assert_ne!(tampered.signer(), Some(alice));
    let out = s.apply_block(4, T0 + 4, &[tampered]);
    assert_ne!(out.responses[0].1, alice);
    assert!(matches!(&out.responses[0].2, TxResponse::Order { statuses } if matches!(statuses[0], OrderResponse::Error(_))));
}

#[test]
fn oracle_median_and_funding_payment() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let vk = validator_keys();
    // Three validators quote, one outlier; stake-weighted median ignores it.
    let mut txs = sg.oracle(&vk[..3], &["101000", "4000"]);
    txs[2] = sg.tx(&vk[2], Action::SetOracle { pxs: vec!["999999".into(), "4000".into()] });
    s.apply_block(1, T0 + 10, &txs);
    assert_eq!(s.assets[0].prices.oracle_px, Px(101_000_000_000));
    // Once the earlier votes are stale, two fresh votes (half the stake) are not
    // a quorum: price unchanged.
    let txs = sg.oracle(&vk[..2], &["50000", "4000"]);
    s.apply_block(2, T0 + 20_000, &txs);
    assert_eq!(s.assets[0].prices.oracle_px, Px(101_000_000_000));

    // Open a 1 BTC long/short pair, then cross an hour boundary with an empty book:
    // premium 0 -> hourly rate = 0.0001/8 = 0.0000125; long pays 101000 * 0.0000125 = 1.2625.
    let (ka, a) = user("alice");
    let (kb, b) = user("bob");
    s.apply_block(3, T0 + 20_030, &[sg.orders(&ka, vec![order(0, false, "101000", "1", Tif::Gtc)])]);
    s.apply_block(4, T0 + 20_040, &[sg.orders(&kb, vec![order(0, true, "101000", "1", Tif::Ioc)])]);
    let (bal_a, bal_b) = (s.users[&a].balance, s.users[&b].balance);
    let next_hour = (T0 / 3_600_000 + 1) * 3_600_000;
    let out = s.apply_block(5, next_hour + 1, &[]);
    let pays: Vec<_> = out.events.iter().filter_map(|e| match e { Event::Funding { user, usdc, .. } => Some((*user, *usdc)), _ => None }).collect();
    assert!(pays.contains(&(b, Usd(-1_262_500))), "{pays:?}");
    assert!(pays.contains(&(a, Usd(1_262_500))), "{pays:?}");
    assert_eq!(s.users[&b].balance, bal_b - Usd(1_262_500));
    assert_eq!(s.users[&a].balance, bal_a + Usd(1_262_500));
}

#[test]
fn liquidation_partial_then_backstop() {
    let mut g = genesis();
    let (kd, d) = user("dave");
    // Dave has only 30k; he goes 10 BTC long at 40x (1M notional, 25k IM).
    g.balances.retain(|(a, _)| *a != d);
    g.balances.push((d, Usd::from_whole(30_000)));
    let mut s = State::genesis(&g);
    let mut sg = Signer::new();
    let (ka, _) = user("alice");
    let (kc, _) = user("carol");
    let vk = validator_keys();
    s.apply_block(1, T0 + 1, &[sg.tx(&kd, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 40 })]);
    s.apply_block(2, T0 + 2, &[sg.orders(&ka, vec![order(0, false, "100000", "10", Tif::Gtc)])]);
    let out = s.apply_block(3, T0 + 3, &[sg.orders(&kd, vec![order(0, true, "100000", "10", Tif::Ioc)])]);
    assert!(matches!(statuses(&out.responses[0].2)[0], OrderResponse::Filled { .. }), "{:?}", out.responses);
    // Book: Carol bids 20 @ 98000, Alice offers a little @ 98100.
    let out = s.apply_block(
        4,
        T0 + 4,
        &[
            sg.orders(&kc, vec![order(0, true, "98000", "20", Tif::Gtc)]),
            sg.orders(&ka, vec![order(0, false, "98100", "0.001", Tif::Gtc)]),
        ],
    );
    let OrderResponse::Resting { oid: carol_bid } = statuses(&out.responses[0].2)[0] else { panic!() };

    // Oracle 98100 -> mark 98100: equity 29550 - 19000 = 10550 < MM 12262 but
    // above 2/3 MM, and the position is above the partial threshold: 20% is
    // sold into Carol's bid.
    let out = s.apply_block(5, T0 + 5_000, &sg.oracle(&vk, &["98100", "4000"]));
    assert_eq!(s.assets[0].prices.mark_px, Px(98_100_000_000));
    let liq = out.events.iter().find_map(|e| match e { Event::Liquidation { method, .. } => Some(*method), _ => None });
    assert_eq!(liq, Some(LiquidationMethod::Market), "{:?}", out.events);
    assert_eq!(s.users[&d].positions[&0].szi, 800_000, "20% of 10 BTC closed");
    assert!(s.users[&d].liq_cooldown_until > s.time_ms);

    // Liquidity disappears and a trade prints at 90000 (cancel runs first by priority).
    s.apply_block(
        6,
        T0 + 6_000,
        &[
            sg.orders(&ka, vec![order(0, false, "90000", "0.01", Tif::Gtc)]),
            sg.orders(&kc, vec![order(0, true, "90000", "0.01", Tif::Ioc)]),
            sg.cancel(&kc, 0, carol_bid),
        ],
    );
    assert_eq!(s.assets[0].prices.last_trade_px, Some(Px(90_000_000_000)));
    assert!(s.users[&d].positions.contains_key(&0), "mark has not moved yet");

    // Oracle 90000: mark ~94050, equity below zero -> auto-deleveraging. Dave's
    // 8 BTC long closes at its bankruptcy price against the shorts (Alice),
    // so the loss is not left to the liquidator vault.
    let liq_before = s.users[&liquidator()].positions.get(&0).map(|p| p.szi).unwrap_or(0);
    let alice_before = s.users[&user("alice").1].positions[&0].szi;
    let out = s.apply_block(7, T0 + 10_000, &sg.oracle(&vk, &["90000", "4000"]));
    let liq = out.events.iter().find_map(|e| match e { Event::Liquidation { method, account_value, .. } => Some((*method, *account_value)), _ => None });
    assert!(matches!(liq, Some((LiquidationMethod::Adl, v)) if v < Usd::ZERO), "{:?}", out.events);
    assert!(s.users[&d].positions.is_empty());
    assert_eq!(s.users[&d].balance, Usd::ZERO);
    assert_eq!(s.users[&liquidator()].positions.get(&0).map(|p| p.szi).unwrap_or(0), liq_before);
    assert_eq!(s.users[&user("alice").1].positions[&0].szi, alice_before + 800_000, "Alice's short took the other side");
    // The bankruptcy price is above the mark: Dave's loss is exactly his equity.
    let fill = out.events.iter().find_map(|e| match e { Event::Fill(f) if f.user == d => Some(f.px), _ => None }).unwrap();
    assert!(fill > s.assets[0].prices.mark_px, "bankruptcy px {fill:?} vs mark {:?}", s.assets[0].prices.mark_px);
}
