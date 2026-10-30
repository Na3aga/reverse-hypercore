//! Price-time priority limit order book for one asset.
//!
//! Each price level is a FIFO queue of order ids. Bids iterate from the highest
//! price and asks from the lowest. All containers are ordered, so iteration
//! order (and therefore matching) is deterministic.

use crate::fixed::Px;
use crate::types::{Address, AssetId, Cloid, Oid, Side, Tif, Tpsl};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestingOrder {
    pub oid: Oid,
    pub user: Address,
    pub asset: AssetId,
    pub side: Side,
    pub px: Px,
    /// Remaining size in lots.
    pub sz: u64,
    pub orig_sz: u64,
    pub reduce_only: bool,
    pub tif: Tif,
    pub cloid: Option<Cloid>,
    /// Block time at placement (ms).
    pub timestamp: u64,
}

/// A TP/SL order waiting off the book for the mark price to cross its trigger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerOrder {
    pub oid: Oid,
    pub user: Address,
    pub asset: AssetId,
    pub side: Side,
    pub trigger_px: Px,
    /// Limit price once triggered (ignored for market triggers, which use the
    /// mark price plus slippage).
    pub limit_px: Px,
    pub is_market: bool,
    pub tpsl: Tpsl,
    /// Size in lots; reduce-only triggers are clipped to the position when they fire.
    pub sz: u64,
    pub reduce_only: bool,
    pub cloid: Option<Cloid>,
    /// Entry order placed in the same action. The trigger waits while the
    /// entry rests unfilled and is cancelled if it is cancelled unfilled.
    pub parent: Option<Oid>,
    pub timestamp: u64,
}

