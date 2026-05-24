//! `InstrumentConfig` — one per `(source, market_id, leverage_bucket, collateral_bucket, twap_window)`.
//!
//! Spec §1.4 / §22.

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_INSTRUMENT};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use super::DISC_INSTRUMENT_CONFIG;

pub const STATUS_ACTIVE: u8 = 0;
pub const STATUS_PAUSED: u8 = 1;
pub const STATUS_CLOSED: u8 = 2;

/// Allowed leverage values (bps). Keep in sync with UI.
/// 2x / 5x / 10x / 20x / 40x / 100x / 250x / 1000x.
pub const ALLOWED_LEVERAGE_BPS: &[u32] = &[
    20_000, 50_000, 100_000, 200_000, 400_000, 1_000_000, 2_500_000, 10_000_000,
];

/// Market-data source. The on-chain program treats `market_id` and the
/// two `source_token_id_*` fields as opaque blobs; the attestor (which
/// knows the source's encoding) is responsible for translating them
/// into platform-specific lookups.
///
/// New sources require a new constant + an attestor branch; no on-chain
/// migration is needed because the byte slots are already there.
pub mod source {
    pub const POLYMARKET: u8 = 0;
    // Reserved (not yet implemented):
    //   pub const KALSHI:   u8 = 1;
    //   pub const MANIFOLD: u8 = 2;

    /// Pyth-priced synthetic instrument (equities / commodities /
    /// crypto majors). `market_id` carries the 32-byte Pyth feed id.
    /// The attestor sources prices from Pyth Benchmarks and normalizes
    /// them into the program's `(0, 1)` fixed-point — see
    /// `docs/internal/POLYLEVERAGE_MULTIASSET.md`.
    pub const PYTH: u8 = 3;

