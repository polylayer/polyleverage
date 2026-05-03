//! PDA seed constants.
//!
//! Keep these in sync with the spec (§1) and with the client/frontend derivations.

pub const SEED_CONFIG: &[u8] = b"config";
pub const SEED_MARGIN: &[u8] = b"margin";
pub const SEED_VAULT: &[u8] = b"vault";
pub const SEED_INSTRUMENT: &[u8] = b"instr";
pub const SEED_BOOK: &[u8] = b"book";
pub const SEED_PMLC: &[u8] = b"pmlc";
pub const SEED_FEE_SCHEDULE: &[u8] = b"fee_schedule";
pub const SEED_USER_VOLUME: &[u8] = b"volume";
pub const SEED_TREASURY: &[u8] = b"treasury";
pub const SEED_MARKET_NONCE: &[u8] = b"nonce";
pub const SEED_TIMELOCK: &[u8] = b"timelock";
pub const SEED_ADMIN_MULTISIG: &[u8] = b"admin_multisig";
pub const SEED_BUCKET_REGISTRY: &[u8] = b"bucket_registry";
pub const SEED_STABLECOIN_REGISTRY: &[u8] = b"stablecoin_registry";
pub const SEED_WUSD_MINT: &[u8] = b"wusd_mint";
pub const SEED_WUSD_AUTHORITY: &[u8] = b"wusd_authority";
pub const SEED_WUSD_RESERVE: &[u8] = b"wusd_reserve";
