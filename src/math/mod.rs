//! Protocol math: fixed-point pricing, notional/size conversion, PnL.
//!
//! Price is a `u64` fixed-point with 18 decimals (`1.0 == 1e18`). Prediction-market
//! prices live in `[0, 1]` so they always fit. We use `i128`/`u128` for intermediate
//! products to avoid overflow — a $40,000 notional × 1e18 is well below 2^128.

pub mod fixed;

pub use fixed::*;