    /// Returns true iff the `s` byte names a source the on-chain program
    /// recognizes. Off-chain attestors enforce their own (possibly
    /// stricter) allow-list.
    pub fn is_known(s: u8) -> bool {
        matches!(s, POLYMARKET | PYTH)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct InstrumentConfig {
    pub discriminator: u8,
    pub status: u8,
    pub bump: u8,
    /// Market-data source — see `source` module.
    pub source: u8,
    pub _pad0: [u8; 4],

    /// Source-specific market identifier. For `source::POLYMARKET` this
    /// is the bytes32 condition_id verbatim. For future sources, see
    /// the per-source encoding rules in `docs/POLYLEVERAGE_SOLANA_SPEC.md` §22.
    pub market_id: [u8; 32],
    /// First outcome token / asset id (for binary markets, "side A").
    /// For Polymarket: YES token_id (uint256, 32 bytes LE).
    pub source_token_id_a: [u8; 32],
    /// Second outcome token (for binary markets, "side B").
    /// For Polymarket: NO token_id.
    pub source_token_id_b: [u8; 32],

    pub collateral_mint: Pubkey,
    /// Associated intent book PDA.
    pub intent_book: Pubkey,

    /// Leverage in bps (e.g. 200_000 == 20x).
    pub leverage_bps: u32,
    pub _pad1: [u8; 4],
    /// Collateral bucket in quote atoms (e.g. $100 USDC == 100_000_000).
    pub collateral_bucket: u64,

    /// TWAP averaging window in slots (liquidation reference). The
    /// attestor uses this off-chain to size its Polymarket history
    /// query; the on-chain code does not introspect TWAP shape.
    pub twap_window_slots: u64,
    /// Minimum price increment (fp with 18 decimals).
    pub tick_fp: u64,
    /// Maintenance-margin threshold in bps. `0` == bounded-win liquidation at zero equity.
    pub liquidation_bps: u32,
    /// Keeper bounty in bps, paid from loser's collateral at liquidation.
    pub liquidation_bounty_bps: u16,
    pub _pad2: [u8; 2],
    /// Attestation freshness bound in seconds (wrapper signature staleness).
    pub max_staleness_secs: u64,

    /// Off-chain attestor reference ceiling, whole USD. Pyth instruments
    /// only — the attestor normalizes a Pyth dollar price into the
    /// program's `(0, 1e18)` fixed-point via
    /// `price_fp = raw * 10^(18+expo) / reference_usd`
    /// (see `docs/internal/POLYLEVERAGE_MULTIASSET.md` §3). Set to `0`
    /// for `POLYMARKET` instruments (unused). Stored on-chain so the
    /// reference is discoverable from the InstrumentConfig itself —
    /// the program never reads it (zero validation impact); it's
    /// metadata the off-chain TEE decodes via `decode_instrument`.
    pub reference_usd: u64,

    pub _reserved: [u8; 40],
}

pub const INSTRUMENT_CONFIG_LEN: usize =
    1 + 1 + 1 + 1 + 4 + 32 + 32 + 32 + 32 + 32 + 4 + 4 + 8 + 8 + 8 + 4 + 2 + 2 + 8 + 8 + 40;
const_assert_size!(InstrumentConfig, INSTRUMENT_CONFIG_LEN);

impl InstrumentConfig {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let c: &InstrumentConfig = pod::try_cast_ref(bytes)?;
        if c.discriminator != DISC_INSTRUMENT_CONFIG {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(c)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let c: &mut InstrumentConfig = pod::try_cast_mut(bytes)?;
        if c.discriminator != DISC_INSTRUMENT_CONFIG {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(c)
    }

    pub fn require_active(&self) -> Result<(), ProgramError> {
        if self.status != STATUS_ACTIVE {
            return Err(PolyleverageError::InstrumentPaused.into());
        }
        Ok(())
    }

    /// PDA seeds: `[SEED_INSTRUMENT, source, market_id, leverage_bps,
    /// collateral_bucket, twap_window_slots]`. The `source` byte is in
    /// the seed so identical bytes32 identifiers from different
    /// platforms cannot collide on the same PDA — even though token-id
    /// collisions are astronomically unlikely, the cost is one byte.
    pub fn find_pda(
        program_id: &Pubkey,
        source: u8,
        market_id: &[u8; 32],
        leverage_bps: u32,
        collateral_bucket: u64,
        twap_window_slots: u64,
    ) -> (Pubkey, u8) {
        let src = [source];
        let lev = leverage_bps.to_le_bytes();
        let bucket = collateral_bucket.to_le_bytes();
        let window = twap_window_slots.to_le_bytes();
        Pubkey::find_program_address(
            &[
                SEED_INSTRUMENT,
                &src,
                market_id.as_ref(),
                &lev,
                &bucket,
                &window,
            ],
            program_id,
        )
    }

    pub fn validate_bucket_config(
        leverage_bps: u32,
        collateral_bucket: u64,
        tick_fp: u64,
    ) -> Result<(), ProgramError> {
        if !ALLOWED_LEVERAGE_BPS.contains(&leverage_bps) {
            return Err(PolyleverageError::InvalidLeverageBucket.into());
        }
        if collateral_bucket == 0 {
            return Err(PolyleverageError::InvalidCollateralBucket.into());
        }
        if tick_fp == 0 {
            return Err(PolyleverageError::PriceNotTickAligned.into());
        }
        Ok(())
    }

    pub fn init(
        bytes: &mut [u8],
        source: u8,
        market_id: [u8; 32],
        source_token_id_a: [u8; 32],
        source_token_id_b: [u8; 32],
        collateral_mint: Pubkey,
        intent_book: Pubkey,
        leverage_bps: u32,
        collateral_bucket: u64,
        twap_window_slots: u64,
        tick_fp: u64,
        liquidation_bps: u32,
        liquidation_bounty_bps: u16,
        max_staleness_secs: u64,
        reference_usd: u64,
        bump: u8,
    ) -> Result<(), ProgramError> {
        let c: &mut InstrumentConfig = pod::try_cast_mut(bytes)?;
        if c.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        if !source::is_known(source) {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        // PYTH instruments must declare a non-zero reference; POLYMARKET
        // instruments must leave it at zero (the off-chain attestor
        // ignores it for binary outcomes).
        match source {
            source::PYTH => {
                if reference_usd == 0 {
                    return Err(PolyleverageError::InvalidInstructionData.into());
                }
            }
            source::POLYMARKET => {
                if reference_usd != 0 {
                    return Err(PolyleverageError::InvalidInstructionData.into());
                }
            }
            _ => return Err(PolyleverageError::InvalidInstructionData.into()),
        }
        Self::validate_bucket_config(leverage_bps, collateral_bucket, tick_fp)?;
        *c = InstrumentConfig {
            discriminator: DISC_INSTRUMENT_CONFIG,
            status: STATUS_ACTIVE,
            bump,
            source,
            _pad0: [0; 4],
            market_id,
            source_token_id_a,
            source_token_id_b,
            collateral_mint,
            intent_book,
            leverage_bps,
            _pad1: [0; 4],
            collateral_bucket,
            twap_window_slots,
            tick_fp,
            liquidation_bps,
            liquidation_bounty_bps,
            _pad2: [0; 2],
            max_staleness_secs,
            reference_usd,
            _reserved: [0; 40],
        };
        Ok(())
    }
}
