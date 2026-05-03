//! `WrapStablecoin` + `UnwrapStablecoin` — 1:1 atom-for-atom conversion
//! between whitelisted USD stablecoins (USDC, USDT, …) and the
//! program-issued canonical `wUSD` SPL mint.
//!
//! Reserve model: each accepted stablecoin has a per-mint "reserve" ATA
//! at the canonical address owned by the `wUSD_authority` PDA (seeded
//! `["wusd_authority"]`). Wrap moves the user's stablecoin into that
//! reserve and mints the matching amount of `wUSD` to the user. Unwrap
//! burns `wUSD` from the user and transfers stablecoin back from the
//! same reserve.
//!
//! Peg assumption: 1 atom of any accepted stablecoin = 1 atom of `wUSD`.
//! Decimals must match (enforced at `AddStablecoinMint` time). The
//! wrapper keeps NO oracle price — if a whitelisted stablecoin depegs,
//! admins must `RemoveStablecoinMint` and accept arbitrage cost during
//! the removal window.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
};

use crate::{
    error::PolyleverageError,
    instruction::{UnwrapStablecoinArgs, WrapStablecoinArgs},
    seeds::{SEED_WUSD_AUTHORITY, SEED_WUSD_MINT},
    state::StablecoinRegistry,
    utils::{assert_pda, assert_signer, assert_writable},
};

/// `WrapStablecoin`: user deposits N atoms of whitelisted stablecoin,
/// receives N atoms of wUSD.
///
/// Accounts:
///   0. `[signer]` user
///   1. `[]` stablecoin_registry
///   2. `[]` source stablecoin mint (must be whitelisted)
///   3. `[writable]` wusd_mint PDA
///   4. `[]` wusd_authority PDA
///   5. `[writable]` user's source-stablecoin ATA
///   6. `[writable]` reserve ATA for that stablecoin (owned by wusd_authority)
///   7. `[writable]` user's wUSD ATA (destination)
///   8. `[]` token program
pub fn process_wrap_stablecoin(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: WrapStablecoinArgs,
) -> ProgramResult {
    if args.amount == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let registry_ai = next_account_info(iter)?;
    let source_mint = next_account_info(iter)?;
    let wusd_mint_ai = next_account_info(iter)?;
    let wusd_authority_ai = next_account_info(iter)?;
    let user_source_ata = next_account_info(iter)?;
    let reserve_ata = next_account_info(iter)?;
    let user_wusd_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    assert_signer(user)?;
    assert_writable(wusd_mint_ai)?;
    assert_writable(user_source_ata)?;
    assert_writable(reserve_ata)?;
    assert_writable(user_wusd_ata)?;
    if registry_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if token_program.key != &spl_token::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    // Validate PDAs + registry state.
    let _ = assert_pda(&[SEED_WUSD_MINT], program_id, wusd_mint_ai.key)?;
    let auth_bump =
        assert_pda(&[SEED_WUSD_AUTHORITY], program_id, wusd_authority_ai.key)?;
    {
        let data = registry_ai.try_borrow_data()?;
        let reg = StablecoinRegistry::load(&data)?;
        if reg.wusd_mint != *wusd_mint_ai.key {
            return Err(PolyleverageError::InvalidMint.into());
        }
        if !reg.is_accepted(source_mint.key) {
            return Err(PolyleverageError::StablecoinNotAccepted.into());
        }
    }

    // Validate reserve ATA belongs to wUSD authority + holds the right mint.
    {
        let data = reserve_ata.try_borrow_data()?;
        let token = spl_token::state::Account::unpack_from_slice(&data)?;
        if token.owner != *wusd_authority_ai.key {
            return Err(PolyleverageError::InvalidTokenAccountOwner.into());
        }
        if token.mint != *source_mint.key {
            return Err(PolyleverageError::InvalidMint.into());
        }
    }

    // 1. User → reserve (stablecoin transfer).
    invoke(
        &spl_token::instruction::transfer(
            token_program.key,
            user_source_ata.key,
            reserve_ata.key,
            user.key,
            &[],
            args.amount,
        )?,
        &[
            user_source_ata.clone(),
            reserve_ata.clone(),
            user.clone(),
            token_program.clone(),
        ],
    )?;

    // 2. Mint wUSD to user (1:1) — signed by wUSD authority PDA.
    invoke_signed(
        &spl_token::instruction::mint_to(
            token_program.key,
            wusd_mint_ai.key,
            user_wusd_ata.key,
            wusd_authority_ai.key,
            &[],
            args.amount,
        )?,
        &[
            wusd_mint_ai.clone(),
            user_wusd_ata.clone(),
            wusd_authority_ai.clone(),
            token_program.clone(),
        ],
        &[&[SEED_WUSD_AUTHORITY, &[auth_bump]]],
    )?;

    Ok(())
}