impl TriggerOrder {
    /// Whether the mark price meets the trigger condition. A sell TP fires at
    /// or above the trigger and a sell SL at or below; buys are mirrored.
    pub fn fires(&self, mark: Px) -> bool {
        match (self.side, self.tpsl) {
            (Side::Ask, Tpsl::Tp) | (Side::Bid, Tpsl::Sl) => mark >= self.trigger_px,
            (Side::Ask, Tpsl::Sl) | (Side::Bid, Tpsl::Tp) => mark <= self.trigger_px,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    pub bids: BTreeMap<Px, VecDeque<Oid>>,
    pub asks: BTreeMap<Px, VecDeque<Oid>>,
    pub orders: BTreeMap<Oid, RestingOrder>,
    /// Resting orders that carry a client order id, for cancel-by-cloid
    /// without scanning the book. A user may reuse a cloid; lookups take the
    /// oldest matching order.
    pub cloids: BTreeSet<(Address, Cloid, Oid)>,
    /// Untriggered TP/SL orders, by oid.
    pub triggers: BTreeMap<Oid, TriggerOrder>,
}

impl Book {
    fn side_mut(&mut self, side: Side) -> &mut BTreeMap<Px, VecDeque<Oid>> {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    pub fn best_bid(&self) -> Option<Px> {
        self.bids.keys().next_back().copied()
    }

    pub fn best_ask(&self) -> Option<Px> {
        self.asks.keys().next().copied()
    }

    /// Best resting price on `side`.
    pub fn best(&self, side: Side) -> Option<Px> {
        match side {
            Side::Bid => self.best_bid(),
            Side::Ask => self.best_ask(),
        }
    }

    /// Front order of the best level on `side` (the next maker to match).
    pub fn front(&self, side: Side) -> Option<Oid> {
        let level = match side {
            Side::Bid => self.bids.iter().next_back(),
            Side::Ask => self.asks.iter().next(),
        };
        level.and_then(|(_, q)| q.front().copied())
    }

    /// Would an order on `side` at `px` cross the opposite side?
    pub fn crosses(&self, side: Side, px: Px) -> bool {
        match side {
            Side::Bid => self.best_ask().is_some_and(|a| px >= a),
            Side::Ask => self.best_bid().is_some_and(|b| px <= b),
        }
    }

    /// Appends at the back of its price level (time priority).
    pub fn insert(&mut self, order: RestingOrder) {
        self.side_mut(order.side).entry(order.px).or_default().push_back(order.oid);
        if let Some(c) = order.cloid {
            self.cloids.insert((order.user, c, order.oid));
        }
        self.orders.insert(order.oid, order);
    }

    pub fn remove(&mut self, oid: Oid) -> Option<RestingOrder> {
        let order = self.orders.remove(&oid)?;
        if let Some(c) = order.cloid {
            self.cloids.remove(&(order.user, c, oid));
        }
        let levels = self.side_mut(order.side);
        if let Some(q) = levels.get_mut(&order.px) {
            q.retain(|o| *o != oid);
            if q.is_empty() {
                levels.remove(&order.px);
            }
        }
        Some(order)
    }

    /// The oldest resting order of `user` with client order id `cloid`.
    pub fn find_cloid(&self, user: Address, cloid: Cloid) -> Option<Oid> {
        self.cloids.range((user, cloid, Oid::MIN)..=(user, cloid, Oid::MAX)).next().map(|(_, _, oid)| *oid)
    }

    /// Reduces a resting order's size in place, keeping its queue position.
    /// Removes it when the size reaches zero.
    pub fn reduce(&mut self, oid: Oid, by: u64) {
        let done = {
            let o = self.orders.get_mut(&oid).expect("order exists");
            o.sz -= by;
            o.sz == 0
        };
        if done {
            self.remove(oid);
        }
    }

    /// Aggregated levels `(px, total_sz, n_orders)`, best first, up to `depth`.
    pub fn levels(&self, side: Side, depth: usize) -> Vec<(Px, u64, usize)> {
        let agg = |(px, q): (&Px, &VecDeque<Oid>)| {
            (*px, q.iter().map(|o| self.orders[o].sz).sum(), q.len())
        };
        match side {
            Side::Bid => self.bids.iter().rev().take(depth).map(agg).collect(),
            Side::Ask => self.asks.iter().take(depth).map(agg).collect(),
        }
    }

    /// Average execution price for sweeping `ntl_target` (in µUSD-notional,
    /// i.e. `px * lots / 10^sz_decimals`) from the side opposite `taker_side`.
    /// Returns `None` if the book is too thin. Used for funding impact prices.
    pub fn impact_px(&self, taker_side: Side, ntl_target: i128, sz_decimals: u32) -> Option<Px> {
        let scale = crate::fixed::pow10(sz_decimals);
        let mut ntl: i128 = 0;
        let mut lots: i128 = 0;
        for (px, sz, _) in self.levels(taker_side.opposite(), usize::MAX) {
            let lvl_ntl = px.0 as i128 * sz as i128 / scale;
            if ntl + lvl_ntl >= ntl_target {
                // Take just enough lots at this level to reach the target.
                let need = ntl_target - ntl;
                let take = (need * scale + px.0 as i128 - 1) / px.0 as i128;
                ntl += px.0 as i128 * take / scale;
                lots += take;
                return Some(Px((ntl * scale / lots) as u64));
            }
            ntl += lvl_ntl;
            lots += sz as i128;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(oid: Oid, side: Side, px: u64, sz: u64) -> RestingOrder {
        RestingOrder {
            oid,
            user: Address::default(),
            asset: 0,
            side,
            px: Px(px),
            sz,
            orig_sz: sz,
            reduce_only: false,
            tif: Tif::Gtc,
            cloid: None,
            timestamp: 0,
        }
    }

    #[test]
    fn price_time_priority() {
        let mut b = Book::default();
        b.insert(o(1, Side::Bid, 100, 5));
        b.insert(o(2, Side::Bid, 101, 5));
        b.insert(o(3, Side::Bid, 101, 5));
        b.insert(o(4, Side::Ask, 103, 5));
        assert_eq!(b.front(Side::Bid), Some(2));
        b.reduce(2, 5);
        assert_eq!(b.front(Side::Bid), Some(3));
        assert!(b.crosses(Side::Ask, Px(101)));
        assert!(!b.crosses(Side::Bid, Px(102)));
        assert_eq!(b.levels(Side::Bid, 10), vec![(Px(101), 5, 1), (Px(100), 5, 1)]);
    }

    #[test]
    fn impact_price() {
        let mut b = Book::default();
        // 2 coins at 100 and 2 coins at 110, sz_decimals = 0, px in µUSD.
        b.insert(o(1, Side::Ask, 100_000_000, 2));
        b.insert(o(2, Side::Ask, 110_000_000, 2));
        // Buy 300 USD notional: 2 @100 + 1 @110 = 310 / 3 lots
        let px = b.impact_px(Side::Bid, 300_000_000, 0).unwrap();
        assert_eq!(px, Px(103_333_333));
        assert_eq!(b.impact_px(Side::Bid, 1_000_000_000, 0), None);
    }
}
