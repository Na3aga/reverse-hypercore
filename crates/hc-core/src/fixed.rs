//! Integer fixed-point arithmetic. No floating point is used anywhere in state
//! transitions, so execution is bit-for-bit reproducible across machines.
//!
//! Units:
//! - [`Px`]: price in micro-USD per whole coin (scale 1e6). Perp prices have at
//!   most `MAX_DECIMALS - sz_decimals` decimals, with `MAX_DECIMALS = 6`, so 1e6
//!   represents every valid price exactly.
//! - Sizes: integer *lots*, where a lot is `10^-sz_decimals` coins. Signed
//!   positions (`szi`) are `i64` lots.
//! - [`Usd`]: micro-USDC (scale 1e6), the collateral unit.
//! - [`Rate`]: dimensionless ratio with scale 1e12 (premiums, funding rates).

use serde::{Deserialize, Serialize};
use std::fmt;

pub const PX_SCALE: i128 = 1_000_000;
pub const USD_SCALE: i128 = 1_000_000;
pub const RATE_SCALE: i128 = 1_000_000_000_000;
/// Perp prices may carry at most `MAX_DECIMALS - sz_decimals` decimal places.
pub const MAX_DECIMALS: u32 = 6;
/// Prices may carry at most this many significant figures (integers excepted).
pub const MAX_SIG_FIGS: u32 = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Px(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Usd(pub i64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Rate(pub i64);

impl Usd {
    pub const ZERO: Usd = Usd(0);
    pub fn from_whole(usd: i64) -> Usd {
        Usd(usd * USD_SCALE as i64)
    }
}

impl std::ops::Add for Usd {
    type Output = Usd;
    fn add(self, o: Usd) -> Usd {
        Usd(self.0.checked_add(o.0).expect("usd overflow"))
    }
}
impl std::ops::Sub for Usd {
    type Output = Usd;
    fn sub(self, o: Usd) -> Usd {
        Usd(self.0.checked_sub(o.0).expect("usd overflow"))
    }
}
impl std::ops::AddAssign for Usd {
    fn add_assign(&mut self, o: Usd) {
        *self = *self + o;
    }
}
impl std::ops::SubAssign for Usd {
    fn sub_assign(&mut self, o: Usd) {
        *self = *self - o;
    }
}
impl std::ops::Neg for Usd {
    type Output = Usd;
    fn neg(self) -> Usd {
        Usd(-self.0)
    }
}

/// Integer division rounding toward negative infinity.
pub fn div_floor(a: i128, b: i128) -> i128 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Integer division rounding half away from zero.
pub fn div_round(a: i128, b: i128) -> i128 {
    let neg = (a < 0) != (b < 0);
    let (a, b) = (a.abs(), b.abs());
    let q = (a + b / 2) / b;
    if neg {
        -q
    } else {
        q
    }
}

pub fn pow10(n: u32) -> i128 {
    10i128.pow(n)
}

/// Signed notional of `lots` (signed) at `px`, rounded toward zero:
/// `lots * 10^-sz_decimals * px`.
pub fn ntl(px: Px, lots: i64, sz_decimals: u32) -> Usd {
    // px is micro-USD/coin and USD is micro-USDC, so the scales cancel.
    let v = px.0 as i128 * lots as i128 / pow10(sz_decimals);
    Usd(i64::try_from(v).expect("notional overflow"))
}

/// `usd * rate`, rounded toward zero.
pub fn mul_rate(usd: Usd, rate: Rate) -> Usd {
    Usd((usd.0 as i128 * rate.0 as i128 / RATE_SCALE) as i64)
}

/// `a / b` as a [`Rate`] (both in the same unit).
pub fn ratio(a: i128, b: i128) -> Rate {
    Rate(div_floor(a * RATE_SCALE, b) as i64)
}

pub fn px_mul_rate(px: Px, rate: Rate) -> i128 {
    px.0 as i128 * rate.0 as i128 / RATE_SCALE
}

/// Checks the tick rule: integer prices are always valid, otherwise at most
/// [`MAX_SIG_FIGS`] significant figures and at most
/// `MAX_DECIMALS - sz_decimals` decimals.
pub fn is_valid_px(px: Px, sz_decimals: u32) -> bool {
    if px.0 == 0 {
        return false;
    }
    if px.0 as i128 % PX_SCALE == 0 {
        return true;
    }
    let max_decimals = MAX_DECIMALS.saturating_sub(sz_decimals);
    // Decimal places beyond `max_decimals` show up as non-zero low digits.
    if px.0 as i128 % pow10(MAX_DECIMALS - max_decimals) != 0 {
        return false;
    }
    let mut digits = px.0;
    while digits.is_multiple_of(10) {
        digits /= 10;
    }
    let mut n = 0;
    while digits > 0 {
        digits /= 10;
        n += 1;
    }
    n <= MAX_SIG_FIGS
}

/// Rounds a price to [`MAX_SIG_FIGS`] significant figures and the asset's decimal
/// limit (toward zero for sells-friendly determinism; callers choose direction
/// via `round_up`). Used for slippage prices of market and liquidation orders.
pub fn round_px(px: i128, sz_decimals: u32, round_up: bool) -> Px {
    let max_decimals = MAX_DECIMALS.saturating_sub(sz_decimals);
    let decimal_step = pow10(MAX_DECIMALS - max_decimals);
    let mut digits = 0;
    let mut v = px;
    while v > 0 {
        v /= 10;
        digits += 1;
    }
    let sig_step = if digits > MAX_SIG_FIGS { pow10(digits - MAX_SIG_FIGS) } else { 1 };
    // Integer prices are always valid, so the step never exceeds one whole coin.
    let step = decimal_step.max(sig_step).min(PX_SCALE);
    let q = if round_up { (px + step - 1) / step } else { px / step };
    Px((q * step).max(step) as u64)
}

/// Parses a decimal string (e.g. `"123.45"`, `"-0.5"`) into an integer with
/// `scale` decimals. Rejects strings with more precision than `scale`.
pub fn parse_decimal(s: &str, scale: u32) -> Option<i128> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.chars().chain(frac_part.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let frac_trimmed = frac_part.trim_end_matches('0');
    if frac_trimmed.len() > scale as usize {
        return None;
    }
    let int_val: i128 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let mut frac_val: i128 = if frac_trimmed.is_empty() { 0 } else { frac_trimmed.parse().ok()? };
    frac_val *= pow10(scale - frac_trimmed.len() as u32);
    let v = int_val.checked_mul(pow10(scale))?.checked_add(frac_val)?;
    Some(if neg { -v } else { v })
}

/// Formats a scaled integer as a decimal string without trailing zeros, the
/// way the public API prints numbers.
pub fn format_decimal(v: i128, scale: u32) -> String {
    let neg = v < 0;
    let a = v.unsigned_abs();
    let p = pow10(scale) as u128;
    let int_part = a / p;
    let frac = a % p;
    let mut s = if frac == 0 {
        int_part.to_string()
    } else {
        let f = format!("{:0width$}", frac, width = scale as usize);
        format!("{}.{}", int_part, f.trim_end_matches('0'))
    };
    if neg && a != 0 {
        s.insert(0, '-');
    }
    s
}

impl fmt::Display for Px {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_decimal(self.0 as i128, 6))
    }
}
impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_decimal(self.0 as i128, 6))
    }
}
impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_decimal(self.0 as i128, 12))
    }
}