/// `UnwrapStablecoin`: user burns N wUSD, receives N of target stablecoin.
///
/// Accounts:
///   0. `[signer]` user
///   1. `[]` stablecoin_registry
///   2. `[]` target stablecoin mint (must be whitelisted AND must have ≥ amount
///      in reserve)
///   3. `[writable]` wusd_mint PDA
///   4. `[]` wusd_authority PDA
///   5. `[writable]` user's wUSD ATA (source)
///   6. `[writable]` reserve ATA for target mint (source)
///   7. `[writable]` user's target-stablecoin ATA (destination)
///   8. `[]` token program
pub fn process_unwrap_stablecoin(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: UnwrapStablecoinArgs,
) -> ProgramResult {
    if args.amount == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let registry_ai = next_account_info(iter)?;
    let target_mint = next_account_info(iter)?;
    let wusd_mint_ai = next_account_info(iter)?;
    let wusd_authority_ai = next_account_info(iter)?;
    let user_wusd_ata = next_account_info(iter)?;
    let reserve_ata = next_account_info(iter)?;
    let user_target_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    assert_signer(user)?;
    assert_writable(wusd_mint_ai)?;
    assert_writable(user_wusd_ata)?;
    assert_writable(reserve_ata)?;
    assert_writable(user_target_ata)?;
    if registry_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if token_program.key != &spl_token::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    let _ = assert_pda(&[SEED_WUSD_MINT], program_id, wusd_mint_ai.key)?;
    let auth_bump =
        assert_pda(&[SEED_WUSD_AUTHORITY], program_id, wusd_authority_ai.key)?;

    // Registry lookup — mint must currently be accepted OR at minimum have been
    // accepted and still hold reserves. We keep the "accepted" requirement for
    // safety: admin can drain via a special path if needed.
    {
        let data = registry_ai.try_borrow_data()?;
        let reg = StablecoinRegistry::load(&data)?;
        if reg.wusd_mint != *wusd_mint_ai.key {
            return Err(PolyleverageError::InvalidMint.into());
        }
        if !reg.is_accepted(target_mint.key) {
            return Err(PolyleverageError::StablecoinNotAccepted.into());
        }
    }

    // Validate reserve ATA owner + mint + sufficiency.
    {
        let data = reserve_ata.try_borrow_data()?;
        let token = spl_token::state::Account::unpack_from_slice(&data)?;
        if token.owner != *wusd_authority_ai.key {
            return Err(PolyleverageError::InvalidTokenAccountOwner.into());
        }
        if token.mint != *target_mint.key {
            return Err(PolyleverageError::InvalidMint.into());
        }
        if token.amount < args.amount {
            return Err(PolyleverageError::InsufficientStablecoinReserve.into());
        }
    }

    // 1. Burn wUSD from user (user is already a signer).
    invoke(
        &spl_token::instruction::burn(
            token_program.key,
            user_wusd_ata.key,
            wusd_mint_ai.key,
            user.key,
            &[],
            args.amount,
        )?,
        &[
            user_wusd_ata.clone(),
            wusd_mint_ai.clone(),
            user.clone(),
            token_program.clone(),
        ],
    )?;

    // 2. Reserve → user (stablecoin transfer, signed by wUSD authority PDA).
    invoke_signed(
        &spl_token::instruction::transfer(
            token_program.key,
            reserve_ata.key,
            user_target_ata.key,
            wusd_authority_ai.key,
            &[],
            args.amount,
        )?,
        &[
            reserve_ata.clone(),
            user_target_ata.clone(),
            wusd_authority_ai.clone(),
            token_program.clone(),
        ],
        &[&[SEED_WUSD_AUTHORITY, &[auth_bump]]],
    )?;

    Ok(())
}
