//! `AdminMultisig` — optional N-of-M admin multisignature account.
//!
//! When a program is bootstrapped, `ProgramConfig.admin` is a single pubkey.
//! After `InitAdminMultisig`, the singleton `AdminMultisig` PDA is created
//! and `ProgramConfig.admin_multisig` is set to its key. From that point
//! on the `authorize_admin_or_multisig()` helper (see `processor::admin`)
//! requires at least `threshold` of the listed signer pubkeys to sign a
//! tx before any admin-gated instruction will succeed.
//!
//! Spec §20.8 (v2 addendum).
//!
//! Layout is fixed-size Pod — 16 signer slots is a deliberate cap: at
//! 32 B/signer, this keeps the account under 600 B and the verifier
//! loop cheap (O(n_signers × n_signer_accounts), both small).

use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_ADMIN_MULTISIG};

pub const DISC_ADMIN_MULTISIG: u8 = 14;

/// Maximum number of signers allowed. Picked to keep the account
/// rent-cheap while still accommodating realistic multisigs.
pub const MAX_MULTISIG_SIGNERS: usize = 16;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct AdminMultisig {
    pub discriminator: u8,
    pub bump: u8,
    /// Approvals required to pass the check. Must satisfy
    /// `1 <= threshold <= n_signers <= MAX_MULTISIG_SIGNERS`.
    pub threshold: u8,
    /// Active signer count — only the first `n_signers` entries of
    /// `signers` are valid.
    pub n_signers: u8,
    pub _pad0: [u8; 4],

    /// Singleton pubkey that seeded the multisig (kept for audit).
    pub bootstrap_admin: Pubkey,
    /// Monotonic counter — bumped on every config change.
    pub version: u64,

    /// Signer pubkeys. Only `signers[..n_signers]` is authoritative.
    pub signers: [Pubkey; MAX_MULTISIG_SIGNERS],

    pub _reserved: [u8; 64],
}

pub const ADMIN_MULTISIG_LEN: usize = 1 + 1 + 1 + 1 + 4 + 32 + 8 + 32 * MAX_MULTISIG_SIGNERS + 64;
const_assert_size!(AdminMultisig, ADMIN_MULTISIG_LEN);