/// Fixed-point scale used by [`exp_neg`].
pub const EXP_SCALE: i128 = 1_000_000_000_000_000_000;

/// `exp(-num/den)` in [`EXP_SCALE`] fixed point, computed with integer-only
/// arithmetic (range reduction plus a Taylor series), so it is deterministic.
pub fn exp_neg(num: i128, den: i128) -> i128 {
    assert!(num >= 0 && den > 0);
    let x = num * EXP_SCALE / den;
    if x >= 50 * EXP_SCALE {
        return 0;
    }
    let whole = x / EXP_SCALE;
    let frac = x % EXP_SCALE;
    // e^-1 in fixed point.
    const E_INV: i128 = 367_879_441_171_442_322;
    let mut r = EXP_SCALE;
    for _ in 0..whole {
        r = r * E_INV / EXP_SCALE;
    }
    // e^-frac = sum_k (-frac)^k / k!
    let mut term = EXP_SCALE;
    let mut sum = EXP_SCALE;
    for k in 1..30 {
        term = -term * frac / EXP_SCALE / k;
        if term == 0 {
            break;
        }
        sum += term;
    }
    r * sum / EXP_SCALE
}

/// Median of a non-empty slice; for an even count, the floor of the mean of the
/// two middle values.
pub fn median(values: &[i128]) -> i128 {
    assert!(!values.is_empty());
    let mut v = values.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        div_floor(v[n / 2 - 1] + v[n / 2], 2)
    }
}

