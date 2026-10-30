//! Isolated margin, the backstop liquidator, and auto-deleveraging.

mod common;

use common::*;
use hc_core::events::{Event, LiquidationMethod, OrderResponse, TxResponse};
use hc_core::exchange::liquidator;
use hc_core::fixed::{Px, Usd};
use hc_core::types::{Address, Tif};
use hc_core::{Action, State};

/// `Σ balances + Σ isolated margin − Σ cost basis + on EVM`, which must equal
/// the USDC minted after every block.
fn assert_conserved(s: &State) {
    let mut total: i64 = s.net_bridged_to_evm.0;
    for u in s.users.values() {
        total += u.balance.0 + u.isolated_margin().0 - u.positions.values().map(|p| p.entry_ntl).sum::<i64>();
    }
    assert_eq!(Usd(total), s.total_minted, "USDC not conserved at height {}", s.height);
}

/// Genesis plus Erin and Frank, who only print trades to move the mark.
fn genesis_with_printers() -> hc_core::Genesis {
    let mut g = genesis();
    for name in ["erin", "frank"] {
        g.balances.push((user(name).1, Usd::from_whole(1_000_000)));
    }
    g
}

/// A 0.001 BTC trade at `px` between Erin and Frank, leaving no resting order.
fn print(s: &mut State, sg: &mut Signer, height: u64, time: u64, px: &str) {
    let (ke, kf) = (user("erin").0, user("frank").0);
    s.apply_block(height, time, &[sg.orders(&ke, vec![order(0, false, px, "0.001", Tif::Gtc)]), sg.orders(&kf, vec![order(0, true, px, "0.001", Tif::Ioc)])]);
    assert_eq!(s.assets[0].prices.last_trade_px.map(|p| p.0 / 1_000_000), px.parse().ok());
}

fn ok(r: &TxResponse) {
    match r {
        TxResponse::Order { statuses } => assert!(statuses.iter().all(|s| !matches!(s, OrderResponse::Error(_))), "{r:?}"),
        TxResponse::Error { .. } => panic!("{r:?}"),
        _ => {}
    }
}

fn szi(s: &State, u: Address) -> i64 {
    s.users.get(&u).and_then(|u| u.positions.get(&0)).map_or(0, |p| p.szi)
}

fn liquidation(events: &[Event], user: Address) -> Option<(LiquidationMethod, Usd)> {
    events.iter().find_map(|e| match e {
        Event::Liquidation { user: u, method, account_value, .. } if *u == user => Some((*method, *account_value)),
        _ => None,
    })
}

#[test]
fn isolated_margin_moves_in_and_out_of_the_cross_balance() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kb, _) = user("bob");
    let start = s.users[&a].balance;

    // Switch BTC to isolated 10x, then buy 1 BTC at 100k from Bob's ask.
    let out = s.apply_block(1, T0 + 1, &[sg.tx(&ka, Action::UpdateLeverage { asset: 0, is_cross: false, leverage: 10 })]);
    ok(&out.responses[0].2);
    s.apply_block(2, T0 + 2, &[sg.orders(&kb, vec![order(0, false, "100000", "1", Tif::Gtc)])]);
    let out = s.apply_block(3, T0 + 3, &[sg.orders(&ka, vec![order(0, true, "100000", "1", Tif::Ioc)])]);
    ok(&out.responses[0].2);
    let p = &s.users[&a].positions[&0];
    // 10k initial margin moved out of the cross balance (plus the taker fee).
    assert_eq!(p.isolated, Some(Usd::from_whole(10_000)));
    let fee = Usd::from_whole(100_000).0 * 450 / 1_000_000;
    assert_eq!(s.users[&a].balance, start - Usd::from_whole(10_000) - Usd(fee));
    assert_conserved(&s);

    // Mode cannot change with the position open.
    let out = s.apply_block(4, T0 + 4, &[sg.tx(&ka, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 10 })]);
    assert!(matches!(&out.responses[0].2, TxResponse::Error { error } if error.contains("Cannot switch")), "{:?}", out.responses);

    // Add 5k, then try to take out more than the position can spare.
    let out = s.apply_block(5, T0 + 5, &[sg.tx(&ka, Action::UpdateIsolatedMargin { asset: 0, is_buy: true, ntli: 5_000_000_000 })]);
    ok(&out.responses[0].2);
    assert_eq!(s.users[&a].positions[&0].isolated, Some(Usd::from_whole(15_000)));
    let out = s.apply_block(6, T0 + 6, &[sg.tx(&ka, Action::UpdateIsolatedMargin { asset: 0, is_buy: true, ntli: -6_000_000_000 })]);
    assert!(matches!(&out.responses[0].2, TxResponse::Error { .. }), "{:?}", out.responses);
    let out = s.apply_block(7, T0 + 7, &[sg.tx(&ka, Action::UpdateIsolatedMargin { asset: 0, is_buy: true, ntli: -5_000_000_000 })]);
    ok(&out.responses[0].2);
    assert_conserved(&s);

    // Sell half at 101k: 500 profit; half of the margin (and the profit) returns.
    s.apply_block(8, T0 + 8, &[sg.orders(&kb, vec![order(0, true, "101000", "0.5", Tif::Gtc)])]);
    let before = s.users[&a].balance;
    s.apply_block(9, T0 + 9, &[sg.orders(&ka, vec![order(0, false, "101000", "0.5", Tif::Ioc)])]);
    let fee = Usd::from_whole(50_500).0 * 450 / 1_000_000;
    assert_eq!(s.users[&a].positions[&0].isolated, Some(Usd::from_whole(5_250)));
    assert_eq!(s.users[&a].balance, before + Usd::from_whole(5_250) - Usd(fee));
    assert_conserved(&s);
}

