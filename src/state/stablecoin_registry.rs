//! `StablecoinRegistry` — admin-managed allowlist of USD-pegged SPL mints
//! that can be wrapped 1:1 into the program-owned `wUSD` token.
//!
//! Rationale: rather than deploying a separate intent book per
//! `(polymarket, leverage, bucket, stablecoin_mint)`, all positions are
//! denominated in a single canonical mint (`wUSD`). Users wrap whatever
//! stablecoin they hold (USDC, USDT, PYUSD, …) into `wUSD` 1:1, trade
//! against a unified order book, and unwrap back on exit.
//!
//! Fragmentation risk and peg risk:
//! - One book per (polymarket, leverage, bucket) → deep liquidity.
//! - Peg risk is pooled: if one accepted stablecoin depeg's, arbitrage
//!   drains the reserve of the healthy coin. Mitigation is admin-side
//!   — pause / remove the depegged mint via `RemoveStablecoinMint`.
//!   Existing wUSD holders can still unwrap to whatever is in reserve.
//!
//! Default bootstrap mints: USDC, USDT. More can be added via
//! `AddStablecoinMint { mint }` (admin- or multisig-gated).

use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_STABLECOIN_REGISTRY};

pub const DISC_STABLECOIN_REGISTRY: u8 = 16;

pub const MAX_STABLECOIN_SLOTS: usize = 16;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct StablecoinRegistry {
    pub discriminator: u8,
    pub bump: u8,
    pub n_mints: u8,
    /// Decimals of `wUSD` (must match each accepted mint's decimals —
    /// enforced at `AddStablecoinMint` time so wrap/unwrap can be 1:1
    /// atom-for-atom).
    pub wusd_decimals: u8,
    pub _pad0: [u8; 4],

    /// Program-owned wUSD SPL mint PDA key.
    pub wusd_mint: Pubkey,

    /// Version counter — bumped on every Add/Remove.
    pub version: u64,

    /// Active mints live in `[0..n_mints]`; trailing slots are zeroed.
    pub mints: [Pubkey; MAX_STABLECOIN_SLOTS],

    pub _reserved: [u8; 64],
}

pub const STABLECOIN_REGISTRY_LEN: usize =
    1 + 1 + 1 + 1 + 4 + 32 + 8 + 32 * MAX_STABLECOIN_SLOTS + 64;
const_assert_size!(StablecoinRegistry, STABLECOIN_REGISTRY_LEN);

