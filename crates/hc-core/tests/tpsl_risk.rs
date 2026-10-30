//! TP/SL trigger orders and per-account risk limits.

mod common;

use common::*;
use hc_core::action::{OrderTypeWire, OrderWire, TriggerWire};
use hc_core::events::{Event, OrderResponse, RiskKind, TxResponse};
use hc_core::types::{OrderStatus, Tif, Tpsl};
use hc_core::{Action, State};

fn statuses(r: &TxResponse) -> &Vec<OrderResponse> {
    match r {
        TxResponse::Order { statuses } | TxResponse::Cancel { statuses } => statuses,
        other => panic!("unexpected response {other:?}"),
    }
}

/// A reduce-only market trigger (sell side closes a long).
fn trigger(is_buy: bool, trigger_px: &str, sz: &str, tpsl: Tpsl) -> OrderWire {
    let mut o = order(0, is_buy, if is_buy { "110000" } else { "90000" }, sz, Tif::Gtc);
    o.r = true;
    o.t = OrderTypeWire::Trigger(TriggerWire { is_market: true, trigger_px: trigger_px.into(), tpsl });
    o
}

fn statuses_of(out: &hc_core::BlockOutput, user: hc_core::types::Address) -> Vec<(u64, OrderStatus)> {
    out.events.iter().filter_map(|e| match e { Event::Order(o) if o.user == user => Some((o.oid, o.status)), _ => None }).collect()
}

fn szi(s: &State, user: hc_core::types::Address) -> i64 {
    s.users.get(&user).and_then(|u| u.positions.get(&0)).map_or(0, |p| p.szi)
}

/// Entry with TP and SL in one action. The SL's condition already holds, so it
/// fires at the end of the block and closes the position; the TP is then
/// cancelled as its sibling (one-cancels-the-other).
#[test]
fn stop_loss_fires_and_cancels_take_profit() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kc, _) = user("carol");
    let (kd, _) = user("dave");
    s.apply_block(
        1,
        T0 + 100,
        &[
            sg.orders(&kc, vec![order(0, false, "100000", "0.1", Tif::Gtc)]),
            sg.orders(&kd, vec![order(0, true, "94000", "0.1", Tif::Gtc)]),
        ],
    );
    // Mark is 100,000: a sell SL at 100,500 fires (mark <= trigger); the TP at 110,000 does not.
    let out = s.apply_block(
        2,
        T0 + 200,
        &[sg.orders_tpsl(&ka, vec![order(0, true, "100000", "0.1", Tif::Gtc), trigger(false, "110000", "0.1", Tpsl::Tp), trigger(false, "100500", "0.1", Tpsl::Sl)])],
    );
    let st = statuses(&out.responses[0].2);
    assert!(matches!(st[0], OrderResponse::Filled { .. }), "{st:?}");
    let (tp, sl) = match (&st[1], &st[2]) {
        (OrderResponse::Resting { oid: tp }, OrderResponse::Resting { oid: sl }) => (*tp, *sl),
        other => panic!("{other:?}"),
    };
    let ev = statuses_of(&out, a);
    assert!(ev.contains(&(sl, OrderStatus::Triggered)), "{ev:?}");
    // The entry's fill is in the transaction's span; the stop loss fired at
    // end of block, so its events are outside every span.
    assert_eq!(out.spans.len(), 1);
    let (h, start, end) = out.spans[0];
    assert_eq!(h, out.responses[0].0);
    let in_tx = &out.events[start as usize..end as usize];
    assert!(in_tx.iter().any(|e| matches!(e, Event::Fill(f) if f.user == a)));
    assert!(!in_tx.iter().any(|e| matches!(e, Event::Order(o) if o.status == OrderStatus::Triggered)));
    assert!(out.events[end as usize..].iter().any(|e| matches!(e, Event::Order(o) if o.oid == sl && o.status == OrderStatus::Triggered)));
    assert!(ev.contains(&(tp, OrderStatus::SiblingFilledCanceled)), "{ev:?}");
    assert_eq!(szi(&s, a), 0, "stop loss closed the position into dave's bid");
    assert!(s.books[0].triggers.is_empty());
    assert!(s.users[&a].open_orders.is_empty());
}

/// TP/SL attached to an entry that rests: they wait while it rests, and are
/// cancelled when it is cancelled unfilled.
#[test]
fn attached_tpsl_waits_for_entry_and_follows_its_cancel() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let out = s.apply_block(
        1,
        T0 + 100,
        &[sg.orders_tpsl(&ka, vec![order(0, true, "99000", "0.1", Tif::Gtc), trigger(false, "120000", "0.1", Tpsl::Tp), trigger(false, "100500", "0.1", Tpsl::Sl)])],
    );
    let st = statuses(&out.responses[0].2);
    let entry = match st[0] {
        OrderResponse::Resting { oid } => oid,
        ref other => panic!("{other:?}"),
    };
    // The SL condition holds, but there is no position yet: both wait.
    assert_eq!(s.books[0].triggers.len(), 2);
    let out = s.apply_block(2, T0 + 200, &[]);
    assert!(statuses_of(&out, a).is_empty());
    // Cancel the entry: its TP/SL go with it.
    let out = s.apply_block(3, T0 + 300, &[sg.cancel(&ka, 0, entry)]);
    let ev = statuses_of(&out, a);
    assert_eq!(ev.iter().filter(|(_, st)| *st == OrderStatus::SiblingFilledCanceled).count(), 2, "{ev:?}");
    assert!(s.books[0].triggers.is_empty());
    // A rejected entry rejects its TP/SL.
    let out = s.apply_block(4, T0 + 400, &[sg.orders_tpsl(&ka, vec![order(0, true, "99000", "0.00001", Tif::Gtc), trigger(false, "120000", "0.1", Tpsl::Tp)])]);
    let st = statuses(&out.responses[0].2);
    assert!(matches!(&st[1], OrderResponse::Error(e) if e.contains("Entry order was rejected")), "{st:?}");
}

