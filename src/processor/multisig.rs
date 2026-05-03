//! AdminMultisig lifecycle + authorization helpers.
//!
//! The multisig is **optional and opt-in**: a fresh program uses a single
//! admin signer. `InitAdminMultisig` flips the program into multisig mode;
//! once in multisig mode, `RotateAdminMultisig` requires a quorum from
//! the old config before accepting new signers / threshold.
//!
//! `authorize_admin_or_multisig` is the unified gate helper — it accepts
//! *either* (a) the legacy single admin as a signer, or (b) a quorum of
//! multisig members signing. Callers pass the (optional) multisig account
//! in addition to the ProgramConfig.

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
    instruction::{InitAdminMultisigArgs, RotateAdminMultisigArgs},
    seeds::SEED_ADMIN_MULTISIG,
    state::{
        AdminMultisig, ProgramConfig, ADMIN_MULTISIG_LEN, DISC_ADMIN_MULTISIG,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Unified admin authorization check.
///
/// Scans `accounts` for an `AdminMultisig` PDA owned by `program_id`. If
/// found, its threshold must be met by signers also present in `accounts`.
/// If no multisig account is provided, the single `config.admin` must be
/// among the signers.
///
/// Callers can pass the entire instruction account list — the helper
/// ignores non-signer, non-multisig accounts.
pub fn authorize_admin_or_multisig(
    program_id: &Pubkey,
    config: &ProgramConfig,
    accounts: &[AccountInfo],
) -> ProgramResult {
    // Try multisig first.
    for ai in accounts {
        if ai.owner != program_id || ai.data_len() < ADMIN_MULTISIG_LEN {
            continue;
        }
        let data = ai.try_borrow_data()?;
        if data.is_empty() || data[0] != DISC_ADMIN_MULTISIG {
            continue;
        }
        // Verify PDA derivation to prevent substitution with a non-canonical
        // account crafted by an attacker.
        let (expected, _bump) = Pubkey::find_program_address(&[SEED_ADMIN_MULTISIG], program_id);
        if *ai.key != expected {
            continue;
        }
        let ms = AdminMultisig::load(&data)?;
        return ms.check_threshold(accounts);
    }
    // Single-admin fallback.
    for ai in accounts {
        if ai.is_signer && ai.key == &config.admin {
            return Ok(());
        }
    }
    Err(PolyleverageError::InvalidAdminSigner.into())
}

/// `InitAdminMultisig` — one-shot bootstrap.
///
/// Accounts:
///   0. `[writable, signer]` current single admin (rent payer)
///   1. `[]` program config
///   2. `[writable]` admin_multisig PDA (uninitialized)
///   3. `[]` system_program
pub fn process_init_admin_multisig(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: InitAdminMultisigArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let config_ai = next_account_info(iter)?;
    let ms_ai = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(admin)?;
    assert_writable(ms_ai)?;
    if config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if system_program.key != &solana_program::system_program::ID {
        return Err(solana_program::program_error::ProgramError::InvalidAccountData);
    }

    // Verify admin matches config.admin.
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.admin != *admin.key {
            return Err(PolyleverageError::InvalidAdminSigner.into());
        }
    }

    let bump = assert_pda(&[SEED_ADMIN_MULTISIG], program_id, ms_ai.key)?;
    if ms_ai.data_len() == 0 {
        let rent = Rent::get()?;
        let lamports = rent.minimum_balance(ADMIN_MULTISIG_LEN);
        let ix = system_instruction::create_account(
            admin.key,
            ms_ai.key,
            lamports,
            ADMIN_MULTISIG_LEN as u64,
            program_id,
        );
        invoke_signed(
            &ix,
            &[admin.clone(), ms_ai.clone(), system_program.clone()],
            &[&[SEED_ADMIN_MULTISIG, &[bump]]],
        )?;
    } else if ms_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    let signers: Vec<Pubkey> = args
        .signers
        .iter()
        .map(|s| Pubkey::new_from_array(*s))
        .collect();
    let mut data = ms_ai.try_borrow_mut_data()?;
    AdminMultisig::init(&mut data, *admin.key, &signers, args.threshold, bump)?;
    Ok(())
}

/// `RotateAdminMultisig` — replace signer set / threshold. Requires
/// quorum on the **existing** multisig.
///
/// Accounts:
///   0. `[]` program config
///   1. `[writable]` admin_multisig PDA (already initialized)
///   2..N `[signer]` approving signers (must include ≥ threshold members)
pub fn process_rotate_admin_multisig(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: RotateAdminMultisigArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let config_ai = next_account_info(iter)?;
    let ms_ai = next_account_info(iter)?;

    assert_writable(ms_ai)?;
    if config_ai.owner != program_id || ms_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Verify quorum on the existing multisig using *all* accounts passed
    // (the header accounts will simply have is_signer==false).
    {
        let data = ms_ai.try_borrow_data()?;
        let ms = AdminMultisig::load(&data)?;
        ms.check_threshold(accounts)?;
    }

    let new_signers: Vec<Pubkey> = args
        .signers
        .iter()
        .map(|s| Pubkey::new_from_array(*s))
        .collect();
    let mut data = ms_ai.try_borrow_mut_data()?;
    let ms = AdminMultisig::load_mut(&mut data)?;
    ms.rotate(&new_signers, args.threshold)?;
    Ok(())
}
