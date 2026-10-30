//! TWAP orders: slices every 30 s, catch-up after thin liquidity, cancel.

mod common;

use common::*;
use hc_core::events::{Event, TwapStatus, TxResponse};
use hc_core::twap::TwapWire;
use hc_core::types::Tif;
use hc_core::{Action, State};

fn twap_events(events: &[Event]) -> Vec<(TwapStatus, u64)> {
    events.iter().filter_map(|e| match e { Event::Twap(t) => Some((t.status, t.filled)), _ => None }).collect()
}

#[test]
fn twap_buys_in_slices_and_catches_up() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kb, _) = user("bob");
    // Bob offers 0.5 BTC at 100k; Alice buys 1 BTC over 2 minutes: 4 slices of 0.25.
    s.apply_block(1, T0 + 1, &[sg.orders(&kb, vec![order(0, false, "100000", "0.5", Tif::Gtc)])]);
    let w = TwapWire { a: 0, b: true, s: "1".into(), r: false, m: 2, t: false };
    let out = s.apply_block(2, T0 + 2, &[sg.tx(&ka, Action::TwapOrder { twap: w })]);
    assert!(matches!(out.responses[0].2, TxResponse::Twap { .. }), "{:?}", out.responses);
    assert_eq!(twap_events(&out.events), vec![(TwapStatus::Activated, 0)]);

    // Nothing before 30 s; then one slice per 30 s.
    assert!(twap_events(&s.apply_block(3, T0 + 20_000, &[]).events).is_empty());
    let out = s.apply_block(4, T0 + 30_002, &[]);
    assert_eq!(twap_events(&out.events), vec![(TwapStatus::Running, 25_000)]);
    let out = s.apply_block(5, T0 + 60_002, &[]);
    assert_eq!(twap_events(&out.events), vec![(TwapStatus::Running, 50_000)]);
    // The book is empty now: slice 3 fills nothing.
    let out = s.apply_block(6, T0 + 90_002, &[]);
    assert_eq!(twap_events(&out.events), vec![(TwapStatus::Running, 50_000)]);
    // Liquidity returns: the last slice catches up (0.5 BTC, under 3 slices) and the TWAP ends.
    s.apply_block(7, T0 + 100_000, &[sg.orders(&kb, vec![order(0, false, "100500", "1", Tif::Gtc)])]);
    let out = s.apply_block(8, T0 + 120_002, &[]);
    assert_eq!(twap_events(&out.events), vec![(TwapStatus::Finished, 100_000)]);
    assert_eq!(s.users[&a].positions[&0].szi, 100_000);
    assert!(s.twaps.is_empty());
}

#[test]
fn twap_cancel_and_validation() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, _) = user("alice");
    let (kb, _) = user("bob");
    // Suborders under $10 are rejected: 0.001 BTC over 60 minutes is $0.83 per slice.
    let tiny = TwapWire { a: 0, b: true, s: "0.001".into(), r: false, m: 60, t: false };
    let out = s.apply_block(1, T0 + 1, &[sg.tx(&ka, Action::TwapOrder { twap: tiny })]);
    assert!(matches!(&out.responses[0].2, TxResponse::Error { error } if error.contains("at least")), "{:?}", out.responses);

    let w = TwapWire { a: 0, b: false, s: "1".into(), r: false, m: 10, t: true };
    let out = s.apply_block(2, T0 + 2, &[sg.tx(&ka, Action::TwapOrder { twap: w })]);
    let TxResponse::Twap { twap_id } = out.responses[0].2 else { panic!("{:?}", out.responses) };
    // Only the owner can cancel it.
    let out = s.apply_block(3, T0 + 3, &[sg.tx(&kb, Action::TwapCancel { a: 0, t: twap_id })]);
    assert!(matches!(out.responses[0].2, TxResponse::Error { .. }));
    let out = s.apply_block(4, T0 + 4, &[sg.tx(&ka, Action::TwapCancel { a: 0, t: twap_id })]);
    assert!(matches!(out.responses[0].2, TxResponse::Default));
    assert_eq!(twap_events(&out.events), vec![(TwapStatus::Canceled, 0)]);
    assert!(s.twaps.is_empty());
}

#[test]
fn randomized_slices_still_add_up() {
    let mut s = State::genesis(&genesis());
    let mut sg = Signer::new();
    let (ka, a) = user("alice");
    let (kb, _) = user("bob");
    s.apply_block(1, T0 + 1, &[sg.orders(&kb, vec![order(0, false, "100000", "5", Tif::Gtc)])]);
    let w = TwapWire { a: 0, b: true, s: "2".into(), r: false, m: 5, t: true };
    s.apply_block(2, T0 + 2, &[sg.tx(&ka, Action::TwapOrder { twap: w })]);
    let mut sizes = Vec::new();
    for k in 1..=10u64 {
        let out = s.apply_block(2 + k, T0 + 2 + k * 30_000, &[]);
        sizes.extend(out.events.iter().filter_map(|e| match e { Event::Fill(f) if f.user == a => Some(f.sz), _ => None }));
    }
    assert_eq!(sizes.iter().sum::<u64>(), 200_000);
    assert!(sizes.iter().any(|z| *z != 20_000), "slice sizes vary: {sizes:?}");
}
