//! Timelock proposal / execute / cancel for attestation-signer rotation.
//! Vendor-neutral: the new signer pubkey can be a Chainlink CRE DON, an
//! EigenCloud TEE app's Ed25519 wallet, a KMS-backed key, etc.
//!
//! Spec §9.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::{CancelTimelockArgs, ExecuteTimelockArgs, ProposeSetAttestationSignerArgs},
    processor::multisig::authorize_admin_or_multisig,
    seeds::SEED_TIMELOCK,
    state::{
        ProgramConfig, TimelockProposal, DISC_TIMELOCK, TIMELOCK_DELAY_SECS, TIMELOCK_PROPOSAL_LEN,
        TL_KIND_SET_ATTESTATION_SIGNER, TL_STATUS_CANCELLED, TL_STATUS_EXECUTED, TL_STATUS_PENDING,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Accounts:
///   0. `[signer, writable]` admin (pays rent)
///   1. `[]` program config
///   2. `[writable]` timelock proposal PDA (new)
///   3. `[]` system program
pub fn process_propose_set_attestation_signer(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: ProposeSetAttestationSignerArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let tl_ai = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(admin)?;
    assert_writable(tl_ai)?;
    if program_config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }

    let bump = assert_pda(
        &[SEED_TIMELOCK, &args.proposal_id.to_le_bytes()],
        program_id,
        tl_ai.key,
    )?;
    if tl_ai.data_len() != 0 {
        return Err(PolyleverageError::AlreadyInitialized.into());
    }

    let clock = Clock::get()?;
    let rent = Rent::get()?;
    invoke_signed(
        &solana_program::system_instruction::create_account(
            admin.key,
            tl_ai.key,
            rent.minimum_balance(TIMELOCK_PROPOSAL_LEN),
            TIMELOCK_PROPOSAL_LEN as u64,
            program_id,
        ),
        &[admin.clone(), tl_ai.clone(), system_program.clone()],
        &[&[SEED_TIMELOCK, &args.proposal_id.to_le_bytes(), &[bump]]],
    )?;

    let mut data = tl_ai.try_borrow_mut_data()?;
    let tl: &mut TimelockProposal = bytemuck::from_bytes_mut(&mut data[..TIMELOCK_PROPOSAL_LEN]);
    *tl = TimelockProposal {
        discriminator: DISC_TIMELOCK,
        kind: TL_KIND_SET_ATTESTATION_SIGNER,
        status: TL_STATUS_PENDING,
        bump,
        _pad0: [0; 4],
        proposal_id: args.proposal_id,
        proposer: *admin.key,
        payload: [0; 64],
        proposed_slot: clock.slot,
        proposed_unix_ts: clock.unix_timestamp,
        executable_after_unix_ts: clock
            .unix_timestamp
            .checked_add(TIMELOCK_DELAY_SECS)
            .ok_or(PolyleverageError::ArithmeticOverflow)?,
        _reserved: [0; 32],
    };
    tl.set_new_signer(&args.new_signer);

    msg!(
        "propose_set_attestation_signer: proposal={} executable_after={}",
        args.proposal_id,
        tl.executable_after_unix_ts,
    );
    Ok(())
}

/// Accounts:
///   0. `[signer]` admin
///   1. `[writable]` program config
///   2. `[writable]` timelock proposal PDA
pub fn process_execute_set_attestation_signer(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: ExecuteTimelockArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let tl_ai = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(program_config_ai)?;
    assert_writable(tl_ai)?;
    if program_config_ai.owner != program_id || tl_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    let _ = assert_pda(
        &[SEED_TIMELOCK, &args.proposal_id.to_le_bytes()],
        program_id,
        tl_ai.key,
    )?;

    let (kind, _proposer, executable_after_unix_ts, status, new_signer) = {
        let data = tl_ai.try_borrow_data()?;
        let tl = TimelockProposal::load(&data)?;
        (
            tl.kind,
            tl.proposer,
            tl.executable_after_unix_ts,
            tl.status,
            tl.new_signer(),
        )
    };
    if status != TL_STATUS_PENDING {
        return Err(PolyleverageError::TimelockNotExecutable.into());
    }
    if kind != TL_KIND_SET_ATTESTATION_SIGNER {
        return Err(PolyleverageError::TimelockNotExecutable.into());
    }

    let clock = Clock::get()?;
    if clock.unix_timestamp < executable_after_unix_ts {
        return Err(PolyleverageError::TimelockNotExecutable.into());
    }

    // Authorize via single-admin OR multisig. Do it under an immutable
    // borrow so `authorize_admin_or_multisig` (which may re-read config) can
    // safely re-borrow the same account info.
    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }
    {
        let mut data = program_config_ai.try_borrow_mut_data()?;
        let cfg = ProgramConfig::load_mut(&mut data)?;
        cfg.attestation_signer = Pubkey::new_from_array(new_signer);
    }
    {
        let mut data = tl_ai.try_borrow_mut_data()?;
        let tl = TimelockProposal::load_mut(&mut data)?;
        tl.status = TL_STATUS_EXECUTED;
    }

    msg!("execute_set_attestation_signer ok");
    Ok(())
}

/// Accounts:
///   0. `[signer]` admin
///   1. `[writable]` timelock proposal PDA
pub fn process_cancel_timelock(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CancelTimelockArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let tl_ai = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(tl_ai)?;
    if tl_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    let _ = assert_pda(
        &[SEED_TIMELOCK, &args.proposal_id.to_le_bytes()],
        program_id,
        tl_ai.key,
    )?;

    {
        let data = tl_ai.try_borrow_data()?;
        let tl = TimelockProposal::load(&data)?;
        if tl.proposer != *admin.key {
            return Err(PolyleverageError::InvalidAdminSigner.into());
        }
        if tl.status != TL_STATUS_PENDING {
            return Err(PolyleverageError::TimelockNotExecutable.into());
        }
    }

    {
        let mut data = tl_ai.try_borrow_mut_data()?;
        let tl = TimelockProposal::load_mut(&mut data)?;
        tl.status = TL_STATUS_CANCELLED;
    }
    msg!("cancel_timelock: {}", args.proposal_id);
    Ok(())
}
