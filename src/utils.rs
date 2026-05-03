//! Common account-validation helpers.

use crate::error::PolyleverageError;
use solana_program::{
    account_info::AccountInfo, program_error::ProgramError, program_pack::Pack, pubkey::Pubkey,
    sysvar::Sysvar,
};

/// Assert that `account.owner == expected`, else [`PolyleverageError::InvalidAccountOwner`].
pub fn assert_owner(account: &AccountInfo, expected: &Pubkey) -> Result<(), ProgramError> {
    if account.owner != expected {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    Ok(())
}

/// Assert that `account` is a signer.
pub fn assert_signer(account: &AccountInfo) -> Result<(), ProgramError> {
    if !account.is_signer {
        return Err(PolyleverageError::MissingSigner.into());
    }
    Ok(())
}

/// Assert that `account` is writable.
pub fn assert_writable(account: &AccountInfo) -> Result<(), ProgramError> {
    if !account.is_writable {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

/// Assert that `actual == expected`, returning `err` if mismatched.
pub fn assert_key_eq(
    actual: &Pubkey,
    expected: &Pubkey,
    err: PolyleverageError,
) -> Result<(), ProgramError> {
    if actual != expected {
        return Err(err.into());
    }
    Ok(())
}

/// Derive a PDA and assert it matches `expected`. Returns the bump.
pub fn assert_pda(
    seeds: &[&[u8]],
    program_id: &Pubkey,
    expected: &Pubkey,
) -> Result<u8, ProgramError> {
    let (derived, bump) = Pubkey::find_program_address(seeds, program_id);
    if derived != *expected {
        return Err(PolyleverageError::InvalidPda.into());
    }
    Ok(bump)
}

/// Current slot from the clock sysvar.
pub fn current_slot() -> Result<u64, ProgramError> {
    Ok(solana_program::clock::Clock::get()?.slot)
}

/// Read an SPL token account's amount field directly, bypassing full deserialization
/// (cheaper on hot paths).
pub fn read_token_amount(account: &AccountInfo) -> Result<u64, ProgramError> {
    let data = account.try_borrow_data()?;
    if data.len() < spl_token::state::Account::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    // Account layout: mint(32) | owner(32) | amount(8) | ... -- amount is at offset 64.
    let bytes = <[u8; 8]>::try_from(&data[64..72]).map_err(|_| ProgramError::InvalidAccountData)?;
    Ok(u64::from_le_bytes(bytes))
}
