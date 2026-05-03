//! `TimelockProposal` — 24-hour admin-delayed change. Currently used for
//! `SetAttestationSigner`; extensible to other privileged mutations.

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_TIMELOCK};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

pub const DISC_TIMELOCK: u8 = 13;

pub const TL_KIND_SET_ATTESTATION_SIGNER: u8 = 1;

pub const TL_STATUS_PENDING: u8 = 0;
pub const TL_STATUS_EXECUTED: u8 = 1;
pub const TL_STATUS_CANCELLED: u8 = 2;

/// 24 hours in unix seconds.
pub const TIMELOCK_DELAY_SECS: i64 = 24 * 60 * 60;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct TimelockProposal {
    pub discriminator: u8,
    pub kind: u8,
    pub status: u8,
    pub bump: u8,
    pub _pad0: [u8; 4],

    pub proposal_id: u64,
    pub proposer: Pubkey,

    /// Kind-specific payload. For SET_ATTESTATION_SIGNER: first 32 bytes = new pubkey.
    pub payload: [u8; 64],

    pub proposed_slot: u64,
    pub proposed_unix_ts: i64,
    pub executable_after_unix_ts: i64,

    pub _reserved: [u8; 32],
}

pub const TIMELOCK_PROPOSAL_LEN: usize = 1 + 1 + 1 + 1 + 4 + 8 + 32 + 64 + 8 + 8 + 8 + 32;
const_assert_size!(TimelockProposal, TIMELOCK_PROPOSAL_LEN);

impl TimelockProposal {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let p: &Self = pod::try_cast_ref(bytes)?;
        if p.discriminator != DISC_TIMELOCK {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(p)
    }
    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let p: &mut Self = pod::try_cast_mut(bytes)?;
        if p.discriminator != DISC_TIMELOCK {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(p)
    }

    pub fn find_pda(program_id: &Pubkey, proposal_id: u64) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_TIMELOCK, &proposal_id.to_le_bytes()], program_id)
    }

    pub fn new_signer(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.payload[..32]);
        out
    }

    pub fn set_new_signer(&mut self, v: &[u8; 32]) {
        self.payload[..32].copy_from_slice(v);
    }
}
