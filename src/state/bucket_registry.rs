//! `BucketRegistry` — admin-managed allowlist of leverage values and
//! collateral bucket sizes that any `CreateInstrument` call must subset.
//!
//! Prior to v2.2, the allowed leverages were a hard-coded `&[u32]` constant
//! in the program binary (2×, 5×, 10×, 20×, 40×) — changing them required
//! a program upgrade. Collateral buckets had no on-chain restriction at
//! all, fragmenting liquidity arbitrarily.
//!
//! The registry is a singleton PDA seeded at `[SEED_BUCKET_REGISTRY]`.
//! It stores up to `MAX_LEVERAGE_SLOTS` leverages and
//! `MAX_COLLATERAL_SLOTS` bucket sizes. Only the first `n_leverage` /
//! `n_collateral` slots are authoritative. `version` is bumped on every
//! edit — useful for clients that cache the set off-chain.
//!
//! Default bootstrap: leverages `[2, 5, 10, 20, 40]×` (in bps), buckets
//! `[$100, $1 000]` in atoms assuming 6-decimal quote. Admins can set
//! their own list via `InitBucketRegistry` / `SetBucketRegistry`.
//!
//! `CreateInstrument` now reads this registry (account passed at ix time)
//! and rejects any `(leverage_bps, collateral_bucket)` not in the
//! superset.

use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_BUCKET_REGISTRY};

pub const DISC_BUCKET_REGISTRY: u8 = 15;

pub const MAX_LEVERAGE_SLOTS: usize = 16;
pub const MAX_COLLATERAL_SLOTS: usize = 16;

/// Default leverages in bps (2×, 5×, 10×, 20×, 40×).
pub const DEFAULT_LEVERAGE_BPS: &[u32] = &[20_000, 50_000, 100_000, 200_000, 400_000];

/// Default collateral buckets in atoms assuming a 6-decimal quote
/// ($100 = 100_000_000, $1 000 = 1_000_000_000).
pub const DEFAULT_COLLATERAL_BUCKETS: &[u64] = &[100_000_000, 1_000_000_000];

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct BucketRegistry {
    pub discriminator: u8,
    pub bump: u8,
    pub n_leverage: u8,
    pub n_collateral: u8,
    pub _pad0: [u8; 4],

    pub version: u64,

    /// Authoritative subrange is `[0..n_leverage]`. Any extra slots are 0.
    pub leverage_bps: [u32; MAX_LEVERAGE_SLOTS],
    /// Authoritative subrange is `[0..n_collateral]`. Any extra slots are 0.
    pub collateral_buckets: [u64; MAX_COLLATERAL_SLOTS],

    pub _reserved: [u8; 64],
}

pub const BUCKET_REGISTRY_LEN: usize =
    1 + 1 + 1 + 1 + 4 + 8 + 4 * MAX_LEVERAGE_SLOTS + 8 * MAX_COLLATERAL_SLOTS + 64;
const_assert_size!(BucketRegistry, BUCKET_REGISTRY_LEN);

