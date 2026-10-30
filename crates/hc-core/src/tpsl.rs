//! Trigger (TP/SL) orders.
//!
//! A trigger order rests off the book. At the end of every block, after the
//! mark price update, each one is checked in oid order:
//! - A reduce-only trigger needs a position it can reduce. Without one it
//!   waits while its entry order (placed in the same action) still rests, and
//!   is otherwise cancelled: the position was closed, for example by the
//!   sibling TP or SL, or the entry was cancelled unfilled.
//! - When the mark price crosses the trigger it is removed and sent as a market
//!   order (IOC at the mark plus slippage) or as a GTC limit order at its price.
//!
//! Hyperliquid triggers TP/SL on the mark price with a 10% market slippage
//! (public documentation); both are used here.

use crate::action::{OrderWire, TriggerWire};
use crate::book::TriggerOrder;
use crate::events::{Event, OrderEvent, OrderResponse};
use crate::exchange::{OrderReq, Origin, State};
use crate::fixed::{self, ntl, parse_decimal, Px};
use crate::types::{Address, AssetId, Oid, OrderStatus, Side, Tif};

/// Slippage of a triggered market order, in percent of the mark price.
const TRIGGER_SLIPPAGE_PCT: u64 = 10;

impl TriggerOrder {
    pub fn event(&self, status: OrderStatus, now: u64) -> OrderEvent {
        OrderEvent {
            user: self.user,
            asset: self.asset,
            oid: self.oid,
            side: self.side,
            limit_px: self.limit_px,
            sz: self.sz,
            orig_sz: self.sz,
            cloid: self.cloid,
            timestamp: self.timestamp,
            status,
            status_timestamp: now,
        }
    }
}

impl State {
    /// Places a trigger order from an `order` action.
    pub(crate) fn place_trigger(&mut self, user: Address, o: &OrderWire, t: &TriggerWire, parent: Option<Oid>, events: &mut Vec<Event>) -> OrderResponse {
        let req = match self.parse_order(o).map_err(|e| e.1) {
            Ok(req) => req,
            Err(msg) => return OrderResponse::Error(msg),
        };
        let szd = self.metas[req.asset as usize].sz_decimals;
        let Some(trigger_px) = parse_decimal(&t.trigger_px, 6).filter(|v| *v > 0).and_then(|v| u64::try_from(v).ok()).map(Px) else {
            return OrderResponse::Error("Invalid trigger price.".into());
        };
        if !fixed::is_valid_px(trigger_px, szd) {
            return OrderResponse::Error("Trigger price must be divisible by tick size.".into());
        }
        if !req.reduce_only {
            if ntl(trigger_px, req.lots as i64, szd) < self.params.min_order_ntl {
                return OrderResponse::Error(format!("Order must have minimum value of ${}.", self.params.min_order_ntl));
            }
            if let Err(msg) = self.risk_check(user, req.asset, req.side, trigger_px, req.lots) {
                return OrderResponse::Error(msg);
            }
        }
        let n_open = self.users.get(&user).map(|u| u.open_orders.len()).unwrap_or(0);
        if n_open >= self.params.max_open_orders {
            return OrderResponse::Error("Too many open orders".into());
        }
        let oid = self.next_oid;
        self.next_oid += 1;
        let order = TriggerOrder {
            oid,
            user,
            asset: req.asset,
            side: req.side,
            trigger_px,
            limit_px: req.px,
            is_market: t.is_market,
            tpsl: t.tpsl,
            sz: req.lots,
            reduce_only: req.reduce_only,
            cloid: req.cloid,
            parent,
            timestamp: self.time_ms,
        };
        events.push(Event::Order(order.event(OrderStatus::Open, self.time_ms)));
        self.books[req.asset as usize].triggers.insert(oid, order);
        self.users.entry(user).or_default().open_orders.insert((req.asset, oid));
        OrderResponse::Resting { oid }
    }

    pub(crate) fn remove_trigger(&mut self, asset: AssetId, oid: Oid, status: OrderStatus, events: &mut Vec<Event>) -> Option<TriggerOrder> {
        let t = self.books[asset as usize].triggers.remove(&oid)?;
        if let Some(u) = self.users.get_mut(&t.user) {
            u.open_orders.remove(&(asset, oid));
        }
        events.push(Event::Order(t.event(status, self.time_ms)));
        Some(t)
    }

    /// End of block: fire, cancel or keep waiting each trigger order. A pass
    /// repeats after anything fired, so a TP/SL whose sibling just closed the
    /// position is cancelled in the same block.
    pub(crate) fn run_triggers(&mut self, events: &mut Vec<Event>) {
        for a in 0..self.books.len() {
            while self.trigger_pass(a, events) {}
        }
    }

    /// One pass over an asset's triggers in oid order; true if any fired.
    fn trigger_pass(&mut self, a: usize, events: &mut Vec<Event>) -> bool {
        let mut fired = false;
        {
            let asset = a as AssetId;
            let oids: Vec<Oid> = self.books[a].triggers.keys().copied().collect();
            for oid in oids {
                // Earlier triggers in this pass may have changed positions.
                let Some(t) = self.books[a].triggers.get(&oid).cloned() else { continue };
                if t.reduce_only {
                    let szi = self.users.get(&t.user).and_then(|u| u.positions.get(&asset)).map_or(0, |p| p.szi);
                    let reducible = (szi > 0 && t.side == Side::Ask) || (szi < 0 && t.side == Side::Bid);
                    if !reducible {
                        let entry_resting = t.parent.is_some_and(|p| self.books[a].orders.contains_key(&p));
                        if !entry_resting {
                            self.remove_trigger(asset, oid, OrderStatus::SiblingFilledCanceled, events);
                        }
                        continue;
                    }
                }
                let mark = self.assets[a].prices.mark_px;
                if !t.fires(mark) {
                    continue;
                }
                self.remove_trigger(asset, oid, OrderStatus::Triggered, events);
                let szd = self.metas[a].sz_decimals;
                let (px, tif) = if t.is_market {
                    let slip = mark.0 as i128 * TRIGGER_SLIPPAGE_PCT as i128 / 100;
                    let raw = if t.side == Side::Bid { mark.0 as i128 + slip } else { mark.0 as i128 - slip };
                    (fixed::round_px(raw.max(1), szd, t.side == Side::Bid), Tif::Ioc)
                } else {
                    (t.limit_px, Tif::Gtc)
                };
                let req = OrderReq { asset, side: t.side, px, lots: t.sz, reduce_only: t.reduce_only, tif, cloid: t.cloid };
                self.place(t.user, req, Origin::Trigger, events);
                fired = true;
            }
        }
        fired
    }
}