#[test]
fn isolated_liquidation_leaves_the_cross_account_alone() {
    let mut s = State::genesis(&genesis_with_printers());
    let mut sg = Signer::new();
    let (kd, d) = user("dave");
    let (ka, _) = user("alice");
    let (kc, _) = user("carol");
    let vk = validator_keys();
    // Dave: isolated 40x long 1 BTC (2.5k margin) next to a cross ETH long.
    s.apply_block(1, T0 + 1, &[sg.tx(&kd, Action::UpdateLeverage { asset: 0, is_cross: false, leverage: 40 })]);
    s.apply_block(
        2,
        T0 + 2,
        &[sg.orders(&ka, vec![order(0, false, "100000", "1", Tif::Gtc)]), sg.orders(&ka, vec![order(1, false, "4000", "1", Tif::Gtc)])],
    );
    let out = s.apply_block(3, T0 + 3, &[sg.orders(&kd, vec![order(0, true, "100000", "1", Tif::Ioc)]), sg.orders(&kd, vec![order(1, true, "4000", "1", Tif::Ioc)])]);
    out.responses.iter().for_each(|r| ok(&r.2));
    let cross_before = s.users[&d].balance;
    // Carol bids under the market so the liquidation has a buyer.
    s.apply_block(4, T0 + 4, &[sg.orders(&kc, vec![order(0, true, "98500", "2", Tif::Gtc)])]);
    print(&mut s, &mut sg, 5, T0 + 5, "98600");

    // BTC to ~98.6k: isolated equity 2.5k - 1.45k = 1.05k < MM 1.23k (1.25%), above 2/3 of it.
    let out = s.apply_block(6, T0 + 5_000, &sg.oracle(&vk, &["98600", "4000"]));
    // The mark is the median of the oracle, the book (98.5k bid, 98.6k last) and its average.
    assert_eq!(s.assets[0].prices.mark_px, Px(98_550_000_000));
    assert_eq!(liquidation(&out.events, d).map(|l| l.0), Some(LiquidationMethod::Market), "{:?}", out.events);
    // Above the 10k partial threshold, so 20% is sold into Carol's bid.
    assert_eq!(szi(&s, d), 80_000);
    // The closed fifth's share of the margin came back to the cross balance;
    // the loss stayed inside the isolated margin.
    assert!(s.users[&d].balance > cross_before, "released margin returns to the cross balance");
    assert!(s.users[&d].positions.contains_key(&1), "the cross ETH position is untouched");
    assert_conserved(&s);
}