impl BucketRegistry {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let r: &Self = pod::try_cast_ref(bytes)?;
        if r.discriminator != DISC_BUCKET_REGISTRY {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(r)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let r: &mut Self = pod::try_cast_mut(bytes)?;
        if r.discriminator != DISC_BUCKET_REGISTRY {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(r)
    }

    pub fn find_pda(program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_BUCKET_REGISTRY], program_id)
    }

    /// Validate a proposed (leverages, buckets) config.
    fn validate_inputs(leverages: &[u32], buckets: &[u64]) -> Result<(), ProgramError> {
        if leverages.is_empty() || leverages.len() > MAX_LEVERAGE_SLOTS {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        if buckets.is_empty() || buckets.len() > MAX_COLLATERAL_SLOTS {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        // Leverages: non-zero, no duplicates, ≤ 1000× (to catch fat-finger).
        for (i, l) in leverages.iter().enumerate() {
            if *l == 0 || *l > 10_000_000 {
                return Err(PolyleverageError::InvalidLeverageBucket.into());
            }
            for j in (i + 1)..leverages.len() {
                if leverages[j] == *l {
                    return Err(PolyleverageError::InvalidLeverageBucket.into());
                }
            }
        }
        // Buckets: non-zero, no duplicates.
        for (i, b) in buckets.iter().enumerate() {
            if *b == 0 {
                return Err(PolyleverageError::InvalidCollateralBucket.into());
            }
            for j in (i + 1)..buckets.len() {
                if buckets[j] == *b {
                    return Err(PolyleverageError::InvalidCollateralBucket.into());
                }
            }
        }
        Ok(())
    }

    pub fn init(
        bytes: &mut [u8],
        leverages: &[u32],
        buckets: &[u64],
        bump: u8,
    ) -> Result<(), ProgramError> {
        Self::validate_inputs(leverages, buckets)?;
        let r: &mut Self = pod::try_cast_mut(bytes)?;
        if r.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        let mut lev_arr = [0u32; MAX_LEVERAGE_SLOTS];
        for (slot, v) in lev_arr.iter_mut().zip(leverages.iter()) {
            *slot = *v;
        }
        let mut bucket_arr = [0u64; MAX_COLLATERAL_SLOTS];
        for (slot, v) in bucket_arr.iter_mut().zip(buckets.iter()) {
            *slot = *v;
        }
        *r = BucketRegistry {
            discriminator: DISC_BUCKET_REGISTRY,
            bump,
            n_leverage: leverages.len() as u8,
            n_collateral: buckets.len() as u8,
            _pad0: [0; 4],
            version: 1,
            leverage_bps: lev_arr,
            collateral_buckets: bucket_arr,
            _reserved: [0; 64],
        };
        Ok(())
    }

    pub fn set(&mut self, leverages: &[u32], buckets: &[u64]) -> Result<(), ProgramError> {
        Self::validate_inputs(leverages, buckets)?;
        self.leverage_bps = [0u32; MAX_LEVERAGE_SLOTS];
        for (slot, v) in self.leverage_bps.iter_mut().zip(leverages.iter()) {
            *slot = *v;
        }
        self.collateral_buckets = [0u64; MAX_COLLATERAL_SLOTS];
        for (slot, v) in self.collateral_buckets.iter_mut().zip(buckets.iter()) {
            *slot = *v;
        }
        self.n_leverage = leverages.len() as u8;
        self.n_collateral = buckets.len() as u8;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn allows_leverage(&self, bps: u32) -> bool {
        let n = (self.n_leverage as usize).min(MAX_LEVERAGE_SLOTS);
        self.leverage_bps[..n].iter().any(|v| *v == bps)
    }

    pub fn allows_bucket(&self, atoms: u64) -> bool {
        let n = (self.n_collateral as usize).min(MAX_COLLATERAL_SLOTS);
        self.collateral_buckets[..n].iter().any(|v| *v == atoms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Vec<u8> {
        vec![0u8; BUCKET_REGISTRY_LEN]
    }

    #[test]
    fn init_with_defaults_accepts_every_default() {
        let mut buf = fresh();
        BucketRegistry::init(
            &mut buf,
            DEFAULT_LEVERAGE_BPS,
            DEFAULT_COLLATERAL_BUCKETS,
            0,
        )
        .unwrap();
        let r = BucketRegistry::load(&buf).unwrap();
        for l in DEFAULT_LEVERAGE_BPS {
            assert!(r.allows_leverage(*l));
        }
        for b in DEFAULT_COLLATERAL_BUCKETS {
            assert!(r.allows_bucket(*b));
        }
        assert!(!r.allows_leverage(1_234_567));
        assert!(!r.allows_bucket(9_999));
    }

    #[test]
    fn rejects_duplicate_leverages() {
        let mut buf = fresh();
        let err = BucketRegistry::init(&mut buf, &[20_000, 20_000], &[100_000_000], 0)
            .unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidLeverageBucket as u32),
        );
    }

    #[test]
    fn rejects_duplicate_buckets() {
        let mut buf = fresh();
        let err = BucketRegistry::init(&mut buf, &[20_000], &[100_000_000, 100_000_000], 0)
            .unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidCollateralBucket as u32),
        );
    }

    #[test]
    fn rejects_zero_leverage() {
        let mut buf = fresh();
        let err = BucketRegistry::init(&mut buf, &[0, 20_000], &[100_000_000], 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidLeverageBucket as u32),
        );
    }

    #[test]
    fn rejects_empty_set() {
        let mut buf = fresh();
        let err = BucketRegistry::init(&mut buf, &[], &[100_000_000], 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidInstructionData as u32),
        );
    }

    #[test]
    fn set_bumps_version_and_replaces_set() {
        let mut buf = fresh();
        BucketRegistry::init(&mut buf, &[20_000, 50_000], &[100_000_000], 0).unwrap();
        {
            let r = BucketRegistry::load_mut(&mut buf).unwrap();
            r.set(&[100_000, 200_000, 400_000], &[500_000_000, 5_000_000_000])
                .unwrap();
        }
        let r = BucketRegistry::load(&buf).unwrap();
        assert_eq!(r.version, 2);
        assert!(!r.allows_leverage(20_000)); // removed
        assert!(r.allows_leverage(100_000)); // added
        assert!(r.allows_bucket(5_000_000_000));
    }
}
