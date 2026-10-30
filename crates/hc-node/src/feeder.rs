//! Validator oracle feeders.
//!
//! Each validator builds its price as the weighted median of eight "venue"
//! quotes (the public weights 3,2,2,1,1,1,1,1), then signs a `SetOracle` vote.
//! The engine combines votes with a stake-weighted median. The venues are
//! synthetic here: a shared random walk plus per-venue and per-validator noise,
//! seeded so a devnet run can be reproduced.

use hc_core::action::SigningKey;
use hc_core::fixed::{format_decimal, Px};
use hc_core::oracle::venue_weighted_median;
use hc_core::{Action, SignedTx};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

pub struct PriceWalk {
    rng: ChaCha8Rng,
    /// Current reference prices (µUSD).
    pub pxs: Vec<i64>,
    /// Max move per step in basis points.
    pub step_bps: i64,
}

impl PriceWalk {
    pub fn new(seed: u64, start: &[Px], step_bps: i64) -> PriceWalk {
        PriceWalk { rng: ChaCha8Rng::seed_from_u64(seed), pxs: start.iter().map(|p| p.0 as i64).collect(), step_bps }
    }

    pub fn step(&mut self) {
        for p in self.pxs.iter_mut() {
            let bps = self.rng.gen_range(-self.step_bps..=self.step_bps);
            *p = (*p + *p * bps / 10_000).max(1);
        }
    }

    /// Venue quotes seen by validator `v`, then its weighted median.
    pub fn validator_prices(&mut self, v: usize) -> Vec<Px> {
        let base = self.pxs.clone();
        base.iter()
            .map(|p| {
                let quotes: Vec<Option<Px>> = (0..8)
                    .map(|venue| {
                        // Small venue/validator noise: up to ±3 bps; one venue sometimes missing.
                        if (venue + v) % 7 == 6 && self.rng.gen_bool(0.3) {
                            return None;
                        }
                        let n = self.rng.gen_range(-3..=3);
                        Some(Px((p + p * n / 10_000).max(1) as u64))
                    })
                    .collect();
                venue_weighted_median(&quotes).expect("some venue quoted")
            })
            .collect()
    }
}

/// Rounds a µUSD oracle price to 6 significant figures, formatted as a decimal string.
pub fn px_string(px: Px) -> String {
    let mut v = px.0 as i128;
    let mut digits = 0;
    let mut t = v;
    while t > 0 {
        t /= 10;
        digits += 1;
    }
    if digits > 6 {
        let s = 10i128.pow(digits - 6);
        v = v / s * s;
    }
    format_decimal(v, 6)
}

pub fn oracle_tx(key: &SigningKey, pxs: &[Px], nonce: u64) -> SignedTx {
    SignedTx::sign(key, Action::SetOracle { pxs: pxs.iter().map(|p| px_string(*p)).collect() }, nonce)
}
