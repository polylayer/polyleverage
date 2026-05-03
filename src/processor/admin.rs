//! Admin handlers: init program config, global pause.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program::invoke_signed,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::InitProgramConfigArgs,
    processor::multisig::authorize_admin_or_multisig,
    seeds::SEED_CONFIG,
    state::{ProgramConfig, PROGRAM_CONFIG_LEN},
    utils::{assert_pda, assert_writable, assert_signer},
};

/// Accounts:
///   0. `[writable, signer]` admin (pays rent, becomes admin authority)
///   1. `[writable]` program config PDA (seeds `[SEED_CONFIG]`)
///   2. `[]` fee treasury authority (will own treasury token accounts; can be any pubkey — admin chooses)
///   3. `[]` system program
pub fn process_init_program_config(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: InitProgramConfigArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let config_ai = next_account_info(iter)?;
    let fee_treasury_authority = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(admin)?;
    assert_writable(config_ai)?;
    if system_program.key != &solana_program::system_program::ID {
        return Err(solana_program::program_error::ProgramError::InvalidAccountData);
    }

    let bump = assert_pda(&[SEED_CONFIG], program_id, config_ai.key)?;

    // Only allow init if not yet allocated.
    if config_ai.data_len() == 0 {
        // Create the account.
        let rent = Rent::get()?;
        let lamports = rent.minimum_balance(PROGRAM_CONFIG_LEN);
        let space = PROGRAM_CONFIG_LEN as u64;
        let ix = system_instruction::create_account(
            admin.key,
            config_ai.key,
            lamports,
            space,
            program_id,
        );
        invoke_signed(
            &ix,
            &[admin.clone(), config_ai.clone(), system_program.clone()],
            &[&[SEED_CONFIG, &[bump]]],
        )?;
    } else if config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    let mut data = config_ai.try_borrow_mut_data()?;
    ProgramConfig::init(
        &mut data,
        *admin.key,
        Pubkey::new_from_array(args.attestation_signer),
        *fee_treasury_authority.key,
        args.default_max_staleness_secs,
        bump,
    )?;

    Ok(())
}

/// Accounts:
///   0. `[signer]` admin
///   1. `[writable]` program config
pub fn process_set_global_pause(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    paused: bool,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let _admin = next_account_info(iter)?;
    let config_ai = next_account_info(iter)?;

    assert_writable(config_ai)?;
    if config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Auth first, under an immutable borrow that drops before we take the
    // mutable borrow below. Holding both at once panics with "RefCell already
    // mutably borrowed" when `authorize_admin_or_multisig` itself reads
    // `config_ai`.
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }
    let mut data = config_ai.try_borrow_mut_data()?;
    let cfg = ProgramConfig::load_mut(&mut data)?;
    cfg.global_pause = if paused { 1 } else { 0 };
    Ok(())
}
