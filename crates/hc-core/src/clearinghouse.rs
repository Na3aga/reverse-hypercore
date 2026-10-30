//! Clearinghouse: positions, account value, tiered maintenance margin, and
//! fill settlement with realized PnL, in cross and isolated margin.
//!
//! Model (from the public margining docs):
//! - cross account value = cross balance + Σ unrealized PnL of cross
//!   positions, with `upnl = szi * mark - entry_ntl`, where `entry_ntl` is the
//!   signed cost basis of the open position (the size-weighted average on
//!   opens, unchanged on closes);
//! - an isolated position holds its own margin, moved out of the cross balance
//!   when it opens. Its equity is `margin + upnl`: its losses stop at that
//!   margin, it is liquidated on its own, and it never counts toward the cross
//!   account;
//! - initial margin of a position = `|szi| * mark / leverage`;
//! - maintenance margin = `ntl * mmr(tier) - maintenance_deduction(tier)`, with
//!   `mmr = 1 / (2 * tier max leverage)` and the deduction chosen to make the
//!   requirement continuous across tiers.

use crate::fixed::{self, ntl, Px, Rate, Usd, RATE_SCALE};
use crate::types::{AssetId, Oid};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarginTier {
    /// Notional at which this tier starts.
    pub lower_bound: Usd,
    pub max_leverage: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetMeta {
    pub name: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    /// Ascending by `lower_bound`; the first tier must start at 0. Its
    /// `max_leverage` must equal the asset's `max_leverage`.
    pub margin_tiers: Vec<MarginTier>,
    /// Notional used for funding impact prices.
    pub impact_ntl: Usd,
}

impl AssetMeta {
    fn mmr(max_leverage: u32) -> i128 {
        RATE_SCALE / (2 * max_leverage as i128)
    }

    /// Maintenance margin for an absolute notional.
    pub fn maintenance_margin(&self, abs_ntl: Usd) -> Usd {
        let mut deduction: i128 = 0;
        let mut prev_mmr = Self::mmr(self.margin_tiers[0].max_leverage);
        let mut mmr = prev_mmr;
        for tier in &self.margin_tiers[1..] {
            if abs_ntl < tier.lower_bound {
                break;
            }
            mmr = Self::mmr(tier.max_leverage);
            deduction += tier.lower_bound.0 as i128 * (mmr - prev_mmr) / RATE_SCALE;
            prev_mmr = mmr;
        }
        Usd((abs_ntl.0 as i128 * mmr / RATE_SCALE - deduction) as i64)
    }

    /// Max leverage allowed for a position of this absolute notional.
    pub fn max_leverage_at(&self, abs_ntl: Usd) -> u32 {
        self.margin_tiers.iter().filter(|t| abs_ntl >= t.lower_bound).map(|t| t.max_leverage).next_back().unwrap_or(self.max_leverage)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Signed size in lots (positive = long).
    pub szi: i64,
    /// Signed cost basis in µUSD (`szi * entry_px`).
    pub entry_ntl: i64,
    /// Cumulative funding paid (positive = paid) since open and all-time.
    pub funding_since_open: Usd,
    pub funding_all_time: Usd,
    /// Margin of an isolated position (`None` for cross): what was moved in
    /// from the cross balance, plus realized PnL, minus funding.
    pub isolated: Option<Usd>,
}

impl Position {
    pub fn entry_px(&self, sz_decimals: u32) -> Option<Px> {
        if self.szi == 0 {
            return None;
        }
        Some(Px((self.entry_ntl as i128 * fixed::pow10(sz_decimals) / self.szi as i128) as u64))
    }

    pub fn upnl(&self, mark: Px, sz_decimals: u32) -> Usd {
        Usd(ntl(mark, self.szi, sz_decimals).0 - self.entry_ntl)
    }

    /// Equity of an isolated position: its margin plus unrealized PnL.
    pub fn isolated_equity(&self, mark: Px, sz_decimals: u32) -> Option<Usd> {
        self.isolated.map(|m| m + self.upnl(mark, sz_decimals))
    }
}

/// Result of applying a fill to a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FillEffect {
    pub start_szi: i64,
    pub closed_pnl: Usd,
}

/// Applies a signed fill of `d` lots at `px` to `pos`. Returns realized PnL
/// from the closing part, which the caller credits to the cross balance.
pub fn apply_fill(pos: &mut Position, d: i64, px: Px, sz_decimals: u32) -> FillEffect {
    let start_szi = pos.szi;
    let mut closed_pnl = 0i64;
    let mut d_rem = d;
    if pos.szi != 0 && (pos.szi > 0) != (d > 0) {
        // Closing part: c lots with the sign of d.
        let c = d.abs().min(pos.szi.abs()) * d.signum();
        // Cost removed is proportional to the closed fraction (toward zero).
        let removed = (pos.entry_ntl as i128 * c.abs() as i128 / pos.szi.abs() as i128) as i64;
        closed_pnl = -ntl(px, c, sz_decimals).0 - removed;
        pos.entry_ntl -= removed;
        pos.szi += c;
        d_rem -= c;
        if pos.szi == 0 {
            pos.entry_ntl = 0;
            pos.funding_since_open = Usd::ZERO;
        }
    }
    if d_rem != 0 {
        pos.entry_ntl += ntl(px, d_rem, sz_decimals).0;
        pos.szi += d_rem;
    }
    FillEffect { start_szi, closed_pnl: Usd(closed_pnl) }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserState {
    /// Cross collateral balance excluding unrealized PnL ("raw USD").
    pub balance: Usd,
    pub positions: BTreeMap<AssetId, Position>,
    /// Per-asset leverage; absent means the asset default.
    pub leverage: BTreeMap<AssetId, u32>,
    /// Assets set to isolated margin: positions opened there are isolated.
    pub isolated: BTreeSet<AssetId>,
    /// Highest nonces seen (kept to [`crate::exchange::Params::nonce_set_size`]).
    pub nonces: BTreeSet<u64>,
    pub open_orders: BTreeSet<(AssetId, Oid)>,
    /// While `now < liq_cooldown_until`, market liquidations are full size.
    pub liq_cooldown_until: u64,
    /// The account's own risk limits, if any were set.
    pub risk: Option<crate::risk::RiskState>,
}

/// Per-asset context needed for margin calculations.
pub struct MarginCtx<'a> {
    pub metas: &'a [AssetMeta],
    pub marks: &'a [Px],
    pub default_leverage: &'a [u32],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarginSummary {
    pub account_value: Usd,
    pub total_ntl_pos: Usd,
    pub total_raw_usd: Usd,
    pub total_margin_used: Usd,
    pub maintenance_margin: Usd,
}

impl UserState {
    pub fn leverage_for(&self, asset: AssetId, ctx: &MarginCtx) -> u32 {
        self.leverage.get(&asset).copied().unwrap_or(ctx.default_leverage[asset as usize])
    }

    /// The cross account: balance and cross positions (isolated ones are
    /// margined on their own, see [`UserState::isolated_summary`]).
    pub fn summary(&self, ctx: &MarginCtx) -> MarginSummary {
        let mut s = MarginSummary { total_raw_usd: self.balance, ..Default::default() };
        let mut upnl = Usd::ZERO;
        for (asset, p) in self.positions.iter().filter(|(_, p)| p.isolated.is_none()) {
            let meta = &ctx.metas[*asset as usize];
            let mark = ctx.marks[*asset as usize];
            let value = Usd(ntl(mark, p.szi, meta.sz_decimals).0.abs());
            upnl += p.upnl(mark, meta.sz_decimals);
            s.total_ntl_pos += value;
            s.total_margin_used += Usd(value.0 / self.leverage_for(*asset, ctx) as i64);
            s.maintenance_margin += meta.maintenance_margin(value);
        }
        s.account_value = self.balance + upnl;
        s
    }

    /// An isolated position's (equity, maintenance margin, initial margin).
    pub fn isolated_summary(&self, asset: AssetId, ctx: &MarginCtx) -> Option<(Usd, Usd, Usd)> {
        let p = self.positions.get(&asset)?;
        let meta = &ctx.metas[asset as usize];
        let mark = ctx.marks[asset as usize];
        let value = Usd(ntl(mark, p.szi, meta.sz_decimals).0.abs());
        let equity = p.isolated_equity(mark, meta.sz_decimals)?;
        Some((equity, meta.maintenance_margin(value), Usd(value.0 / self.leverage_for(asset, ctx) as i64)))
    }

    /// Total account value: the cross account plus every isolated position's equity.
    pub fn total_account_value(&self, ctx: &MarginCtx) -> Usd {
        let iso: i64 = self.positions.iter().filter_map(|(a, p)| p.isolated_equity(ctx.marks[*a as usize], ctx.metas[*a as usize].sz_decimals)).map(|u| u.0).sum();
        self.summary(ctx).account_value + Usd(iso)
    }

    /// USDC held in isolated margin.
    pub fn isolated_margin(&self) -> Usd {
        Usd(self.positions.values().filter_map(|p| p.isolated).map(|u| u.0).sum())
    }

    /// Withdrawable / transferable margin: account value minus
    /// `max(initial margin, 10% of total notional)` (public transfer rule).
    pub fn withdrawable(&self, ctx: &MarginCtx) -> Usd {
        let s = self.summary(ctx);
        let required = s.total_margin_used.max(Usd(s.total_ntl_pos.0 / 10));
        Usd((s.account_value - required).0.max(0))
    }
}

/// Fee in µUSD for a notional at `rate`, rounded toward zero.
pub fn fee(abs_ntl: Usd, rate: Rate) -> Usd {
    fixed::mul_rate(abs_ntl, rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> AssetMeta {
        AssetMeta {
            name: "BTC".into(),
            sz_decimals: 5,
            max_leverage: 40,
            margin_tiers: vec![
                MarginTier { lower_bound: Usd::ZERO, max_leverage: 40 },
                MarginTier { lower_bound: Usd::from_whole(150_000_000), max_leverage: 20 },
            ],
            impact_ntl: Usd::from_whole(20_000),
        }
    }

    #[test]
    fn tiered_maintenance_is_continuous() {
        let m = meta();
        // Below tier 2: 1.25% of notional.
        assert_eq!(m.maintenance_margin(Usd::from_whole(1_000_000)), Usd::from_whole(12_500));
        // At the boundary both formulas agree: 150M * 1.25% = 1.875M.
        assert_eq!(m.maintenance_margin(Usd::from_whole(150_000_000)), Usd::from_whole(1_875_000));
        // Above: 200M * 2.5% - 150M * (2.5% - 1.25%) = 5M - 1.875M.
        assert_eq!(m.maintenance_margin(Usd::from_whole(200_000_000)), Usd::from_whole(3_125_000));
    }

    #[test]
    fn fills_realize_pnl() {
        let mut p = Position::default();
        // Buy 2 BTC (200000 lots) at 100k, then 1 at 110k: avg 103333.33
        apply_fill(&mut p, 200_000, Px(100_000_000_000), 5);
        apply_fill(&mut p, 100_000, Px(110_000_000_000), 5);
        assert_eq!(p.entry_ntl, 310_000_000_000);
        // Sell 1.5 BTC at 120k: closes 1.5 of 3, cost 155k, proceeds 180k.
        let e = apply_fill(&mut p, -150_000, Px(120_000_000_000), 5);
        assert_eq!(e.closed_pnl, Usd::from_whole(25_000));
        assert_eq!(p.szi, 150_000);
        // Flip: sell 2.5 at 100k: close 1.5 (pnl 150k - 155k = -5k), open short 1.
        let e = apply_fill(&mut p, -250_000, Px(100_000_000_000), 5);
        assert_eq!(e.closed_pnl, Usd::from_whole(-5_000));
        assert_eq!(p.szi, -100_000);
        assert_eq!(p.entry_ntl, -100_000_000_000);
        assert_eq!(p.entry_px(5), Some(Px(100_000_000_000)));
    }
}
