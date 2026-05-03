//! Polyleverage — pairwise-matched leverage contracts on prediction markets.
//!
//! See `docs/POLYLEVERAGE_SOLANA_SPEC.md` for the full protocol specification.

#![deny(unsafe_op_in_unsafe_fn)]

use solana_program::declare_id;

pub mod attestation;
pub mod error;
pub mod instruction;
pub mod math;
pub mod pod;
pub mod processor;
pub mod seeds;
pub mod state;
pub mod utils;

#[cfg(not(feature = "no-entrypoint"))]
pub mod entrypoint;

// Program ID from solana/keys/polyleverage-keypair.json — replace before mainnet.
declare_id!("6Fvi3dGdQkBP8HHZFt3e42RJUKtzmfM4wHkCXVjiYyqv");
