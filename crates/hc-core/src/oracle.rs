//! Oracle aggregation and mark price.
//!
//! - Each validator submits one price per asset (a `SetOracle` action). In a
//!   real deployment each validator's price is itself a weighted median of
//!   venue prices ([`venue_weighted_median`]).
//! - At the end of a block that contains new votes, the oracle price of each
//!   asset becomes the **stake-weighted median** of fresh votes, provided the
//!   voters hold more than 2/3 of the stake.
//! - The mark price is the median of:
//!   1. oracle + 150 s EMA of (mid − oracle),
//!   2. median(best bid, best ask, last trade),
//!   3. because only two inputs exist here (no external perp venues), the
//!      30 s EMA of input 2.
//!
//! EMAs are time-weighted: `num ← num·e^(−t/τ) + sample·t`,
//! `den ← den·e^(−t/τ) + t`, `ema = num/den`, computed in integer fixed point.

use crate::fixed::{self, Px, EXP_SCALE};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ema {
    pub num: i128,
    pub den: i128,
    pub last_ms: u64,
    pub tau_ms: u64,
}

impl Ema {
    pub fn new(tau_ms: u64) -> Ema {
        Ema { tau_ms, ..Default::default() }
    }

    pub fn update(&mut self, sample: i128, now_ms: u64) {
        if self.den == 0 {
            // First sample: unit weight.
            self.num = sample;
            self.den = 1;
            self.last_ms = now_ms;
            return;
        }
        let t = now_ms.saturating_sub(self.last_ms) as i128;
        if t == 0 {
            return;
        }
        let decay = fixed::exp_neg(t, self.tau_ms as i128);
        self.num = self.num * decay / EXP_SCALE + sample * t;
        self.den = self.den * decay / EXP_SCALE + t;
        self.last_ms = now_ms;
    }

    pub fn value(&self) -> Option<i128> {
        (self.den > 0).then(|| fixed::div_floor(self.num, self.den))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleVote {
    pub time_ms: u64,
    pub pxs: Vec<Px>,
}

/// Stake-weighted median per asset over votes no older than `max_age_ms`.
/// Returns `None` if fresh votes hold ≤ 2/3 of total stake.
pub fn aggregate(
    votes: &BTreeMap<u32, OracleVote>,
    stakes: &[u64],
    n_assets: usize,
    now_ms: u64,
    max_age_ms: u64,
) -> Option<Vec<Px>> {
    let total: u128 = stakes.iter().map(|s| *s as u128).sum();
    let fresh: Vec<(&u32, &OracleVote)> =
        votes.iter().filter(|(_, v)| now_ms.saturating_sub(v.time_ms) <= max_age_ms && v.pxs.len() == n_assets).collect();
    let fresh_stake: u128 = fresh.iter().map(|(i, _)| stakes[**i as usize] as u128).sum();
    if fresh_stake * 3 <= total * 2 {
        return None;
    }
    Some(
        (0..n_assets)
            .map(|a| {
                let pts: Vec<(i128, u64)> = fresh.iter().map(|(i, v)| (v.pxs[a].0 as i128, stakes[**i as usize])).collect();
                Px(fixed::weighted_median(&pts).expect("non-empty") as u64)
            })
            .collect(),
    )
}

/// Venue weights from the public oracle docs (Binance, OKX, Bybit, Kraken,
/// Kucoin, Gate, MEXC, Hyperliquid).
pub const VENUE_WEIGHTS: [u64; 8] = [3, 2, 2, 1, 1, 1, 1, 1];

/// A validator's own price: weighted median of the venue prices it observes.
/// Missing venues are `None` and simply drop out.
pub fn venue_weighted_median(venue_pxs: &[Option<Px>]) -> Option<Px> {
    let pts: Vec<(i128, u64)> = venue_pxs
        .iter()
        .zip(VENUE_WEIGHTS)
        .filter_map(|(p, w)| p.map(|p| (p.0 as i128, w)))
        .collect();
    fixed::weighted_median(&pts).map(|v| Px(v as u64))
}

/// Per-asset price state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceState {
    pub oracle_px: Px,
    pub mark_px: Px,
    pub last_trade_px: Option<Px>,
    /// 150 s EMA of (mid − oracle).
    pub ema_mid_minus_oracle: Ema,
    /// 30 s EMA of median(bid, ask, last).
    pub ema_book: Ema,
}

impl PriceState {
    pub fn new(oracle_px: Px) -> PriceState {
        PriceState {
            oracle_px,
            mark_px: oracle_px,
            last_trade_px: None,
            ema_mid_minus_oracle: Ema::new(150_000),
            ema_book: Ema::new(30_000),
        }
    }

    /// Recomputes the mark price after a new oracle price.
    pub fn update_mark(&mut self, best_bid: Option<Px>, best_ask: Option<Px>, now_ms: u64) {
        let oracle = self.oracle_px.0 as i128;
        if let (Some(b), Some(a)) = (best_bid, best_ask) {
            let mid = (b.0 as i128 + a.0 as i128) / 2;
            self.ema_mid_minus_oracle.update(mid - oracle, now_ms);
        }
        let mut inputs = vec![oracle + self.ema_mid_minus_oracle.value().unwrap_or(0)];
        let book: Vec<i128> = [best_bid, best_ask, self.last_trade_px].iter().flatten().map(|p| p.0 as i128).collect();
        if !book.is_empty() {
            let m = fixed::median(&book);
            self.ema_book.update(m, now_ms);
            inputs.push(m);
            // Exactly two of three inputs exist (no external perp venues), so
            // the 30 s EMA joins the median.
            inputs.push(self.ema_book.value().expect("just updated"));
        }
        self.mark_px = Px(fixed::median(&inputs).max(1) as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stake_weighted_median_requires_quorum() {
        let mut votes = BTreeMap::new();
        votes.insert(0, OracleVote { time_ms: 1000, pxs: vec![Px(100)] });
        votes.insert(1, OracleVote { time_ms: 1000, pxs: vec![Px(102)] });
        let stakes = [1, 1, 1, 1];
        assert_eq!(aggregate(&votes, &stakes, 1, 1000, 5000), None);
        votes.insert(2, OracleVote { time_ms: 1000, pxs: vec![Px(500)] });
        assert_eq!(aggregate(&votes, &stakes, 1, 1000, 5000), Some(vec![Px(102)]));
        // Stale votes do not count.
        assert_eq!(aggregate(&votes, &stakes, 1, 100_000, 5000), None);
    }

    #[test]
    fn venue_median() {
        let p = |v| Some(Px(v));
        assert_eq!(venue_weighted_median(&[p(100), p(200), p(200), p(1), p(1), p(1), p(1), p(1)]), Some(Px(100)));
        assert_eq!(venue_weighted_median(&[None; 8]), None);
    }

    #[test]
    fn mark_tracks_book_and_oracle() {
        let mut s = PriceState::new(Px(100_000_000));
        // Empty book: mark = oracle.
        s.update_mark(None, None, 0);
        assert_eq!(s.mark_px, Px(100_000_000));
        // Book at 101/102: inputs = [oracle+ema(mid-oracle)=101.5, median(101,102)=101.5, ema=101.5]
        s.update_mark(Some(Px(101_000_000)), Some(Px(102_000_000)), 3_000);
        assert_eq!(s.mark_px, Px(101_500_000));
    }
}
