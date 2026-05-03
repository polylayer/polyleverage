//! Fee schedule admin + volume tracker + treasury sweep.
//!
//! - `InitFeeSchedule` — creates the singleton PDA (one-time).
//! - `SetFeeSchedule` — admin replaces tiers. For v1 simplicity we enforce
//!   `new_max ≤ old_max` so existing intents' pre-reserved fee buffers always
//!   cover the actual match-time fee (cannot over-charge retroactively).
//! - `SweepFees` — admin moves accrued fees out of the CollateralVault to a
//!   designated treasury ATA.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::{FeeTier, InitFeeScheduleArgs, SetFeeScheduleArgs, SweepFeesArgs},
    seeds::{SEED_FEE_SCHEDULE, SEED_TREASURY, SEED_VAULT},
    state::{
        compute_max_fee_bps, validate_tiers, FeeSchedule, FeeTierRaw, FeeTreasury,
        ProgramConfig, FEE_SCHEDULE_LEN, FEE_SCHEDULE_MAX_TIERS, FEE_TREASURY_LEN,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

fn to_raw_tiers(
    tiers: &[FeeTier; FEE_SCHEDULE_MAX_TIERS],
) -> [FeeTierRaw; FEE_SCHEDULE_MAX_TIERS] {
    let mut out = [FeeTierRaw {
        volume_threshold: 0,
        fee_bps: 0,
        _pad: [0; 6],
    }; FEE_SCHEDULE_MAX_TIERS];
    for (i, t) in tiers.iter().enumerate() {
        out[i] = FeeTierRaw {
            volume_threshold: t.volume_threshold,
            fee_bps: t.fee_bps,
            _pad: [0; 6],
        };
    }
    out
}

/// Accounts:
///   0. `[signer, writable]` admin (pays rent)
///   1. `[]` program config
///   2. `[writable]` fee schedule PDA (new)
///   3. `[]` system program
pub fn process_init_fee_schedule(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: InitFeeScheduleArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let fee_schedule_ai = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(admin)?;
    assert_writable(fee_schedule_ai)?;
    if program_config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.admin != *admin.key {
            return Err(PolyleverageError::InvalidAdminSigner.into());
        }
    }

    let tiers = to_raw_tiers(&args.tiers);
    validate_tiers(&tiers)?;

    let bump = assert_pda(&[SEED_FEE_SCHEDULE], program_id, fee_schedule_ai.key)?;
    if fee_schedule_ai.data_len() != 0 {
        return Err(PolyleverageError::AlreadyInitialized.into());
    }

    let rent = Rent::get()?;
    invoke_signed(
        &solana_program::system_instruction::create_account(
            admin.key,
            fee_schedule_ai.key,
            rent.minimum_balance(FEE_SCHEDULE_LEN),
            FEE_SCHEDULE_LEN as u64,
            program_id,
        ),
        &[admin.clone(), fee_schedule_ai.clone(), system_program.clone()],
        &[&[SEED_FEE_SCHEDULE, &[bump]]],
    )?;

    let mut data = fee_schedule_ai.try_borrow_mut_data()?;
    FeeSchedule::init(&mut data, &tiers, Clock::get()?.slot, bump)?;
    msg!("fee schedule initialized");
    Ok(())
}

/// Accounts:
///   0. `[signer]` admin
///   1. `[]` program config
///   2. `[writable]` fee schedule
pub fn process_set_fee_schedule(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SetFeeScheduleArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let fee_schedule_ai = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(fee_schedule_ai)?;
    if program_config_ai.owner != program_id || fee_schedule_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.admin != *admin.key {
            return Err(PolyleverageError::InvalidAdminSigner.into());
        }
    }

    let new_tiers = to_raw_tiers(&args.tiers);
    validate_tiers(&new_tiers)?;
    let new_max = compute_max_fee_bps(&new_tiers);

    let mut data = fee_schedule_ai.try_borrow_mut_data()?;
    let sched = FeeSchedule::load_mut(&mut data)?;
    if new_max > sched.max_tier_fee_bps {
        // Cannot increase max without first draining existing intents —
        // their fee_buffer reservations were computed at the old max.
        return Err(PolyleverageError::FeeIncreaseBlocked.into());
    }
    sched.tiers = new_tiers;
    sched.max_tier_fee_bps = new_max;
    sched.last_updated_slot = Clock::get()?.slot;
    msg!("fee schedule updated (max_bps={})", new_max);
    Ok(())
}

