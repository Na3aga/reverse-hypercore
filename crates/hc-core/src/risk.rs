//! Per-account risk limits, set by the account itself and enforced by the engine.
//!
//! - **Max position (USD):** an order that would take a position past this
//!   notional is rejected. Orders that reduce a position always pass.
//! - **Max leverage:** leverage changes above it are rejected. Setting the
//!   limit also lowers existing leverage settings above it, if margin allows.
//! - **Daily loss:** at the end of each block, if account value is more than
//!   this below its value at 00:00 UTC, the account is halted until the next
//!   UTC day: only orders that reduce a position are accepted. Faucet and
//!   bridge transfers move the baseline with them, so moving funds is not a loss.
//! - **Close at margin %:** at the end of each block, before liquidations, if
//!   maintenance margin reaches this percent of account value, every position
//!   is closed with a reduce-only market order. Liquidation happens at 100%.

use crate::events::{Event, RiskKind, TxResponse};
use crate::exchange::{OrderReq, Origin, State};
use crate::fixed::{self, ntl, parse_decimal, Px, Usd};
use crate::types::{Address, AssetId, Side, Tif};
use serde::{Deserialize, Serialize};

const DAY_MS: u64 = 86_400_000;
/// Slippage of a risk auto-close market order, in percent of the mark price.
const CLOSE_SLIPPAGE_PCT: u64 = 10;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskState {
    pub max_position_usd: Option<Usd>,
    pub max_leverage: Option<u32>,
    pub daily_loss_usd: Option<Usd>,
    pub close_at_margin_pct: Option<u32>,
    /// UTC day (`time / 1 day`) that `day_start_value` belongs to.
    pub day: u64,
    /// Account value at the start of `day`, adjusted for transfers since.
    pub day_start_value: Usd,
    /// Daily loss limit reached: only reducing orders until the day ends.
    pub halted: bool,
}

fn parse_usd(s: Option<&str>, what: &str) -> Result<Option<Usd>, String> {
    s.map(|s| parse_decimal(s, 6).filter(|v| *v > 0).and_then(|v| i64::try_from(v).ok()).map(Usd).ok_or(format!("Invalid {what}.")))
        .transpose()
}

impl State {
    /// Total account value (cross and isolated), so moving margin into an
    /// isolated position is not counted as a loss.
    fn account_value(&self, user: Address) -> Usd {
        self.with_margin_ctx(|ctx| self.users.get(&user).map(|u| u.total_account_value(ctx)).unwrap_or_default())
    }

