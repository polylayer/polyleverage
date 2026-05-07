//! Zero-click session handlers: CreateSession, RevokeSession.
//!
//! See `state::session` for the on-chain layout + bounds semantics.
//! Delegate-signed PostIntent / CancelIntent live in `processor::intent`.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::CreateSessionArgs,
    seeds::SEED_SESSION,
    state::{ProgramConfig, Session, MAX_SESSION_INSTRUMENTS, SESSION_LEN},
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Accounts:
///   0. `[signer, writable]` owner — pays rent on first init
///   1. `[]` program config
///   2. `[writable]` session PDA = `[SEED_SESSION, owner]`
///   3. `[]` system program
pub fn process_create_session(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CreateSessionArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let session_ai = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(owner)?;
    assert_writable(session_ai)?;
    if program_config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    // Honour global pause — same gate as PostIntent.
    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.global_pause != 0 {
            return Err(PolyleverageError::GlobalPaused.into());
        }
    }

    let bump = assert_pda(
        &[SEED_SESSION, owner.key.as_ref()],
        program_id,
        session_ai.key,
    )?;

    if args.allowed_instruments.len() > MAX_SESSION_INSTRUMENTS {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }

    let now_slot = Clock::get()?.slot;
    if args.expires_at_slot <= now_slot {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    if args.per_intent_max_collateral_atoms == 0 || args.cumulative_collateral_cap == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    if args.per_intent_max_collateral_atoms > args.cumulative_collateral_cap {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }

    // Allocate or re-init.
    let needs_alloc = session_ai.data_is_empty();
    if needs_alloc {
        let rent = Rent::get()?;
        let lamports = rent.minimum_balance(SESSION_LEN);
        let create_ix = system_instruction::create_account(
            owner.key,
            session_ai.key,
            lamports,
            SESSION_LEN as u64,
            program_id,
        );
        invoke_signed(
            &create_ix,
            &[owner.clone(), session_ai.clone(), system_program.clone()],
            &[&[SEED_SESSION, owner.key.as_ref(), &[bump]]],
        )?;
    } else {
        // Existing session — only allow re-init if it's revoked or expired.
        let data = session_ai.try_borrow_data()?;
        let existing = Session::load(&data)?;
        if existing.owner != *owner.key {
            return Err(PolyleverageError::SessionOwnerMismatch.into());
        }
        if existing.is_active(now_slot) {
            // Caller must revoke first to avoid silently dropping cumulative
            // counters that the off-chain TEE thinks are still in effect.
            return Err(PolyleverageError::AlreadyInitialized.into());
        }
        // Otherwise fall through to re-init in place.
    }

    // Decode instrument allowlist.
    let mut allowed: [Pubkey; MAX_SESSION_INSTRUMENTS] =
        [Pubkey::default(); MAX_SESSION_INSTRUMENTS];
    for (i, raw) in args.allowed_instruments.iter().enumerate() {
        allowed[i] = Pubkey::new_from_array(*raw);
    }

    let delegate = Pubkey::new_from_array(args.delegate);

    let mut data = session_ai.try_borrow_mut_data()?;
    Session::init(
        &mut data,
        *owner.key,
        delegate,
        bump,
        args.expires_at_slot,
        args.per_intent_max_collateral_atoms,
        args.cumulative_collateral_cap,
        &allowed[..args.allowed_instruments.len()],
        now_slot,
    )?;

    msg!(
        "session created delegate={} expires_slot={} per_intent_cap={} cumulative_cap={} n_instruments={}",
        delegate,
        args.expires_at_slot,
        args.per_intent_max_collateral_atoms,
        args.cumulative_collateral_cap,
        args.allowed_instruments.len()
    );

    Ok(())
}

/// Accounts:
///   0. `[signer]` owner
///   1. `[writable]` session PDA
///
/// Sets `revoked = 1`. Idempotent.
pub fn process_revoke_session(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let session_ai = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(session_ai)?;
    if session_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    let _ = assert_pda(
        &[SEED_SESSION, owner.key.as_ref()],
        program_id,
        session_ai.key,
    )?;

    let mut data = session_ai.try_borrow_mut_data()?;
    let s = Session::load_mut(&mut data)?;
    if s.owner != *owner.key {
        return Err(PolyleverageError::SessionOwnerMismatch.into());
    }
    s.revoke();

    msg!("session revoked owner={}", owner.key);
    Ok(())
}
