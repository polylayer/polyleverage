//! `MarketNonceAccount` — stores last accepted CRE attestation nonce per market_id.
//! Used to prevent replay of outdated prices/resolutions.
//!
//! Spec §5.2

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_MARKET_NONCE};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use super::DISC_MARKET_NONCE;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct MarketNonceAccount {
    pub discriminator: u8,
    pub is_resolved: u8,
    pub bump: u8,
    pub _pad0: [u8; 5],

    pub market_id: [u8; 32],

    pub last_accepted_cre_nonce: u64,
    pub last_accepted_twap_fp: u64,
    pub last_accepted_ts: u64,

    /// Cached once `is_resolved == 1`. Valid values: 0, 5000, 10000.
    pub final_outcome_bps: u16,
    pub _pad1: [u8; 6],

    pub _reserved: [u8; 32],
}

pub const MARKET_NONCE_LEN: usize = 1 + 1 + 1 + 5 + 32 + 8 + 8 + 8 + 2 + 6 + 32;
const_assert_size!(MarketNonceAccount, MARKET_NONCE_LEN);

impl MarketNonceAccount {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let n: &Self = pod::try_cast_ref(bytes)?;
        if n.discriminator != DISC_MARKET_NONCE {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(n)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let n: &mut Self = pod::try_cast_mut(bytes)?;
        if n.discriminator != DISC_MARKET_NONCE {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(n)
    }

    pub fn init(bytes: &mut [u8], market_id: [u8; 32], bump: u8) -> Result<(), ProgramError> {
        let n: &mut Self = pod::try_cast_mut(bytes)?;
        if n.discriminator != 0 {
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        *n = MarketNonceAccount {
            discriminator: DISC_MARKET_NONCE,
            is_resolved: 0,
            bump,
            _pad0: [0; 5],
            market_id,
            last_accepted_cre_nonce: 0,
            last_accepted_twap_fp: 0,
            last_accepted_ts: 0,
            final_outcome_bps: 0,
            _pad1: [0; 6],
            _reserved: [0; 32],
        };
        Ok(())
    }

    pub fn find_pda(program_id: &Pubkey, market_id: &[u8; 32]) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_MARKET_NONCE, market_id.as_ref()], program_id)
    }
}