/// Weighted median: the smallest value whose cumulative weight reaches half of
/// the total weight. Used for stake-weighted oracle aggregation and for each
/// validator's weighted median of venue prices.
pub fn weighted_median(values: &[(i128, u64)]) -> Option<i128> {
    let total: u128 = values.iter().map(|(_, w)| *w as u128).sum();
    if total == 0 {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable_by_key(|(x, _)| *x);
    let mut cum: u128 = 0;
    for (x, w) in v {
        cum += w as u128;
        if cum * 2 >= total {
            return Some(x);
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_rule_matches_public_examples() {
        // Public docs: 1234.5 ok, 1234.56 not (5 sig figs); 0.001234 ok, 0.0012345 not.
        assert!(is_valid_px(Px(1_234_500_000), 0));
        assert!(!is_valid_px(Px(1_234_560_000), 0));
        assert!(is_valid_px(Px(1_234), 0));
        assert!(!is_valid_px(Px(1_234), 1)); // szDecimals=1 allows only 5 decimals
        // Integer prices are always valid.
        assert!(is_valid_px(Px(123_456_000_000), 5));
    }

    #[test]
    fn decimal_roundtrip() {
        assert_eq!(parse_decimal("123.45", 6), Some(123_450_000));
        assert_eq!(parse_decimal("-0.5", 6), Some(-500_000));
        assert_eq!(parse_decimal("1.0000001", 6), None);
        assert_eq!(format_decimal(123_450_000, 6), "123.45");
        assert_eq!(format_decimal(-500_000, 6), "-0.5");
        assert_eq!(format_decimal(0, 6), "0");
    }

    #[test]
    fn exp_is_accurate() {
        // e^-1 ≈ 0.367879441171
        let e1 = exp_neg(1, 1);
        assert!((e1 - 367_879_441_171_442_322).abs() < 1_000);
        // e^-0.5 ≈ 0.60653065971
        let e05 = exp_neg(1, 2);
        assert!((e05 - 606_530_659_712_633_423).abs() < 1_000_000);
        assert_eq!(exp_neg(0, 5), EXP_SCALE);
    }

    #[test]
    fn medians() {
        assert_eq!(median(&[3, 1, 2]), 2);
        assert_eq!(median(&[4, 1, 2, 3]), 2);
        assert_eq!(weighted_median(&[(10, 3), (20, 2), (30, 2), (40, 1)]), Some(20));
        assert_eq!(weighted_median(&[(5, 1), (1, 1), (9, 1), (7, 1)]), Some(5));
    }

    #[test]
    fn round_px_respects_rules() {
        let p = round_px(123_456_789_000, 5, false); // 123456.789 -> integer price allowed
        assert!(is_valid_px(p, 5));
        let p = round_px(1_234_567, 2, true); // 1.234567 with szDecimals=2 -> 4 decimals
        assert!(is_valid_px(p, 2));
        assert_eq!(p, Px(1_234_600));
    }
}
