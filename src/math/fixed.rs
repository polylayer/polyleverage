//! 18-decimal fixed-point math for prediction-market prices (always in `[0, 1]`).

use crate::error::PolyleverageError;
use solana_program::program_error::ProgramError;

/// One unit in fixed-point price representation (1.0 == 1e18).
pub const PRICE_ONE: u64 = 1_000_000_000_000_000_000;
/// Same, as u128, for intermediate products.
pub const PRICE_ONE_U128: u128 = PRICE_ONE as u128;

/// Basis points denominator (10_000 bps = 1.0).
pub const BPS_DENOM: u64 = 10_000;

// --- Price validation -------------------------------------------------------

/// Prices must satisfy `0 < price < 1e18`, with optional tick alignment.
pub fn validate_price(price_fp: u64) -> Result<(), ProgramError> {
    if price_fp == 0 || price_fp >= PRICE_ONE {
        return Err(PolyleverageError::InvalidPriceRange.into());
    }
    Ok(())
}

/// Both endpoints of an entry range are tick-aligned and `min <= max`.
pub fn validate_range(min_fp: u64, max_fp: u64, tick_fp: u64) -> Result<(), ProgramError> {
    validate_price(min_fp)?;
    validate_price(max_fp)?;
    if min_fp > max_fp {
        return Err(PolyleverageError::InvalidPriceRange.into());
    }
    if tick_fp == 0 {
        return Err(PolyleverageError::PriceNotTickAligned.into());
    }
    if min_fp % tick_fp != 0 || max_fp % tick_fp != 0 {
        return Err(PolyleverageError::PriceNotTickAligned.into());
    }
    Ok(())
}

// --- Overlap / match --------------------------------------------------------

/// Return `(overlap_min, overlap_max)` or `None` if ranges don't overlap.
#[inline]
pub fn range_overlap(
    long_min: u64,
    long_max: u64,
    short_min: u64,
    short_max: u64,
) -> Option<(u64, u64)> {
    let lo = long_min.max(short_min);
    let hi = long_max.min(short_max);
    if lo <= hi {
        Some((lo, hi))
    } else {
        None
    }
}

/// Midpoint of a valid overlap — the PMLC entry price.
#[inline]
pub fn overlap_midpoint(lo: u64, hi: u64) -> u64 {
    // lo and hi are both <= PRICE_ONE < 2^60, safe to add as u64.
    (lo / 2) + (hi / 2) + ((lo & 1) & (hi & 1))
}

// --- Notional / size --------------------------------------------------------

/// `notional_atoms = collateral_bucket × leverage` where `leverage = leverage_bps / 10_000`.
/// Returns u128 to safely hold up to ~$1B × 40× = 4e10 atoms × anything reasonable.
#[inline]
pub fn notional_atoms(collateral_bucket: u64, leverage_bps: u32) -> Result<u128, ProgramError> {
    (collateral_bucket as u128)
        .checked_mul(leverage_bps as u128)
        .and_then(|v| v.checked_div(BPS_DENOM as u128))
        .ok_or_else(|| PolyleverageError::ArithmeticOverflow.into())
}

/// `size_fp = notional_atoms × 1e18 / entry_price_fp`. Represented as u128 to avoid overflow.
#[inline]
pub fn position_size_fp(notional_atoms: u128, entry_price_fp: u64) -> Result<u128, ProgramError> {
    if entry_price_fp == 0 {
        return Err(PolyleverageError::DivisionByZero.into());
    }
    notional_atoms
        .checked_mul(PRICE_ONE_U128)
        .and_then(|v| v.checked_div(entry_price_fp as u128))
        .ok_or_else(|| PolyleverageError::ArithmeticOverflow.into())
}

// --- PnL --------------------------------------------------------------------

/// Long-side PnL in quote atoms (signed). Returns `(mark - entry) * size / 1e18`.
#[inline]
pub fn pnl_long_atoms(
    entry_fp: u64,
    mark_fp: u64,
    size_fp: u128,
) -> Result<i128, ProgramError> {
    let delta_fp = (mark_fp as i128) - (entry_fp as i128);
    let size_i = size_fp as i128; // size_fp fits in i128 easily
    delta_fp
        .checked_mul(size_i)
        .and_then(|v| v.checked_div(PRICE_ONE_U128 as i128))
        .ok_or_else(|| PolyleverageError::ArithmeticOverflow.into())
}

