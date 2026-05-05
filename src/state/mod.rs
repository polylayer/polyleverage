//! Account state — structs and layout constants for every persistent account.

pub mod admin_multisig;
pub mod bucket_registry;
pub mod fee_schedule;
pub mod stablecoin_registry;
pub mod fee_treasury;
pub mod instrument_config;
pub mod intent_book;
pub mod intent_tree;
pub mod margin_account;
pub mod market_nonce;
pub mod pmlc;
pub mod program_config;
pub mod seat_tree;
pub mod session;
pub mod timelock;
pub mod user_volume;

pub use admin_multisig::*;
pub use bucket_registry::*;
pub use fee_schedule::*;
pub use stablecoin_registry::*;
pub use fee_treasury::*;
pub use instrument_config::*;
pub use intent_book::*;
pub use margin_account::*;
pub use market_nonce::*;
pub use pmlc::*;
pub use program_config::*;
pub use session::*;
pub use timelock::*;
pub use user_volume::*;

// Discriminator bytes live at offset 0 of every Pod account.
// Keep them distinct across account types so a wrong-account attack fails validation.
// NOTE: fee_schedule, fee_treasury, user_volume, timelock define their own DISC
// constants in their respective modules (see those files). Base set below.
pub const DISC_PROGRAM_CONFIG: u8 = 1;
pub const DISC_MARGIN_ACCOUNT: u8 = 2;
pub const DISC_INSTRUMENT_CONFIG: u8 = 3;
pub const DISC_INTENT_BOOK: u8 = 4;
pub const DISC_PMLC: u8 = 5;
pub const DISC_MARKET_NONCE: u8 = 8;
