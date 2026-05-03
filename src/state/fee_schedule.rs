//! `FeeSchedule` — volume-tiered taker fee table. Singleton PDA, admin-managed.
//!
//! Up to 8 tiers, sorted ascending by `volume_threshold_atoms`. A trader's
//! 30-day rolling volume (quote atoms) is compared against thresholds to find
//! their applicable tier; the fee_bps at that tier applies.
//!
//! Spec §8.

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_FEE_SCHEDULE};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

pub const FEE_SCHEDULE_MAX_TIERS: usize = 8;

pub const DISC_FEE_SCHEDULE: u8 = 10;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct FeeTierRaw {
    pub volume_threshold: u64,
    pub fee_bps: u16,
    pub _pad: [u8; 6],
}

pub const FEE_TIER_LEN: usize = 8 + 2 + 6;
const_assert_size!(FeeTierRaw, FEE_TIER_LEN);

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct FeeSchedule {
    pub discriminator: u8,
    pub bump: u8,
    pub _pad0: [u8; 6],

    pub tiers: [FeeTierRaw; FEE_SCHEDULE_MAX_TIERS],

    pub max_tier_fee_bps: u16,
    pub _pad1: [u8; 6],

    pub last_updated_slot: u64,

    pub _reserved: [u8; 32],
}

pub const FEE_SCHEDULE_LEN: usize =
    1 + 1 + 6 + (FEE_SCHEDULE_MAX_TIERS * FEE_TIER_LEN) + 2 + 6 + 8 + 32;
const_assert_size!(FeeSchedule, FEE_SCHEDULE_LEN);

impl FeeSchedule {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let s: &Self = pod::try_cast_ref(bytes)?;
        if s.discriminator != DISC_FEE_SCHEDULE {
            return Err(PolyleverageError::FeeScheduleNotInitialized.into());
        }
        Ok(s)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let s: &mut Self = pod::try_cast_mut(bytes)?;
        if s.discriminator != DISC_FEE_SCHEDULE {
            return Err(PolyleverageError::FeeScheduleNotInitialized.into());
        }
        Ok(s)
    }

    pub fn init(
        bytes: &mut [u8],
        tiers: &[FeeTierRaw; FEE_SCHEDULE_MAX_TIERS],
        now_slot: u64,
        bump: u8,
    ) -> Result<(), ProgramError> {
        let s: &mut Self = pod::try_cast_mut(bytes)?;
        if s.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        let max = compute_max_fee_bps(tiers);
        *s = FeeSchedule {
            discriminator: DISC_FEE_SCHEDULE,
            bump,
            _pad0: [0; 6],
            tiers: *tiers,
            max_tier_fee_bps: max,
            _pad1: [0; 6],
            last_updated_slot: now_slot,
            _reserved: [0; 32],
        };
        Ok(())
    }

    pub fn find_pda(program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_FEE_SCHEDULE], program_id)
    }

    /// Look up fee_bps for a user's rolling 30d volume.
    pub fn fee_for_volume(&self, volume_30d: u64) -> u16 {
        // Tiers assumed sorted ascending by threshold. Walk in reverse to find the
        // largest threshold ≤ volume_30d.
        let mut found = self.tiers[0].fee_bps;
        for t in self.tiers.iter() {
            if volume_30d >= t.volume_threshold {
                found = t.fee_bps;
            }
        }
        found
    }
}

/// Compute max fee across all tier entries (skipping sentinel-high thresholds as
/// "unused" is represented by threshold = u64::MAX; still, the fee_bps at those
/// entries is counted in the cap since it's a hard-allowable value).
pub fn compute_max_fee_bps(tiers: &[FeeTierRaw; FEE_SCHEDULE_MAX_TIERS]) -> u16 {
    let mut max = 0u16;
    for t in tiers.iter() {
        if t.fee_bps > max {
            max = t.fee_bps;
        }
    }
    max
}

/// Validate a new tier table: ascending thresholds, reasonable fee caps.
pub fn validate_tiers(
    tiers: &[FeeTierRaw; FEE_SCHEDULE_MAX_TIERS],
) -> Result<(), ProgramError> {
    // Monotonic non-decreasing on volume_threshold.
    let mut prev: u64 = 0;
    for t in tiers.iter() {
        if t.volume_threshold < prev {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        prev = t.volume_threshold;
        if t.fee_bps > 1_000 {
            // Hard cap: no tier > 10% (absurdly high). Defence against admin bugs.
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_tiers(pairs: &[(u64, u16)]) -> [FeeTierRaw; FEE_SCHEDULE_MAX_TIERS] {
        let mut out = [FeeTierRaw {
            volume_threshold: u64::MAX,
            fee_bps: 0,
            _pad: [0; 6],
        }; FEE_SCHEDULE_MAX_TIERS];
        for (i, (vol, bps)) in pairs.iter().enumerate() {
            out[i] = FeeTierRaw {
                volume_threshold: *vol,
                fee_bps: *bps,
                _pad: [0; 6],
            };
        }
        out
    }

    #[test]
    fn fee_for_volume_picks_largest_threshold_below_volume() {
        let mut buf = vec![0u8; FEE_SCHEDULE_LEN];
        FeeSchedule::init(
            &mut buf,
            &mk_tiers(&[(0, 20), (1_000, 15), (10_000, 10)]),
            0,
            0,
        )
        .unwrap();
        let s = FeeSchedule::load(&buf).unwrap();

        assert_eq!(s.fee_for_volume(0), 20);
        assert_eq!(s.fee_for_volume(999), 20);
        assert_eq!(s.fee_for_volume(1_000), 15);
        assert_eq!(s.fee_for_volume(9_999), 15);
        assert_eq!(s.fee_for_volume(10_000), 10);
        assert_eq!(s.fee_for_volume(1_000_000), 10);
    }

    #[test]
    fn max_fee_bps_is_cached_at_init() {
        let mut buf = vec![0u8; FEE_SCHEDULE_LEN];
        FeeSchedule::init(
            &mut buf,
            &mk_tiers(&[(0, 20), (1_000, 30), (10_000, 10)]),
            0,
            0,
        )
        .unwrap();
        let s = FeeSchedule::load(&buf).unwrap();
        assert_eq!(s.max_tier_fee_bps, 30);
    }

    #[test]
    fn validate_rejects_descending_thresholds() {
        let bad = mk_tiers(&[(100, 5), (50, 3)]);
        assert!(validate_tiers(&bad).is_err());
    }

    #[test]
    fn validate_rejects_fee_above_10pct() {
        let bad = mk_tiers(&[(0, 1_001)]);
        assert!(validate_tiers(&bad).is_err());
    }

    #[test]
    fn validate_accepts_all_zero_tiers() {
        let tiers = [FeeTierRaw {
            volume_threshold: 0,
            fee_bps: 0,
            _pad: [0; 6],
        }; FEE_SCHEDULE_MAX_TIERS];
        assert!(validate_tiers(&tiers).is_ok());
    }
}
