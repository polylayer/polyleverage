//! Margin-account handlers: create, deposit, withdraw.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::{DepositArgs, WithdrawArgs},
    seeds::{SEED_MARGIN, SEED_VAULT},
    state::{MarginAccount, MARGIN_ACCOUNT_LEN},
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Accounts:
///   0. `[writable, signer]` owner
///   1. `[writable]` margin account PDA (seeds `[SEED_MARGIN, owner, mint]`)
///   2. `[]` collateral mint
///   3. `[]` system program
pub fn process_create_margin_account(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(owner)?;
    assert_writable(margin_ai)?;
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }
    if mint.owner != &spl_token::ID {
        return Err(PolyleverageError::InvalidMint.into());
    }

    let bump = assert_pda(
        &[SEED_MARGIN, owner.key.as_ref(), mint.key.as_ref()],
        program_id,
        margin_ai.key,
    )?;

    if margin_ai.data_len() != 0 {
        return Err(PolyleverageError::AlreadyInitialized.into());
    }

    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(MARGIN_ACCOUNT_LEN);
    let ix = system_instruction::create_account(
        owner.key,
        margin_ai.key,
        lamports,
        MARGIN_ACCOUNT_LEN as u64,
        program_id,
    );
    invoke_signed(
        &ix,
        &[owner.clone(), margin_ai.clone(), system_program.clone()],
        &[&[SEED_MARGIN, owner.key.as_ref(), mint.key.as_ref(), &[bump]]],
    )?;

    let mut data = margin_ai.try_borrow_mut_data()?;
    MarginAccount::init(&mut data, *owner.key, *mint.key, bump)?;
    Ok(())
}

/// Accounts:
///   0. `[signer]` owner
///   1. `[writable]` margin account PDA
///   2. `[]` mint
///   3. `[writable]` user token ATA (source)
///   4. `[writable]` collateral vault token account (destination) — PDA ATA
///   5. `[]` token program
pub fn process_deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: DepositArgs,
) -> ProgramResult {
    if args.amount == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let vault_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(margin_ai)?;
    assert_writable(user_ata)?;
    assert_writable(vault_ata)?;
    if margin_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if token_program.key != &spl_token::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    // Validate margin PDA
    let bump = assert_pda(
        &[SEED_MARGIN, owner.key.as_ref(), mint.key.as_ref()],
        program_id,
        margin_ai.key,
    )?;
    {
        let data = margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != *owner.key || m.collateral_mint != *mint.key || m.bump != bump {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    // CPI: transfer from user ATA to collateral vault.
    let ix = spl_token::instruction::transfer(
        token_program.key,
        user_ata.key,
        vault_ata.key,
        owner.key,
        &[],
        args.amount,
    )?;
    solana_program::program::invoke(
        &ix,
        &[
            user_ata.clone(),
            vault_ata.clone(),
            owner.clone(),
            token_program.clone(),
        ],
    )?;

    // Update ledger.
    let mut data = margin_ai.try_borrow_mut_data()?;
    let m = MarginAccount::load_mut(&mut data)?;
    m.credit_free(args.amount)?;
    msg!("deposit ok: {}", args.amount);
    Ok(())
}

/// Accounts:
///   0. `[signer]` owner
///   1. `[writable]` margin account PDA
///   2. `[]` mint
///   3. `[writable]` user token ATA (destination)
///   4. `[writable]` collateral vault token account (source) — PDA ATA
///   5. `[]` vault authority PDA (seeds `[SEED_VAULT, mint]`)
///   6. `[]` token program
pub fn process_withdraw(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: WithdrawArgs,
) -> ProgramResult {
    if args.amount == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let vault_ata = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(margin_ai)?;
    assert_writable(user_ata)?;
    assert_writable(vault_ata)?;
    if margin_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if token_program.key != &spl_token::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    let margin_bump = assert_pda(
        &[SEED_MARGIN, owner.key.as_ref(), mint.key.as_ref()],
        program_id,
        margin_ai.key,
    )?;
    let vault_auth_bump = assert_pda(
        &[SEED_VAULT, mint.key.as_ref()],
        program_id,
        vault_authority.key,
    )?;

    // Validate vault ATA owner is our vault_authority PDA (prevents passing in a random token account).
    {
        let data = vault_ata.try_borrow_data()?;
        let token = spl_token::state::Account::unpack_from_slice(&data)?;
        if token.owner != *vault_authority.key {
            return Err(PolyleverageError::InvalidTokenAccountOwner.into());
        }
        if token.mint != *mint.key {
            return Err(PolyleverageError::InvalidMint.into());
        }
    }

    // Update ledger first; if CPI fails, runtime reverts anyway.
    {
        let mut data = margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        if m.owner != *owner.key || m.collateral_mint != *mint.key || m.bump != margin_bump {
            return Err(PolyleverageError::InvalidPda.into());
        }
        m.debit_free(args.amount)?;
    }

    // CPI: transfer from vault to user, signed by vault authority PDA.
    let ix = spl_token::instruction::transfer(
        token_program.key,
        vault_ata.key,
        user_ata.key,
        vault_authority.key,
        &[],
        args.amount,
    )?;
    invoke_signed(
        &ix,
        &[
            vault_ata.clone(),
            user_ata.clone(),
            vault_authority.clone(),
            token_program.clone(),
        ],
        &[&[SEED_VAULT, mint.key.as_ref(), &[vault_auth_bump]]],
    )?;

    Ok(())
}
