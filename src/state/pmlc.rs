//! `Pmlc` — one PDA per live pairwise matched leverage contract.
//!
//! Spec §1.6

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_PMLC};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use super::DISC_PMLC;

pub const PMLC_STATUS_LIVE: u8 = 0;
pub const PMLC_STATUS_CLOSED: u8 = 1;
pub const PMLC_STATUS_LIQUIDATED: u8 = 2;
pub const PMLC_STATUS_RESOLVED: u8 = 3;

// Close reasons.
pub const CLOSE_REASON_NONE: u8 = 0;
pub const CLOSE_REASON_LIQUIDATED: u8 = 1;
pub const CLOSE_REASON_RESOLVED: u8 = 2;
pub const CLOSE_REASON_MUTUAL: u8 = 3;

pub const PMLC_FLAG_LONG_REENTRY: u8 = 0b0000_0001;
pub const PMLC_FLAG_SHORT_REENTRY: u8 = 0b0000_0010;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct Pmlc {
    pub discriminator: u8,
    pub status: u8,
    pub close_reason: u8,
    pub flags: u8,
    pub bump: u8,
    pub _pad0: [u8; 3],

    pub id: u64,

    pub instrument: Pubkey,
    pub collateral_mint: Pubkey,
    pub market_id: [u8; 32],

    pub long_owner: Pubkey,
    pub short_owner: Pubkey,

    pub collateral_per_side: u64,
    pub leverage_bps: u32,
    pub _pad1: [u8; 4],

    pub entry_price_fp: u64,
    /// Notional atoms (= collateral × leverage).
    pub notional_atoms: u64,
    /// Position size as fp with 18 decimals (= notional × 1e18 / entry). Stored as u128.
    pub size_fp_lo: u64,
    pub size_fp_hi: u64,

    pub opened_slot: u64,
    pub last_mark_slot: u64,

    /// Reentry entry-range for the long side (valid iff flags & LONG_REENTRY).
    pub long_reentry_min_fp: u64,
    pub long_reentry_max_fp: u64,
    /// Reentry entry-range for the short side (valid iff flags & SHORT_REENTRY).
    pub short_reentry_min_fp: u64,
    pub short_reentry_max_fp: u64,

    pub _reserved: [u8; 32],
}

pub const PMLC_LEN: usize = 1
    + 1
    + 1
    + 1
    + 1
    + 3
    + 8
    + 32
    + 32
    + 32
    + 32
    + 32
    + 8
    + 4
    + 4
    + 8
    + 8
    + 8
    + 8
    + 8
    + 8
    + 8
    + 8
    + 8
    + 8
    + 32;
const_assert_size!(Pmlc, PMLC_LEN);

impl Pmlc {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let p: &Pmlc = pod::try_cast_ref(bytes)?;
        if p.discriminator != DISC_PMLC {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(p)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let p: &mut Pmlc = pod::try_cast_mut(bytes)?;
        if p.discriminator != DISC_PMLC {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(p)
    }

    pub fn find_pda(program_id: &Pubkey, instrument: &Pubkey, id: u64) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[SEED_PMLC, instrument.as_ref(), &id.to_le_bytes()],
            program_id,
        )
    }

    pub fn size_fp(&self) -> u128 {
        ((self.size_fp_hi as u128) << 64) | (self.size_fp_lo as u128)
    }

    pub fn set_size_fp(&mut self, value: u128) {
        self.size_fp_lo = value as u64;
        self.size_fp_hi = (value >> 64) as u64;
    }

    pub fn require_live(&self) -> Result<(), ProgramError> {
        if self.status != PMLC_STATUS_LIVE {
            return Err(PolyleverageError::PmlcNotLive.into());
        }
        Ok(())
    }
}