    pub(crate) fn set_risk_limits(
        &mut self,
        user: Address,
        max_position_usd: Option<&str>,
        max_leverage: Option<u32>,
        daily_loss_usd: Option<&str>,
        close_at_margin_pct: Option<u32>,
    ) -> TxResponse {
        let (max_position, daily_loss) = match (parse_usd(max_position_usd, "max position"), parse_usd(daily_loss_usd, "daily loss limit")) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return TxResponse::err(e),
        };
        if max_leverage == Some(0) {
            return TxResponse::err("Max leverage must be at least 1.");
        }
        if close_at_margin_pct.is_some_and(|p| p == 0 || p >= 100) {
            return TxResponse::err("Close-at-margin must be between 1 and 99 percent.");
        }
        if max_position.is_none() && max_leverage.is_none() && daily_loss.is_none() && close_at_margin_pct.is_none() {
            if let Some(u) = self.users.get_mut(&user) {
                u.risk = None;
            }
            self.risk_users.remove(&user);
            return TxResponse::Default;
        }
        let mut u = self.users.get(&user).cloned().unwrap_or_default();
        // Lowering the leverage cap lowers settings above it, if margin allows.
        if let Some(cap) = max_leverage {
            for a in 0..self.metas.len() as AssetId {
                let lev = self.with_margin_ctx(|ctx| u.leverage_for(a, ctx));
                if lev > cap {
                    u.leverage.insert(a, cap);
                }
            }
            let ok = self.with_margin_ctx(|ctx| {
                let s = u.summary(ctx);
                s.total_margin_used <= s.account_value
            });
            if !ok {
                return TxResponse::err(format!("Insufficient margin to lower leverage to {cap}x."));
            }
        }
        let today = self.time_ms / DAY_MS;
        let value = self.with_margin_ctx(|ctx| u.total_account_value(ctx));
        let mut r = u.risk.take().unwrap_or(RiskState { day: today, day_start_value: value, ..Default::default() });
        r.max_position_usd = max_position;
        r.max_leverage = max_leverage;
        r.daily_loss_usd = daily_loss;
        r.close_at_margin_pct = close_at_margin_pct;
        if daily_loss.is_none() {
            r.halted = false;
        }
        u.risk = Some(r);
        self.users.insert(user, u);
        self.risk_users.insert(user);
        TxResponse::Default
    }

    /// Pre-trade check for an order that may increase exposure.
    pub(crate) fn risk_check(&self, user: Address, asset: AssetId, side: Side, px: Px, lots: u64) -> Result<(), String> {
        let Some(u) = self.users.get(&user) else { return Ok(()) };
        let Some(r) = &u.risk else { return Ok(()) };
        let cur = u.positions.get(&asset).map_or(0, |p| p.szi);
        let new = cur + side.sign() * lots as i64;
        if new.unsigned_abs() <= cur.unsigned_abs() {
            return Ok(());
        }
        if r.halted {
            return Err("Daily loss limit reached: only orders that reduce a position are accepted until 00:00 UTC.".into());
        }
        if let Some(cap) = r.max_position_usd {
            let meta = &self.metas[asset as usize];
            let px = px.max(self.assets[asset as usize].prices.mark_px);
            let value = Usd(ntl(px, new, meta.sz_decimals).0.abs());
            if value > cap {
                return Err(format!("Order would take your {} position to ${value}, above your limit of ${cap}.", meta.name));
            }
        }
        Ok(())
    }

    /// Moves the daily-loss baseline with a transfer in (+) or out (-).
    pub(crate) fn risk_baseline(&mut self, user: Address, delta: Usd) {
        if let Some(r) = self.users.get_mut(&user).and_then(|u| u.risk.as_mut()) {
            r.day_start_value += delta;
        }
    }

    /// End of block: roll the UTC day, halt on daily loss, auto-close on margin.
    pub(crate) fn run_risk(&mut self, events: &mut Vec<Event>) {
        let today = self.time_ms / DAY_MS;
        let users: Vec<Address> = self.risk_users.iter().copied().collect();
        for user in users {
            // The daily loss counts the whole account; the margin ratio is the cross account's.
            let (value, cross_value, maintenance) = self.with_margin_ctx(|ctx| {
                let u = &self.users[&user];
                let s = u.summary(ctx);
                (u.total_account_value(ctx), s.account_value, s.maintenance_margin)
            });
            let r = self.users.get_mut(&user).and_then(|u| u.risk.as_mut()).expect("risk user has limits");
            if r.day != today {
                r.day = today;
                r.day_start_value = value;
                r.halted = false;
            }
            if let Some(limit) = r.daily_loss_usd {
                if !r.halted && value < r.day_start_value - limit {
                    r.halted = true;
                    events.push(Event::Risk { user, kind: RiskKind::DailyLossHalt, account_value: value });
                }
            }
            let close = r.close_at_margin_pct.is_some_and(|pct| maintenance.0 > 0 && maintenance.0 as i128 * 100 >= cross_value.0 as i128 * pct as i128);
            if close {
                self.close_all(user, events);
                events.push(Event::Risk { user, kind: RiskKind::MarginAutoClose, account_value: value });
            }
        }
    }

    /// Reduce-only market orders that close every position of `user`.
    fn close_all(&mut self, user: Address, events: &mut Vec<Event>) {
        let positions: Vec<(AssetId, i64)> = self.users[&user].positions.iter().map(|(a, p)| (*a, p.szi)).collect();
        for (asset, szi) in positions {
            let szd = self.metas[asset as usize].sz_decimals;
            let mark = self.assets[asset as usize].prices.mark_px;
            let side = if szi > 0 { Side::Ask } else { Side::Bid };
            let slip = mark.0 as i128 * CLOSE_SLIPPAGE_PCT as i128 / 100;
            let raw = if side == Side::Bid { mark.0 as i128 + slip } else { mark.0 as i128 - slip };
            let px = fixed::round_px(raw.max(1), szd, side == Side::Bid);
            let req = OrderReq { asset, side, px, lots: szi.unsigned_abs(), reduce_only: true, tif: Tif::Ioc, cloid: None };
            self.place(user, req, Origin::Trigger, events);
        }
    }

    /// Today's PnL against the (transfer-adjusted) value at 00:00 UTC.
    pub fn risk_day_pnl(&self, user: Address) -> Option<Usd> {
        let r = self.users.get(&user)?.risk.as_ref()?;
        Some(self.account_value(user) - r.day_start_value)
    }
}