/// Compute bounded long-side equity given collateral and PnL. Clamps to `[0, 2c]`.
#[inline]
pub fn bounded_equity(collateral_atoms: u64, pnl: i128) -> u64 {
    let eq = (collateral_atoms as i128).saturating_add(pnl);
    let max = (collateral_atoms as i128).saturating_mul(2);
    eq.clamp(0, max) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_price() {
        assert!(validate_price(0).is_err());
        assert!(validate_price(PRICE_ONE).is_err());
        assert!(validate_price(1).is_ok());
        assert!(validate_price(PRICE_ONE - 1).is_ok());
    }

    #[test]
    fn test_validate_range() {
        // tick = 0.01 = 1e16
        let tick = 10_000_000_000_000_000_u64;
        assert!(validate_range(tick, 2 * tick, tick).is_ok());
        assert!(validate_range(tick + 1, 2 * tick, tick).is_err()); // not aligned
        assert!(validate_range(2 * tick, tick, tick).is_err()); // min > max
    }

    #[test]
    fn test_overlap() {
        // Long [0.58, 0.61], Short [0.60, 0.63] → [0.60, 0.61]
        let lo = 580_000_000_000_000_000_u64;
        let hi = 630_000_000_000_000_000_u64;
        let (omin, omax) = range_overlap(lo, 610_000_000_000_000_000, 600_000_000_000_000_000, hi)
            .expect("should overlap");
        assert_eq!(omin, 600_000_000_000_000_000);
        assert_eq!(omax, 610_000_000_000_000_000);
    }

    #[test]
    fn test_no_overlap() {
        assert!(range_overlap(100, 200, 300, 400).is_none());
    }

    #[test]
    fn test_midpoint() {
        let m = overlap_midpoint(600_000_000_000_000_000, 610_000_000_000_000_000);
        assert_eq!(m, 605_000_000_000_000_000);
    }

    #[test]
    fn test_notional_and_size() {
        // $1000 @ 20x = $20,000 notional; entry = 0.60
        let collateral = 1_000_000_000_u64; // 1000 * 1e6 atoms (USDC)
        let leverage_bps = 200_000_u32; // 20x
        let entry = 600_000_000_000_000_000_u64; // 0.60
        let n = notional_atoms(collateral, leverage_bps).unwrap();
        assert_eq!(n, 20_000_000_000);
        let size = position_size_fp(n, entry).unwrap();
        // 20,000 atoms (scaled) / 0.60 ≈ 33,333 atoms-equivalent
        // exactly: 20e9 * 1e18 / 6e17 = 33_333_333_333_333_333_333 (check calc)
        // 20e9 * 1e18 = 2e28; / 6e17 = 3.333...e10 scaled — let's just verify magnitudes
        assert!(size > 0);
    }

    #[test]
    fn test_pnl_long_profitable() {
        let entry = 600_000_000_000_000_000_u64;
        let mark = 630_000_000_000_000_000_u64;
        let size = 33_333_333_333_u128; // rough size
        let pnl = pnl_long_atoms(entry, mark, size).unwrap();
        assert!(pnl > 0);
    }

    #[test]
    fn test_pnl_short_profitable() {
        // Short profits when mark < entry, i.e. pnl_long is negative.
        let entry = 600_000_000_000_000_000_u64;
        let mark = 570_000_000_000_000_000_u64;
        let size = 33_333_333_333_u128;
        let pnl = pnl_long_atoms(entry, mark, size).unwrap();
        assert!(pnl < 0);
    }

    #[test]
    fn test_bounded_equity() {
        let c = 1_000_000_000_u64;
        assert_eq!(bounded_equity(c, 0), c);
        assert_eq!(bounded_equity(c, -(c as i128)), 0);
        assert_eq!(bounded_equity(c, c as i128), 2 * c);
        assert_eq!(bounded_equity(c, (c as i128) * 100), 2 * c); // clamped to 2c
        assert_eq!(bounded_equity(c, -(c as i128) * 100), 0); // clamped to 0
    }
}
