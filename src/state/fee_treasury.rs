//! `FeeTreasury` — per-collateral-mint accrual ledger for taker fees.
//!
//! Tokens themselves live in the CollateralVault (same token account that holds
//! all user margin balances). This account is pure bookkeeping: how many atoms
//! of fees have accrued. On `SweepFees`, the admin can move `accrued_atoms` out
//! of the CollateralVault to a treasury ATA they designate.
//!
//! Spec §1.9.

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_TREASURY};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

pub const DISC_FEE_TREASURY: u8 = 12;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct FeeTreasury {
    pub discriminator: u8,
    pub bump: u8,
    pub _pad0: [u8; 6],

    pub collateral_mint: Pubkey,
    pub accrued_atoms: u64,
    pub total_swept_atoms: u64,

    pub _reserved: [u8; 32],
}

pub const FEE_TREASURY_LEN: usize = 1 + 1 + 6 + 32 + 8 + 8 + 32;
const_assert_size!(FeeTreasury, FEE_TREASURY_LEN);

impl FeeTreasury {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let t: &Self = pod::try_cast_ref(bytes)?;
        if t.discriminator != DISC_FEE_TREASURY {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(t)
    }
    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let t: &mut Self = pod::try_cast_mut(bytes)?;
        if t.discriminator != DISC_FEE_TREASURY {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(t)
    }

    pub fn init(bytes: &mut [u8], collateral_mint: Pubkey, bump: u8) -> Result<(), ProgramError> {
        let t: &mut Self = pod::try_cast_mut(bytes)?;
        if t.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        *t = FeeTreasury {
            discriminator: DISC_FEE_TREASURY,
            bump,
            _pad0: [0; 6],
            collateral_mint,
            accrued_atoms: 0,
            total_swept_atoms: 0,
            _reserved: [0; 32],
        };
        Ok(())
    }

    pub fn find_pda(program_id: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_TREASURY, mint.as_ref()], program_id)
    }

    pub fn accrue(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.accrued_atoms = self
            .accrued_atoms
            .checked_add(amount)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn sweep(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.accrued_atoms = self
            .accrued_atoms
            .checked_sub(amount)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        self.total_swept_atoms = self
            .total_swept_atoms
            .checked_add(amount)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(())
    }
}