impl StablecoinRegistry {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let r: &Self = pod::try_cast_ref(bytes)?;
        if r.discriminator != DISC_STABLECOIN_REGISTRY {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(r)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let r: &mut Self = pod::try_cast_mut(bytes)?;
        if r.discriminator != DISC_STABLECOIN_REGISTRY {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(r)
    }

    pub fn find_pda(program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_STABLECOIN_REGISTRY], program_id)
    }

    pub fn init(
        bytes: &mut [u8],
        wusd_mint: Pubkey,
        wusd_decimals: u8,
        initial_mints: &[Pubkey],
        bump: u8,
    ) -> Result<(), ProgramError> {
        if initial_mints.len() > MAX_STABLECOIN_SLOTS {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        for (i, m) in initial_mints.iter().enumerate() {
            if *m == Pubkey::default() {
                return Err(PolyleverageError::InvalidMint.into());
            }
            for j in (i + 1)..initial_mints.len() {
                if initial_mints[j] == *m {
                    return Err(PolyleverageError::InvalidMint.into());
                }
            }
        }
        let r: &mut Self = pod::try_cast_mut(bytes)?;
        if r.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        let mut m_arr = [Pubkey::default(); MAX_STABLECOIN_SLOTS];
        for (slot, v) in m_arr.iter_mut().zip(initial_mints.iter()) {
            *slot = *v;
        }
        *r = StablecoinRegistry {
            discriminator: DISC_STABLECOIN_REGISTRY,
            bump,
            n_mints: initial_mints.len() as u8,
            wusd_decimals,
            _pad0: [0; 4],
            wusd_mint,
            version: 1,
            mints: m_arr,
            _reserved: [0; 64],
        };
        Ok(())
    }

    pub fn is_accepted(&self, mint: &Pubkey) -> bool {
        let n = (self.n_mints as usize).min(MAX_STABLECOIN_SLOTS);
        self.mints[..n].iter().any(|m| m == mint)
    }

    pub fn add_mint(&mut self, mint: Pubkey) -> Result<(), ProgramError> {
        if self.is_accepted(&mint) {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        let n = self.n_mints as usize;
        if n >= MAX_STABLECOIN_SLOTS {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        self.mints[n] = mint;
        self.n_mints = (n + 1) as u8;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn remove_mint(&mut self, mint: &Pubkey) -> Result<(), ProgramError> {
        let n = self.n_mints as usize;
        let mut idx: Option<usize> = None;
        for i in 0..n {
            if self.mints[i] == *mint {
                idx = Some(i);
                break;
            }
        }
        let i = idx.ok_or(PolyleverageError::NotInitialized)?;
        // Move the last element into the gap so the prefix stays dense.
        if i + 1 < n {
            self.mints[i] = self.mints[n - 1];
        }
        self.mints[n - 1] = Pubkey::default();
        self.n_mints = (n - 1) as u8;
        self.version = self.version.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Vec<u8> {
        vec![0u8; STABLECOIN_REGISTRY_LEN]
    }

    #[test]
    fn init_and_lookup() {
        let mut buf = fresh();
        let wusd = Pubkey::new_unique();
        let usdc = Pubkey::new_unique();
        let usdt = Pubkey::new_unique();
        StablecoinRegistry::init(&mut buf, wusd, 6, &[usdc, usdt], 0).unwrap();
        let r = StablecoinRegistry::load(&buf).unwrap();
        assert!(r.is_accepted(&usdc));
        assert!(r.is_accepted(&usdt));
        assert!(!r.is_accepted(&Pubkey::new_unique()));
        assert_eq!(r.wusd_mint, wusd);
        assert_eq!(r.wusd_decimals, 6);
        assert_eq!(r.n_mints, 2);
        assert_eq!(r.version, 1);
    }

    #[test]
    fn rejects_duplicates_at_init() {
        let mut buf = fresh();
        let a = Pubkey::new_unique();
        let err =
            StablecoinRegistry::init(&mut buf, Pubkey::new_unique(), 6, &[a, a], 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidMint as u32)
        );
    }

    #[test]
    fn add_and_remove_mints() {
        let mut buf = fresh();
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let c = Pubkey::new_unique();
        StablecoinRegistry::init(&mut buf, Pubkey::new_unique(), 6, &[a, b], 0).unwrap();
        {
            let r = StablecoinRegistry::load_mut(&mut buf).unwrap();
            r.add_mint(c).unwrap();
        }
        {
            let r = StablecoinRegistry::load(&buf).unwrap();
            assert!(r.is_accepted(&c));
            assert_eq!(r.n_mints, 3);
            assert_eq!(r.version, 2);
        }
        // Removing a leaves [c, b] or [b, c] (order not guaranteed).
        {
            let r = StablecoinRegistry::load_mut(&mut buf).unwrap();
            r.remove_mint(&a).unwrap();
        }
        let r = StablecoinRegistry::load(&buf).unwrap();
        assert!(!r.is_accepted(&a));
        assert!(r.is_accepted(&b));
        assert!(r.is_accepted(&c));
        assert_eq!(r.n_mints, 2);
        assert_eq!(r.version, 3);
    }

    #[test]
    fn add_rejects_duplicate() {
        let mut buf = fresh();
        let a = Pubkey::new_unique();
        StablecoinRegistry::init(&mut buf, Pubkey::new_unique(), 6, &[a], 0).unwrap();
        let r = StablecoinRegistry::load_mut(&mut buf).unwrap();
        assert!(r.add_mint(a).is_err());
    }

    #[test]
    fn remove_rejects_unknown() {
        let mut buf = fresh();
        let a = Pubkey::new_unique();
        StablecoinRegistry::init(&mut buf, Pubkey::new_unique(), 6, &[a], 0).unwrap();
        let r = StablecoinRegistry::load_mut(&mut buf).unwrap();
        assert!(r.remove_mint(&Pubkey::new_unique()).is_err());
    }
}
