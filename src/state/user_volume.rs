//! `UserVolume` — per-(user, collateral_mint) rolling 30-day volume counter.
//!
//! Implementation uses a two-epoch scheme: `current_epoch` + `prev_epoch`.
//! Epoch length is 30 days in slots. On each volume contribution we check
//! if the current epoch has rolled; if yes, shift `current → prev` and start
//! a fresh `current`.
//!
//! Effective 30-day volume returned to callers is
//! `max(current, prev)`, which biases toward "recent" and avoids gaming
//! via epoch-boundary timing.
//!
//! Spec §8.2 / §1.8.

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_USER_VOLUME};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

pub const DISC_USER_VOLUME: u8 = 11;

/// 30 days of slots at ~0.4s/slot = 6_480_000. Conservative.
pub const USER_VOLUME_EPOCH_SLOTS: u64 = 6_480_000;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct UserVolume {
    pub discriminator: u8,
    pub bump: u8,
    pub _pad0: [u8; 6],

    pub owner: Pubkey,
    pub collateral_mint: Pubkey,

    pub volume_current_atoms: u64,
    pub volume_prev_atoms: u64,

    pub epoch_start_slot: u64,
    pub epoch_length_slots: u64,

    pub _reserved: [u8; 32],
}

pub const USER_VOLUME_LEN: usize = 1 + 1 + 6 + 32 + 32 + 8 + 8 + 8 + 8 + 32;
const_assert_size!(UserVolume, USER_VOLUME_LEN);

impl UserVolume {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let v: &Self = pod::try_cast_ref(bytes)?;
        if v.discriminator != DISC_USER_VOLUME {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(v)
    }
    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let v: &mut Self = pod::try_cast_mut(bytes)?;
        if v.discriminator != DISC_USER_VOLUME {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(v)
    }

    pub fn init(
        bytes: &mut [u8],
        owner: Pubkey,
        collateral_mint: Pubkey,
        now_slot: u64,
        bump: u8,
    ) -> Result<(), ProgramError> {
        let v: &mut Self = pod::try_cast_mut(bytes)?;
        if v.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        *v = UserVolume {
            discriminator: DISC_USER_VOLUME,
            bump,
            _pad0: [0; 6],
            owner,
            collateral_mint,
            volume_current_atoms: 0,
            volume_prev_atoms: 0,
            epoch_start_slot: now_slot,
            epoch_length_slots: USER_VOLUME_EPOCH_SLOTS,
            _reserved: [0; 32],
        };
        Ok(())
    }

    pub fn find_pda(program_id: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[SEED_USER_VOLUME, owner.as_ref(), mint.as_ref()],
            program_id,
        )
    }

    /// Add `amount` to the current epoch after rolling epochs if needed.
    pub fn add_volume(&mut self, amount: u64, now_slot: u64) -> Result<(), ProgramError> {
        self.maybe_roll_epoch(now_slot)?;
        self.volume_current_atoms = self
            .volume_current_atoms
            .checked_add(amount)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(())
    }

    /// Returns the larger of current and previous epoch totals.
    pub fn effective_30d_volume(&self, now_slot: u64) -> u64 {
        let mut v = *self;
        let _ = v.maybe_roll_epoch(now_slot);
        v.volume_current_atoms.max(v.volume_prev_atoms)
    }

    fn maybe_roll_epoch(&mut self, now_slot: u64) -> Result<(), ProgramError> {
        let length = self.epoch_length_slots.max(1);
        if now_slot < self.epoch_start_slot {
            return Ok(()); // clock skew — don't mutate
        }
        let elapsed = now_slot - self.epoch_start_slot;
        if elapsed < length {
            return Ok(());
        }
        // Roll at least one epoch.
        let epochs_elapsed = elapsed / length;
        if epochs_elapsed == 1 {
            self.volume_prev_atoms = self.volume_current_atoms;
            self.volume_current_atoms = 0;
        } else {
            // Two or more epochs passed → both windows are stale.
            self.volume_prev_atoms = 0;
            self.volume_current_atoms = 0;
        }
        self.epoch_start_slot = self
            .epoch_start_slot
            .checked_add(
                epochs_elapsed
                    .checked_mul(length)
                    .ok_or(PolyleverageError::ArithmeticOverflow)?,
            )
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::pubkey::Pubkey;

    fn fresh(now: u64) -> Vec<u8> {
        let mut buf = vec![0u8; USER_VOLUME_LEN];
        UserVolume::init(&mut buf, Pubkey::new_unique(), Pubkey::new_unique(), now, 0).unwrap();
        buf
    }

    #[test]
    fn add_volume_stays_in_current_epoch() {
        let mut buf = fresh(1_000);
        let v = UserVolume::load_mut(&mut buf).unwrap();
        v.add_volume(500, 1_000).unwrap();
        v.add_volume(300, 1_500).unwrap();
        assert_eq!(v.volume_current_atoms, 800);
        assert_eq!(v.volume_prev_atoms, 0);
    }

    #[test]
    fn roll_once_moves_current_to_prev() {
        let mut buf = fresh(0);
        let v = UserVolume::load_mut(&mut buf).unwrap();
        v.add_volume(1_000, 0).unwrap();
        let later = USER_VOLUME_EPOCH_SLOTS + 100;
        v.add_volume(500, later).unwrap();
        assert_eq!(v.volume_prev_atoms, 1_000);
        assert_eq!(v.volume_current_atoms, 500);
    }

    #[test]
    fn roll_many_epochs_wipes_both_windows() {
        let mut buf = fresh(0);
        let v = UserVolume::load_mut(&mut buf).unwrap();
        v.add_volume(1_000, 0).unwrap();
        let far_future = USER_VOLUME_EPOCH_SLOTS * 5 + 100;
        v.add_volume(42, far_future).unwrap();
        assert_eq!(v.volume_prev_atoms, 0);
        assert_eq!(v.volume_current_atoms, 42);
    }

    #[test]
    fn effective_30d_takes_max_of_current_and_prev() {
        let mut buf = fresh(0);
        {
            let v = UserVolume::load_mut(&mut buf).unwrap();
            v.add_volume(1_000, 0).unwrap();
            // Roll: current (1000) → prev. Then add 200 to new current.
            v.add_volume(200, USER_VOLUME_EPOCH_SLOTS + 50).unwrap();
        }
        let v = UserVolume::load(&buf).unwrap();
        assert_eq!(v.effective_30d_volume(USER_VOLUME_EPOCH_SLOTS + 100), 1_000);
    }
}
