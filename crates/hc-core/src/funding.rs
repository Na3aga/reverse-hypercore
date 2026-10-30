//! Funding: premium sampling and the hourly funding rate.
//!
//! Public formula: `F = P + clamp(r − P, −c, c)` on an 8-hour basis, where `P`
//! is the average premium over the hour (sampled every 5 s) and `r` the
//! interest rate (0.01% / 8h). Funding is paid hourly at `F / 8`, capped at
//! 4%/hour, as `szi * oracle_px * rate` (longs pay when positive).
//!
//! The binary's `FundingTracker` keeps `asset_to_premiums`, `default_clamp`,
//! `default_impact_usd`, `use_binance_formula` and per-perp multiplier,
//! interest-rate and clamp overrides, which is consistent with this form.

use crate::book::Book;
use crate::fixed::{self, Px, Rate, RATE_SCALE};
use crate::types::Side;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingState {
    /// Sum of premium samples since the last payment (Rate units).
    pub premium_sum: i128,
    pub n_samples: u64,
}

/// Premium from impact prices: `(max(ib − o, 0) − max(o − ia, 0)) / o`.
/// A missing impact price (thin book) contributes zero.
pub fn premium(book: &Book, oracle: Px, impact_ntl: i128, sz_decimals: u32) -> Rate {
    let o = oracle.0 as i128;
    // Impact bid: average price to sell `impact_ntl` into the bids.
    let ib = book.impact_px(Side::Ask, impact_ntl, sz_decimals).map(|p| p.0 as i128);
    let ia = book.impact_px(Side::Bid, impact_ntl, sz_decimals).map(|p| p.0 as i128);
    let up = ib.map(|ib| (ib - o).max(0)).unwrap_or(0);
    let down = ia.map(|ia| (o - ia).max(0)).unwrap_or(0);
    fixed::ratio(up - down, o)
}

/// Hourly funding rate from the average premium.
pub fn hourly_rate(avg_premium: Rate, interest_8h: Rate, clamp: Rate, cap_hourly: Rate) -> Rate {
    let p = avg_premium.0 as i128;
    let adj = (interest_8h.0 as i128 - p).clamp(-(clamp.0 as i128), clamp.0 as i128);
    let f8 = p + adj;
    let hourly = f8 / 8;
    Rate(hourly.clamp(-(cap_hourly.0 as i128), cap_hourly.0 as i128) as i64)
}

impl FundingState {
    pub fn sample(&mut self, premium: Rate) {
        self.premium_sum += premium.0 as i128;
        self.n_samples += 1;
    }

    /// Average premium, then reset for the next interval.
    pub fn take_average(&mut self) -> (Rate, u64) {
        let n = self.n_samples;
        let avg = if n == 0 { Rate(0) } else { Rate(fixed::div_floor(self.premium_sum, n as i128) as i64) };
        *self = FundingState::default();
        (avg, n)
    }
}

pub const fn rate_bps(bps_e2: i64) -> Rate {
    // bps_e2 is basis points * 100, e.g. 450 = 4.5 bps.
    Rate(bps_e2 * (RATE_SCALE as i64 / 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: i64 = RATE_SCALE as i64;

    #[test]
    fn public_example_inside_clamp_band() {
        // Docs example: premium 0.00031774 -> hourly funding 0.0000125.
        let p = Rate(317_740_000); // 0.00031774
        let f = hourly_rate(p, Rate(R / 10_000), Rate(5 * R / 10_000), Rate(4 * R / 100));
        assert_eq!(f, Rate(12_500_000)); // 0.0000125
    }

    #[test]
    fn outside_band_and_cap() {
        // Premium 1%: F8 = 0.01 + clamp(0.0001-0.01) = 0.01 - 0.0005 = 0.0095; hourly 0.0011875.
        let f = hourly_rate(Rate(R / 100), Rate(R / 10_000), Rate(5 * R / 10_000), Rate(4 * R / 100));
        assert_eq!(f, Rate(1_187_500_000));
        // Huge premium is capped at 4%/h.
        let f = hourly_rate(Rate(R), Rate(R / 10_000), Rate(5 * R / 10_000), Rate(4 * R / 100));
        assert_eq!(f, Rate(4 * R / 100));
    }

    #[test]
    fn bps_helper() {
        assert_eq!(rate_bps(450), Rate(450_000_000)); // 4.5 bps = 0.00045
    }
}
