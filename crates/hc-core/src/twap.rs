//! TWAP orders: a large order executed as market slices over time.
//!
//! Following the public documentation, a TWAP sends one IOC suborder every
//! 30 seconds for its duration, each at most 3% from the mark price. A slice
//! that cannot fill fully (thin book, slippage limit) leaves the rest to later
//! slices, which may catch up by up to 3× the normal slice size. With
//! `randomize`, each slice's size varies by up to ±20% (from a hash of the
//! TWAP id and slice number, so every validator draws the same sizes).
//! The TWAP finishes after its last slice, filled or not.

use crate::events::{Event, OrderResponse, TwapEvent, TwapStatus, TxResponse};
use crate::exchange::{OrderReq, Origin, State};
use crate::fixed::{self, ntl, parse_decimal};
use crate::types::{Address, AssetId, Side, Tif};
use serde::{Deserialize, Serialize};

/// Time between slices.
pub const SLICE_MS: u64 = 30_000;
/// Slice price limit, in percent of the mark price.
const SLIPPAGE_PCT: u64 = 3;
/// A slice may catch up to this many normal slices.
const MAX_CATCH_UP: u64 = 3;
/// Longest TWAP, in minutes (one day).
pub const MAX_MINUTES: u32 = 1_440;

/// `{"a":0,"b":true,"s":"1.5","r":false,"m":10,"t":false}`: asset, is buy,
/// size, reduce only, minutes, randomize.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwapWire {
    pub a: AssetId,
    pub b: bool,
    pub s: String,
    pub r: bool,
    pub m: u32,
    pub t: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Twap {
    pub id: u64,
    pub user: Address,
    pub asset: AssetId,
    pub side: Side,
    /// Total size and size filled so far, in lots.
    pub sz: u64,
    pub filled: u64,
    /// Notional filled so far (µUSDC), for the average price.
    pub filled_ntl: i64,
    pub reduce_only: bool,
    pub randomize: bool,
    pub minutes: u32,
    pub start_ms: u64,
    /// Slices sent so far.
    pub slices_done: u64,
}

impl Twap {
    pub fn n_slices(&self) -> u64 {
        (self.minutes as u64 * 60_000 / SLICE_MS).max(1)
    }

    pub fn event(&self, status: TwapStatus) -> Event {
        Event::Twap(TwapEvent { id: self.id, user: self.user, asset: self.asset, side: self.side, sz: self.sz, filled: self.filled, filled_ntl: self.filled_ntl, minutes: self.minutes, status })
    }

    /// Size of slice `k` (1-based): the share of the total due by then,
    /// minus what is filled, capped at `MAX_CATCH_UP` normal slices.
    fn slice_size(&self, k: u64) -> u64 {
        let n = self.n_slices();
        let mut due = (self.sz as u128 * k as u128 / n as u128) as u64;
        if self.randomize && k < n {
            // ±20% of one slice, the same on every validator.
            let h = blake3::hash(&[self.id.to_be_bytes(), k.to_be_bytes()].concat());
            let r = u64::from_be_bytes(h.as_bytes()[..8].try_into().expect("8 bytes")) % 41;
            let slice = self.sz / n;
            due = (due + slice * r / 100).saturating_sub(slice * 20 / 100).min(self.sz);
        }
        due.saturating_sub(self.filled).min((self.sz / n).max(1) * MAX_CATCH_UP)
    }
}

impl State {
    pub(crate) fn twap_order(&mut self, user: Address, w: &TwapWire, events: &mut Vec<Event>) -> TxResponse {
        let Some(meta) = self.metas.get(w.a as usize) else { return TxResponse::err("Unknown asset.") };
        let Some(sz) = parse_decimal(&w.s, meta.sz_decimals).filter(|v| *v > 0).and_then(|v| u64::try_from(v).ok()) else {
            return TxResponse::err("Invalid TWAP size.");
        };
        if w.m == 0 || w.m > MAX_MINUTES {
            return TxResponse::err(format!("TWAP duration must be 1 to {MAX_MINUTES} minutes."));
        }
        let mark = self.assets[w.a as usize].prices.mark_px;
        let n = (w.m as u64 * 60_000 / SLICE_MS).max(1);
        if !w.r && ntl(mark, (sz / n) as i64, meta.sz_decimals) < self.params.min_order_ntl {
            return TxResponse::err(format!("TWAP suborders must be worth at least ${}; use a larger size or a shorter duration.", self.params.min_order_ntl));
        }
        let id = self.next_twap_id;
        self.next_twap_id += 1;
        let t = Twap {
            id,
            user,
            asset: w.a,
            side: Side::from_is_buy(w.b),
            sz,
            filled: 0,
            filled_ntl: 0,
            reduce_only: w.r,
            randomize: w.t,
            minutes: w.m,
            start_ms: self.time_ms,
            slices_done: 0,
        };
        events.push(t.event(TwapStatus::Activated));
        self.twaps.insert(id, t);
        TxResponse::Twap { twap_id: id }
    }

    pub(crate) fn twap_cancel(&mut self, user: Address, asset: AssetId, id: u64, events: &mut Vec<Event>) -> TxResponse {
        match self.twaps.get(&id) {
            Some(t) if t.user == user && t.asset == asset => {
                let t = self.twaps.remove(&id).expect("present");
                events.push(t.event(TwapStatus::Canceled));
                TxResponse::Default
            }
            _ => TxResponse::err("TWAP was never placed, already canceled, or finished."),
        }
    }

    /// End of block: send every slice that is due, in TWAP id order.
    pub(crate) fn run_twaps(&mut self, events: &mut Vec<Event>) {
        let ids: Vec<u64> = self.twaps.keys().copied().collect();
        for id in ids {
            let mut t = self.twaps[&id].clone();
            let n = t.n_slices();
            // Slice k is due at start + k·30 s; a late block sends only the latest due slice.
            let due = ((self.time_ms - t.start_ms) / SLICE_MS).min(n);
            if due <= t.slices_done {
                continue;
            }
            t.slices_done = due;
            let lots = t.slice_size(due);
            if lots > 0 {
                let szd = self.metas[t.asset as usize].sz_decimals;
                let mark = self.assets[t.asset as usize].prices.mark_px;
                let slip = mark.0 as i128 * SLIPPAGE_PCT as i128 / 100;
                let raw = if t.side == Side::Bid { mark.0 as i128 + slip } else { mark.0 as i128 - slip };
                let px = fixed::round_px(raw.max(1), szd, t.side == Side::Bid);
                let req = OrderReq { asset: t.asset, side: t.side, px, lots, reduce_only: t.reduce_only, tif: Tif::Ioc, cloid: None };
                if let OrderResponse::Filled { total_sz, avg_px, .. } = self.place(t.user, req, Origin::Trigger, events) {
                    t.filled += total_sz;
                    t.filled_ntl += ntl(avg_px, total_sz as i64, szd).0;
                }
            }
            if due == n || t.filled >= t.sz {
                self.twaps.remove(&id);
                events.push(t.event(TwapStatus::Finished));
            } else {
                events.push(t.event(TwapStatus::Running));
                self.twaps.insert(id, t);
            }
        }
    }
}
