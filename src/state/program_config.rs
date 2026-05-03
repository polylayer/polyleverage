//! `ProgramConfig` — singleton PDA holding program-global admin + CRE key + pause flag.
//!
//! Spec §1.1

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_CONFIG};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use super::DISC_PROGRAM_CONFIG;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct ProgramConfig {
    /// Account discriminator = [`DISC_PROGRAM_CONFIG`].
    pub discriminator: u8,
    /// `1` if globally paused, `0` otherwise.
    pub global_pause: u8,
    /// PDA bump for this account.
    pub bump: u8,
    /// Padding to align following fields.
    pub _pad0: [u8; 5],

    /// Admin authority pubkey (timelock-gated for sensitive changes).
    pub admin: Pubkey,
    /// Fee treasury PDA — SPL token account address per mint is derived separately.
    pub fee_treasury_authority: Pubkey,
    /// Ed25519 pubkey of the CRE DON (32 bytes). Used to verify attestations.
    pub attestation_signer: Pubkey,

    /// Freshness bound for CRE attestations (seconds of wall-clock, not slots — CRE
    /// signs wall-clock timestamps). Per-instrument overrides live on `InstrumentConfig`.
    pub default_max_staleness_secs: u64,

    /// Reserved for future fields. Must remain zero.
    pub _reserved: [u8; 64],
}

pub const PROGRAM_CONFIG_LEN: usize = 1 + 1 + 1 + 5 + 32 + 32 + 32 + 8 + 64;
const_assert_size!(ProgramConfig, PROGRAM_CONFIG_LEN);

impl ProgramConfig {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let cfg: &ProgramConfig = pod::try_cast_ref(bytes)?;
        if cfg.discriminator != DISC_PROGRAM_CONFIG {
            return Err(PolyleverageError::InvalidProgramConfig.into());
        }
        Ok(cfg)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let cfg: &mut ProgramConfig = pod::try_cast_mut(bytes)?;
        if cfg.discriminator != DISC_PROGRAM_CONFIG {
            return Err(PolyleverageError::InvalidProgramConfig.into());
        }
        Ok(cfg)
    }

    pub fn init(
        bytes: &mut [u8],
        admin: Pubkey,
        attestation_signer: Pubkey,
        fee_treasury_authority: Pubkey,
        default_max_staleness_secs: u64,
        bump: u8,
    ) -> Result<(), ProgramError> {
        let cfg: &mut ProgramConfig = pod::try_cast_mut(bytes)?;
        if cfg.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        *cfg = ProgramConfig {
            discriminator: DISC_PROGRAM_CONFIG,
            global_pause: 0,
            bump,
            _pad0: [0; 5],
            admin,
            fee_treasury_authority,
            attestation_signer,
            default_max_staleness_secs,
            _reserved: [0; 64],
        };
        Ok(())
    }

    pub fn find_pda(program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_CONFIG], program_id)
    }
}