/// Accounts:
///   0. `[signer]` admin
///   1. `[]` program config
///   2. `[writable]` fee treasury PDA
///   3. `[writable]` collateral vault (source)
///   4. `[writable]` destination ATA
///   5. `[]` collateral_mint
///   6. `[]` vault authority PDA (seeds [SEED_VAULT, mint])
///   7. `[]` token program
pub fn process_sweep_fees(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SweepFeesArgs,
) -> ProgramResult {
    if args.amount == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let treasury_ai = next_account_info(iter)?;
    let vault_ata = next_account_info(iter)?;
    let dest_ata = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let vault_authority_ai = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(treasury_ai)?;
    assert_writable(vault_ata)?;
    assert_writable(dest_ata)?;
    if program_config_ai.owner != program_id || treasury_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if token_program.key != &spl_token::ID {
        return Err(ProgramError::InvalidAccountData);
    }
    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.admin != *admin.key {
            return Err(PolyleverageError::InvalidAdminSigner.into());
        }
    }

    let vault_auth_bump = assert_pda(
        &[SEED_VAULT, mint.key.as_ref()],
        program_id,
        vault_authority_ai.key,
    )?;
    let treasury_bump = assert_pda(
        &[SEED_TREASURY, mint.key.as_ref()],
        program_id,
        treasury_ai.key,
    )?;
    let _ = treasury_bump;

    // Validate vault_ata owner + mint.
    {
        let data = vault_ata.try_borrow_data()?;
        let acc = spl_token::state::Account::unpack_from_slice(&data)?;
        if acc.owner != *vault_authority_ai.key || acc.mint != *mint.key {
            return Err(PolyleverageError::InvalidTokenAccountOwner.into());
        }
    }

    // Debit treasury ledger first.
    {
        let mut data = treasury_ai.try_borrow_mut_data()?;
        let t = FeeTreasury::load_mut(&mut data)?;
        if t.collateral_mint != *mint.key {
            return Err(PolyleverageError::InvalidPda.into());
        }
        t.sweep(args.amount)?;
    }

    // CPI transfer with vault authority PDA sign.
    let ix = spl_token::instruction::transfer(
        &spl_token::ID,
        vault_ata.key,
        dest_ata.key,
        vault_authority_ai.key,
        &[],
        args.amount,
    )?;
    invoke_signed(
        &ix,
        &[
            vault_ata.clone(),
            dest_ata.clone(),
            vault_authority_ai.clone(),
            token_program.clone(),
        ],
        &[&[SEED_VAULT, mint.key.as_ref(), &[vault_auth_bump]]],
    )?;

    msg!("swept {} fee atoms", args.amount);
    Ok(())
}

/// Lazily create a FeeTreasury PDA if absent. Used by the match path.
///
/// Accounts:
///   0. `[signer, writable]` payer
///   1. `[writable]` treasury PDA
///   2. `[]` mint
///   3. `[]` system program
pub fn ensure_fee_treasury<'info>(
    program_id: &Pubkey,
    payer: &AccountInfo<'info>,
    treasury_ai: &AccountInfo<'info>,
    mint: &Pubkey,
    system_program: &AccountInfo<'info>,
) -> ProgramResult {
    if treasury_ai.data_len() != 0 {
        return Ok(());
    }
    let bump = assert_pda(
        &[SEED_TREASURY, mint.as_ref()],
        program_id,
        treasury_ai.key,
    )?;
    let rent = Rent::get()?;
    invoke_signed(
        &solana_program::system_instruction::create_account(
            payer.key,
            treasury_ai.key,
            rent.minimum_balance(FEE_TREASURY_LEN),
            FEE_TREASURY_LEN as u64,
            program_id,
        ),
        &[payer.clone(), treasury_ai.clone(), system_program.clone()],
        &[&[SEED_TREASURY, mint.as_ref(), &[bump]]],
    )?;
    let mut data = treasury_ai.try_borrow_mut_data()?;
    FeeTreasury::init(&mut data, *mint, bump)?;
    Ok(())
}