#[test]
fn backstop_takes_over_when_equity_is_low_but_positive() {
    let mut g = genesis_with_printers();
    let (kd, d) = user("dave");
    g.balances.retain(|(a, _)| *a != d);
    g.balances.push((d, Usd::from_whole(30_000)));
    let mut s = State::genesis(&g);
    let mut sg = Signer::new();
    let (ka, _) = user("alice");
    let vk = validator_keys();
    s.apply_block(1, T0 + 1, &[sg.tx(&kd, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 40 })]);
    s.apply_block(2, T0 + 2, &[sg.orders(&ka, vec![order(0, false, "100000", "10", Tif::Gtc)])]);
    s.apply_block(3, T0 + 3, &[sg.orders(&kd, vec![order(0, true, "100000", "10", Tif::Ioc)])]);
    print(&mut s, &mut sg, 4, T0 + 4, "97500");
    // Mark 97.5k: equity 30k - 450 fee - 25k = ~4.5k, under 2/3 of MM (12.2k).
    let out = s.apply_block(5, T0 + 5_000, &sg.oracle(&vk, &["97500", "4000"]));
    assert_eq!(s.assets[0].prices.mark_px, Px(97_500_000_000));
    let (method, equity) = liquidation(&out.events, d).expect("liquidated");
    assert_eq!(method, LiquidationMethod::Backstop);
    assert!(equity > Usd::ZERO);
    assert_eq!(szi(&s, d), 0);
    assert_eq!(szi(&s, liquidator()), 1_000_000, "the vault holds the 10 BTC long");
    assert_conserved(&s);
}

#[test]
fn adl_closes_an_insolvent_isolated_position_against_the_best_ranked_shorts() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (kd, d) = user("dave");
    let (ka, a) = user("alice");
    let (kb, b) = user("bob");
    let (kc, _) = user("carol");
    let vk = validator_keys();
    // Dave: isolated 40x long 2 BTC. Alice (20x) and Bob (2x) sold to him at 100k.
    s.apply_block(
        1,
        T0 + 1,
        &[
            sg.tx(&kd, Action::UpdateLeverage { asset: 0, is_cross: false, leverage: 40 }),
            sg.tx(&ka, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 20 }),
            sg.tx(&kb, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 2 }),
        ],
    );
    s.apply_block(2, T0 + 2, &[sg.orders(&ka, vec![order(0, false, "100000", "1", Tif::Gtc)]), sg.orders(&kb, vec![order(0, false, "100000", "1", Tif::Gtc)])]);
    s.apply_block(3, T0 + 3, &[sg.orders(&kd, vec![order(0, true, "100000", "2", Tif::Ioc)])]);
    assert_eq!(szi(&s, d), 200_000);
    // Alice sells Carol 0.01 at 90k: the last trade, with an empty book, moves the mark.
    s.apply_block(4, T0 + 4, &[sg.orders(&ka, vec![order(0, false, "90000", "0.01", Tif::Gtc)]), sg.orders(&kc, vec![order(0, true, "90000", "0.01", Tif::Ioc)])]);

    // Oracle 90k: Dave's 5k margin is down ~20k; his position is below zero.
    let out = s.apply_block(5, T0 + 5_000, &sg.oracle(&vk, &["90000", "4000"]));
    let (method, equity) = liquidation(&out.events, d).expect("liquidated");
    assert_eq!(method, LiquidationMethod::Adl, "{:?}", out.events);
    assert!(equity < Usd::ZERO);
    assert_eq!(szi(&s, d), 0);
    assert_eq!(szi(&s, liquidator()), 0, "the vault took nothing");
    // Equal profits per BTC, so the higher-leveraged Alice is deleveraged first.
    let order: Vec<Address> = out.events.iter().filter_map(|e| match e { Event::Fill(f) if f.user != d => Some(f.user), _ => None }).collect();
    assert_eq!(order, vec![a, b]);
    assert_eq!(szi(&s, a), 0, "Alice's whole 1.01 BTC short is closed");
    assert_eq!(szi(&s, b), -1_000, "Bob covers the remaining 0.99 BTC");
    // Bankruptcy price: 2 BTC from 100k lose exactly the 5k margin at 97.5k.
    let px = out.events.iter().find_map(|e| match e { Event::Fill(f) if f.user == d => Some(f.px), _ => None }).unwrap();
    assert_eq!(px, Px(97_500_000_000));
    assert_eq!(equity, Usd::from_whole(-15_000));
    assert_conserved(&s);
}
