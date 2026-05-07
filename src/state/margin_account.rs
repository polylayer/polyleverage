//! `MarginAccount` — one per (user, collateral_mint). Global across all instruments
//! that share the mint.
//!
//! Spec §1.2

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_MARGIN};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use super::DISC_MARGIN_ACCOUNT;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct MarginAccount {
    pub discriminator: u8,
    pub bump: u8,
    pub _pad0: [u8; 6],

    pub owner: Pubkey,
    pub collateral_mint: Pubkey,

    /// Withdrawable, usable for new intents.
    pub free_collateral: u64,
    /// Locked in open intents (collateral + fee buffer; see spec §8).
    pub reserved_collateral: u64,
    /// Locked in live PMLCs.
    pub locked_collateral: u64,

    /// Cumulative realized PnL — display only, never used for invariants.
    pub realized_pnl: i64,

    pub flags: u64,
    pub _reserved: [u8; 32],
}

pub const MARGIN_ACCOUNT_LEN: usize = 1 + 1 + 6 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 32;
const_assert_size!(MarginAccount, MARGIN_ACCOUNT_LEN);

impl MarginAccount {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let a: &MarginAccount = pod::try_cast_ref(bytes)?;
        if a.discriminator != DISC_MARGIN_ACCOUNT {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(a)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let a: &mut MarginAccount = pod::try_cast_mut(bytes)?;
        if a.discriminator != DISC_MARGIN_ACCOUNT {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(a)
    }

    pub fn init(
        bytes: &mut [u8],
        owner: Pubkey,
        collateral_mint: Pubkey,
        bump: u8,
    ) -> Result<(), ProgramError> {
        let a: &mut MarginAccount = pod::try_cast_mut(bytes)?;
        if a.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        *a = MarginAccount {
            discriminator: DISC_MARGIN_ACCOUNT,
            bump,
            _pad0: [0; 6],
            owner,
            collateral_mint,
            free_collateral: 0,
            reserved_collateral: 0,
            locked_collateral: 0,
            realized_pnl: 0,
            flags: 0,
            _reserved: [0; 32],
        };
        Ok(())
    }

    pub fn find_pda(program_id: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_MARGIN, owner.as_ref(), mint.as_ref()], program_id)
    }

    // --- Safe arithmetic helpers ------------------------------------------

    pub fn credit_free(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.free_collateral = self
            .free_collateral
            .checked_add(amount)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn debit_free(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.free_collateral = self
            .free_collateral
            .checked_sub(amount)
            .ok_or(PolyleverageError::InsufficientFreeCollateral)?;
        Ok(())
    }

    pub fn credit_reserved(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.reserved_collateral = self
            .reserved_collateral
            .checked_add(amount)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn debit_reserved(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.reserved_collateral = self
            .reserved_collateral
            .checked_sub(amount)
            .ok_or(PolyleverageError::InsufficientReservedCollateral)?;
        Ok(())
    }

    pub fn credit_locked(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.locked_collateral = self
            .locked_collateral
            .checked_add(amount)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn debit_locked(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.locked_collateral = self
            .locked_collateral
            .checked_sub(amount)
            .ok_or(PolyleverageError::InsufficientLockedCollateral)?;
        Ok(())
    }

    /// Move `amount` from free to reserved. Fails if insufficient free.
    pub fn move_free_to_reserved(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.debit_free(amount)?;
        self.credit_reserved(amount)?;
        Ok(())
    }

    /// Move `amount` from reserved to free.
    pub fn move_reserved_to_free(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.debit_reserved(amount)?;
        self.credit_free(amount)?;
        Ok(())
    }

    /// Move `amount` from reserved to locked.
    pub fn move_reserved_to_locked(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.debit_reserved(amount)?;
        self.credit_locked(amount)?;
        Ok(())
    }

    /// Move `amount` from locked to free.
    pub fn move_locked_to_free(&mut self, amount: u64) -> Result<(), ProgramError> {
        self.debit_locked(amount)?;
        self.credit_free(amount)?;
        Ok(())
    }
}