#[test]
fn risk_max_position_and_leverage() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kc, _) = user("carol");
    let limits = Action::SetRiskLimits { max_position_usd: Some("15000".into()), max_leverage: Some(5), daily_loss_usd: None, close_at_margin_pct: None };
    let out = s.apply_block(1, T0 + 100, &[sg.tx(&ka, limits), sg.orders(&kc, vec![order(0, false, "100000", "1", Tif::Gtc)])]);
    assert_eq!(out.responses[0].2, TxResponse::Default);
    // 0.1 BTC (10k) is fine; another 0.1 would make 20k > 15k.
    let out = s.apply_block(2, T0 + 200, &[sg.orders(&ka, vec![order(0, true, "100000", "0.1", Tif::Ioc)])]);
    assert!(matches!(statuses(&out.responses[0].2)[0], OrderResponse::Filled { .. }));
    let out = s.apply_block(3, T0 + 300, &[sg.orders(&ka, vec![order(0, true, "100000", "0.1", Tif::Ioc)])]);
    assert!(matches!(&statuses(&out.responses[0].2)[0], OrderResponse::Error(e) if e.contains("above your limit")));
    // Leverage above the cap is refused; at the cap it is fine.
    let out = s.apply_block(4, T0 + 400, &[
        sg.tx(&ka, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 10 }),
        sg.tx(&ka, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 5 }),
    ]);
    assert!(matches!(&out.responses[0].2, TxResponse::Error { error } if error.contains("risk limit of 5x")));
    assert_eq!(out.responses[1].2, TxResponse::Default);
    assert_eq!(s.users[&a].leverage.get(&0), Some(&5));
}

/// Losing more than the daily limit halts new exposure; reducing still works.
#[test]
fn risk_daily_loss_halts_until_reduced() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kc, _) = user("carol");
    let (kd, _) = user("dave");
    let limits = Action::SetRiskLimits { max_position_usd: None, max_leverage: None, daily_loss_usd: Some("100".into()), close_at_margin_pct: None };
    s.apply_block(1, T0 + 100, &[
        sg.tx(&ka, limits),
        sg.orders(&kc, vec![order(0, false, "100000", "0.2", Tif::Gtc)]),
        sg.orders(&kd, vec![order(0, true, "99000", "0.2", Tif::Gtc)]),
    ]);
    // Buy 0.2 at 100k, sell 0.1 at 99k: about 100 realized loss plus fees.
    let out = s.apply_block(2, T0 + 200, &[
        sg.orders(&ka, vec![order(0, true, "100000", "0.2", Tif::Ioc)]),
        sg.orders(&ka, vec![order(0, false, "99000", "0.1", Tif::Ioc)]),
    ]);
    assert!(out.events.iter().any(|e| matches!(e, Event::Risk { user, kind: RiskKind::DailyLossHalt, .. } if *user == a)));
    assert!(s.users[&a].risk.as_ref().unwrap().halted);
    // New exposure is refused; reducing the position is accepted.
    let out = s.apply_block(3, T0 + 300, &[sg.orders(&ka, vec![order(0, true, "100000", "0.01", Tif::Gtc)])]);
    assert!(matches!(&statuses(&out.responses[0].2)[0], OrderResponse::Error(e) if e.contains("Daily loss limit")));
    let out = s.apply_block(4, T0 + 400, &[sg.orders(&ka, vec![order(0, false, "99000", "0.1", Tif::Ioc)])]);
    assert!(matches!(statuses(&out.responses[0].2)[0], OrderResponse::Filled { .. }));
    assert_eq!(szi(&s, a), 0);
    // A new UTC day clears the halt.
    s.apply_block(5, T0 + 86_400_000, &[]);
    assert!(!s.users[&a].risk.as_ref().unwrap().halted);
}

/// Maintenance margin reaching the set share of account value closes positions.
#[test]
fn risk_auto_close_on_margin() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ke, e) = user("erin");
    let (kc, _) = user("carol");
    let (kd, _) = user("dave");
    // Erin has 10k; 1 BTC at 100k uses about 1.25k maintenance margin (12.5%).
    s.apply_block(1, T0 + 100, &[
        sg.tx(&ke, Action::Faucet { usd: "10000".into() }),
        sg.orders(&kc, vec![order(0, false, "100000", "1", Tif::Gtc)]),
        sg.orders(&kd, vec![order(0, true, "99900", "1", Tif::Gtc)]),
    ]);
    let limits = Action::SetRiskLimits { max_position_usd: None, max_leverage: None, daily_loss_usd: None, close_at_margin_pct: Some(10) };
    let out = s.apply_block(2, T0 + 200, &[sg.tx(&ke, limits), sg.orders(&ke, vec![order(0, true, "100000", "1", Tif::Ioc)])]);
    assert!(matches!(statuses(&out.responses[1].2)[0], OrderResponse::Filled { .. }), "{:?}", out.responses[1].2);
    assert!(out.events.iter().any(|ev| matches!(ev, Event::Risk { user, kind: RiskKind::MarginAutoClose, .. } if *user == e)));
    assert_eq!(szi(&s, e), 0, "auto-close sold into dave's bid");
}