impl AdminMultisig {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let v: &Self = pod::try_cast_ref(bytes)?;
        if v.discriminator != DISC_ADMIN_MULTISIG {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(v)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let v: &mut Self = pod::try_cast_mut(bytes)?;
        if v.discriminator != DISC_ADMIN_MULTISIG {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(v)
    }

    pub fn init(
        bytes: &mut [u8],
        bootstrap_admin: Pubkey,
        signers: &[Pubkey],
        threshold: u8,
        bump: u8,
    ) -> Result<(), ProgramError> {
        if signers.is_empty() || signers.len() > MAX_MULTISIG_SIGNERS {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        if threshold == 0 || (threshold as usize) > signers.len() {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        // Reject duplicates: threshold N-of-M is meaningless if one key can
        // sign twice.
        for i in 0..signers.len() {
            for j in (i + 1)..signers.len() {
                if signers[i] == signers[j] {
                    return Err(PolyleverageError::InvalidInstructionData.into());
                }
            }
        }
        let v: &mut Self = pod::try_cast_mut(bytes)?;
        if v.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        *v = AdminMultisig {
            discriminator: DISC_ADMIN_MULTISIG,
            bump,
            threshold,
            n_signers: signers.len() as u8,
            _pad0: [0; 4],
            bootstrap_admin,
            version: 1,
            signers: {
                let mut buf = [Pubkey::default(); MAX_MULTISIG_SIGNERS];
                for (slot, s) in buf.iter_mut().zip(signers.iter()) {
                    *slot = *s;
                }
                buf
            },
            _reserved: [0; 64],
        };
        Ok(())
    }

    pub fn find_pda(program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_ADMIN_MULTISIG], program_id)
    }

    /// Is `key` in the active signer set?
    pub fn is_member(&self, key: &Pubkey) -> bool {
        let n = (self.n_signers as usize).min(MAX_MULTISIG_SIGNERS);
        self.signers[..n].iter().any(|s| s == key)
    }

    /// Count distinct-by-key signers in `accounts` that are members and
    /// have `is_signer == true`.
    pub fn count_approvals(&self, accounts: &[solana_program::account_info::AccountInfo]) -> u8 {
        let mut matched: u8 = 0;
        let n = (self.n_signers as usize).min(MAX_MULTISIG_SIGNERS);
        for s in &self.signers[..n] {
            for ai in accounts {
                if ai.is_signer && ai.key == s {
                    matched = matched.saturating_add(1);
                    break;
                }
            }
        }
        matched
    }

    /// Return Ok(()) if `accounts` contain at least `threshold` signers
    /// from this multisig. Error otherwise.
    pub fn check_threshold(
        &self,
        accounts: &[solana_program::account_info::AccountInfo],
    ) -> Result<(), ProgramError> {
        if self.count_approvals(accounts) >= self.threshold {
            Ok(())
        } else {
            Err(PolyleverageError::InsufficientMultisigApprovals.into())
        }
    }

    /// Apply a config change in-place. Caller must have verified a
    /// quorum on the old config first.
    pub fn rotate(&mut self, signers: &[Pubkey], threshold: u8) -> Result<(), ProgramError> {
        if signers.is_empty() || signers.len() > MAX_MULTISIG_SIGNERS {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        if threshold == 0 || (threshold as usize) > signers.len() {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        for i in 0..signers.len() {
            for j in (i + 1)..signers.len() {
                if signers[i] == signers[j] {
                    return Err(PolyleverageError::InvalidInstructionData.into());
                }
            }
        }
        self.signers = [Pubkey::default(); MAX_MULTISIG_SIGNERS];
        for (slot, s) in self.signers.iter_mut().zip(signers.iter()) {
            *slot = *s;
        }
        self.n_signers = signers.len() as u8;
        self.threshold = threshold;
        self.version = self.version.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::pubkey::Pubkey;

    fn fresh_buf() -> Vec<u8> {
        vec![0u8; ADMIN_MULTISIG_LEN]
    }

    #[test]
    fn init_accepts_valid_config() {
        let mut buf = fresh_buf();
        let bootstrap = Pubkey::new_unique();
        let signers = vec![
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        AdminMultisig::init(&mut buf, bootstrap, &signers, 2, 0).unwrap();
        let ms = AdminMultisig::load(&buf).unwrap();
        assert_eq!(ms.threshold, 2);
        assert_eq!(ms.n_signers, 3);
        assert_eq!(ms.version, 1);
        assert_eq!(ms.bootstrap_admin, bootstrap);
        for (i, s) in signers.iter().enumerate() {
            assert_eq!(&ms.signers[i], s);
            assert!(ms.is_member(s));
        }
        assert!(!ms.is_member(&Pubkey::new_unique()));
    }

    #[test]
    fn init_rejects_zero_signers() {
        let mut buf = fresh_buf();
        let err = AdminMultisig::init(&mut buf, Pubkey::new_unique(), &[], 1, 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidInstructionData as u32)
        );
    }

    #[test]
    fn init_rejects_threshold_greater_than_signers() {
        let mut buf = fresh_buf();
        let signers = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let err = AdminMultisig::init(&mut buf, Pubkey::new_unique(), &signers, 3, 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidInstructionData as u32)
        );
    }

    #[test]
    fn init_rejects_threshold_zero() {
        let mut buf = fresh_buf();
        let signers = vec![Pubkey::new_unique()];
        let err = AdminMultisig::init(&mut buf, Pubkey::new_unique(), &signers, 0, 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidInstructionData as u32)
        );
    }

    #[test]
    fn init_rejects_duplicate_signers() {
        let mut buf = fresh_buf();
        let a = Pubkey::new_unique();
        let signers = vec![a, a];
        let err = AdminMultisig::init(&mut buf, Pubkey::new_unique(), &signers, 2, 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidInstructionData as u32)
        );
    }

    #[test]
    fn init_rejects_too_many_signers() {
        let mut buf = fresh_buf();
        let signers: Vec<_> = (0..(MAX_MULTISIG_SIGNERS + 1))
            .map(|_| Pubkey::new_unique())
            .collect();
        let err = AdminMultisig::init(&mut buf, Pubkey::new_unique(), &signers, 2, 0).unwrap_err();
        assert_eq!(
            err,
            ProgramError::Custom(PolyleverageError::InvalidInstructionData as u32)
        );
    }

    #[test]
    fn rotate_bumps_version_and_replaces_set() {
        let mut buf = fresh_buf();
        let initial = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        AdminMultisig::init(&mut buf, Pubkey::new_unique(), &initial, 2, 0).unwrap();
        let new_set = vec![
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        {
            let ms = AdminMultisig::load_mut(&mut buf).unwrap();
            ms.rotate(&new_set, 3).unwrap();
        }
        let ms = AdminMultisig::load(&buf).unwrap();
        assert_eq!(ms.threshold, 3);
        assert_eq!(ms.n_signers, 4);
        assert_eq!(ms.version, 2);
        assert!(!ms.is_member(&initial[0]));
        assert!(ms.is_member(&new_set[2]));
    }
}
