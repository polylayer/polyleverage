//! Close a fully-settled PMLC and refund its rent lamports to the recipient.
//!
//! Solana rent model recap: every account pays a one-time rent-exempt minimum
//! (~0.003 SOL for a 280-byte PMLC). Those lamports sit idle until the account
//! is closed. On close we:
//!
//!   1. Move all lamports out of the PMLC to the caller (or a specified recipient).
//!   2. Zero the data region.
//!   3. Reassign ownership to the system program.
//!
//! After close, the PDA address is permanently vacant (no one can re-init it
//! without seeding — and our PMLC PDA seeds include a unique `pmlc_id`, so
//! that's impossible by design).
//!
//! The caller is usually the keeper (same wallet that triggered Liquidate /
//! Resolve / CloseMutual). They get the rent refund as a small bounty.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    pubkey::Pubkey,
};

use crate::{
    error::PolyleverageError,
    state::{
        Pmlc, PMLC_STATUS_CLOSED, PMLC_STATUS_LIQUIDATED, PMLC_STATUS_RESOLVED,
    },
    utils::{assert_signer, assert_writable},
};

/// Accounts:
///   0. `[signer, writable]` recipient (receives rent refund)
///   1. `[writable]` pmlc (to be closed)
pub fn process_close_pmlc(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let recipient = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;

    assert_signer(recipient)?;
    assert_writable(recipient)?;
    assert_writable(pmlc_ai)?;

    if pmlc_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    let pmlc_id = {
        let data = pmlc_ai.try_borrow_data()?;
        let p = Pmlc::load(&data)?;
        let is_closed = matches!(
            p.status,
            PMLC_STATUS_CLOSED | PMLC_STATUS_LIQUIDATED | PMLC_STATUS_RESOLVED
        );
        if !is_closed {
            return Err(PolyleverageError::PmlcNotLive.into()); // Reused error: "wrong status"
        }
        p.id
    };

    close_pmlc_account(recipient, pmlc_ai)?;
    msg!("pmlc {} closed, rent refunded", pmlc_id);
    Ok(())
}

/// Lower-level helper — zeroes data, transfers lamports out, reassigns owner.
/// Exposed so settlement ixs can perform an atomic "settle + close" when the
/// caller opts in by passing the PMLC account as writable.
pub fn close_pmlc_account<'info>(
    recipient: &AccountInfo<'info>,
    pmlc_ai: &AccountInfo<'info>,
) -> ProgramResult {
    let pmlc_lamports = pmlc_ai.lamports();
    **pmlc_ai.try_borrow_mut_lamports()? = 0;
    **recipient.try_borrow_mut_lamports()? = recipient
        .lamports()
        .checked_add(pmlc_lamports)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;

    // Zero the data region so future reads fail discriminator checks.
    {
        let mut data = pmlc_ai.try_borrow_mut_data()?;
        for b in data.iter_mut() {
            *b = 0;
        }
    }
    pmlc_ai.assign(&solana_program::system_program::ID);
    Ok(())
}
