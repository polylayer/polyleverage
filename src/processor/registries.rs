//! Registry lifecycle handlers: `BucketRegistry` + `StablecoinRegistry`.
//!
//! See `state::bucket_registry` and `state::stablecoin_registry` for the
//! data-layer contracts. Every admin operation here is routed through the
//! unified `authorize_admin_or_multisig` helper.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
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
    instruction::{
        InitBucketRegistryArgs, InitStablecoinRegistryArgs, SetBucketRegistryArgs,
    },
    processor::multisig::authorize_admin_or_multisig,
    seeds::{
        SEED_BUCKET_REGISTRY, SEED_STABLECOIN_REGISTRY, SEED_WUSD_AUTHORITY, SEED_WUSD_MINT,
    },
    state::{
        BucketRegistry, ProgramConfig, StablecoinRegistry, BUCKET_REGISTRY_LEN,
        DEFAULT_COLLATERAL_BUCKETS, DEFAULT_LEVERAGE_BPS, STABLECOIN_REGISTRY_LEN,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

// ---------------------------------------------------------------------------
// BucketRegistry
// ---------------------------------------------------------------------------

/// Accounts:
///   0. `[writable, signer]` admin (rent payer)
///   1. `[]` program config
///   2. `[writable]` bucket_registry PDA (uninitialized)
///   3. `[]` system program
///   (additional signer accounts / multisig PDA accepted for auth)
pub fn process_init_bucket_registry(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: InitBucketRegistryArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let config_ai = next_account_info(iter)?;
    let registry_ai = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(admin)?;
    assert_writable(registry_ai)?;
    if config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }

    let bump = assert_pda(&[SEED_BUCKET_REGISTRY], program_id, registry_ai.key)?;
    if registry_ai.data_len() == 0 {
        let rent = Rent::get()?;
        let lamports = rent.minimum_balance(BUCKET_REGISTRY_LEN);
        invoke_signed(
            &system_instruction::create_account(
                admin.key,
                registry_ai.key,
                lamports,
                BUCKET_REGISTRY_LEN as u64,
                program_id,
            ),
            &[admin.clone(), registry_ai.clone(), system_program.clone()],
            &[&[SEED_BUCKET_REGISTRY, &[bump]]],
        )?;
    } else if registry_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Empty payload fields → apply defaults.
    let leverages: Vec<u32> = if args.leverage_bps.is_empty() {
        DEFAULT_LEVERAGE_BPS.to_vec()
    } else {
        args.leverage_bps
    };
    let buckets: Vec<u64> = if args.collateral_buckets.is_empty() {
        DEFAULT_COLLATERAL_BUCKETS.to_vec()
    } else {
        args.collateral_buckets
    };

    let mut data = registry_ai.try_borrow_mut_data()?;
    BucketRegistry::init(&mut data, &leverages, &buckets, bump)?;
    Ok(())
}

/// Accounts:
///   0. `[]` program config
///   1. `[writable]` bucket_registry PDA
///   (followed by admin signer / multisig PDA + member signers)
pub fn process_set_bucket_registry(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SetBucketRegistryArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let config_ai = next_account_info(iter)?;
    let registry_ai = next_account_info(iter)?;
    assert_writable(registry_ai)?;
    if config_ai.owner != program_id || registry_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }
    let mut data = registry_ai.try_borrow_mut_data()?;
    let r = BucketRegistry::load_mut(&mut data)?;
    r.set(&args.leverage_bps, &args.collateral_buckets)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// StablecoinRegistry — init only. Per-mint Add/Remove handlers live in
// `processor::wusd` alongside the wrap/unwrap logic since they touch the
// same reserve machinery.
// ---------------------------------------------------------------------------

/// `InitStablecoinRegistry` — one-shot bootstrap.
///
/// Accounts:
///   0. `[writable, signer]` admin (rent payer for registry + wusd_mint)
///   1. `[]` program config
///   2. `[writable]` stablecoin_registry PDA (uninitialized)
///   3. `[writable]` wusd_mint PDA (uninitialized; becomes a fresh SPL mint)
///   4. `[]` wusd_authority PDA (the mint authority; signer is derived by this program)
///   5. `[]` token program
///   6. `[]` system program
///   7. `[]` rent sysvar
///   (plus any multisig accounts)
pub fn process_init_stablecoin_registry(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: InitStablecoinRegistryArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let config_ai = next_account_info(iter)?;
    let registry_ai = next_account_info(iter)?;
    let wusd_mint_ai = next_account_info(iter)?;
    let wusd_authority_ai = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(admin)?;
    assert_writable(registry_ai)?;
    assert_writable(wusd_mint_ai)?;
    if config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if token_program.key != &spl_token::ID
        || system_program.key != &solana_program::system_program::ID
    {
        return Err(ProgramError::InvalidAccountData);
    }
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }

    let reg_bump = assert_pda(&[SEED_STABLECOIN_REGISTRY], program_id, registry_ai.key)?;
    let mint_bump = assert_pda(&[SEED_WUSD_MINT], program_id, wusd_mint_ai.key)?;
    let auth_bump = assert_pda(&[SEED_WUSD_AUTHORITY], program_id, wusd_authority_ai.key)?;

    // --- Create registry PDA ---
    if registry_ai.data_len() == 0 {
        let rent = Rent::get()?;
        let lamports = rent.minimum_balance(STABLECOIN_REGISTRY_LEN);
        invoke_signed(
            &system_instruction::create_account(
                admin.key,
                registry_ai.key,
                lamports,
                STABLECOIN_REGISTRY_LEN as u64,
                program_id,
            ),
            &[admin.clone(), registry_ai.clone(), system_program.clone()],
            &[&[SEED_STABLECOIN_REGISTRY, &[reg_bump]]],
        )?;
    } else if registry_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // --- Create wUSD mint PDA (owned by SPL token program, authority = wusd_authority PDA) ---
    if wusd_mint_ai.data_len() == 0 {
        let rent = Rent::get()?;
        let lamports = rent.minimum_balance(spl_token::state::Mint::LEN);
        invoke_signed(
            &system_instruction::create_account(
                admin.key,
                wusd_mint_ai.key,
                lamports,
                spl_token::state::Mint::LEN as u64,
                &spl_token::ID,
            ),
            &[admin.clone(), wusd_mint_ai.clone(), system_program.clone()],
            &[&[SEED_WUSD_MINT, &[mint_bump]]],
        )?;
        let init_mint_ix = spl_token::instruction::initialize_mint2(
            token_program.key,
            wusd_mint_ai.key,
            wusd_authority_ai.key,
            Some(wusd_authority_ai.key),
            args.wusd_decimals,
        )?;
        solana_program::program::invoke(
            &init_mint_ix,
            &[wusd_mint_ai.clone(), token_program.clone(), rent_sysvar.clone()],
        )?;
    }

    // --- Init registry data ---
    let initial_mints: Vec<Pubkey> = args
        .initial_mints
        .iter()
        .map(|b| Pubkey::new_from_array(*b))
        .collect();
    let mut data = registry_ai.try_borrow_mut_data()?;
    StablecoinRegistry::init(
        &mut data,
        *wusd_mint_ai.key,
        args.wusd_decimals,
        &initial_mints,
        reg_bump,
    )?;
    // Defense: auth bump kept out of state (derived at each call), just
    // validated here to prevent silent mismatch.
    let _ = auth_bump;
    Ok(())
}

/// Accounts:
///   0. `[]` program config
///   1. `[writable]` stablecoin_registry
///   2. `[]` new stablecoin mint (SPL mint)
///   (plus admin signer / multisig)
pub fn process_add_stablecoin_mint(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let config_ai = next_account_info(iter)?;
    let registry_ai = next_account_info(iter)?;
    let new_mint_ai = next_account_info(iter)?;
    assert_writable(registry_ai)?;
    if config_ai.owner != program_id || registry_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if new_mint_ai.owner != &spl_token::ID {
        return Err(PolyleverageError::InvalidMint.into());
    }
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }

    // Decimals must match wUSD so wrap/unwrap is atom-for-atom 1:1.
    {
        use solana_program::program_pack::Pack;
        let data = new_mint_ai.try_borrow_data()?;
        let mint = spl_token::state::Mint::unpack_from_slice(&data)?;
        let reg_data = registry_ai.try_borrow_data()?;
        let reg = StablecoinRegistry::load(&reg_data)?;
        if mint.decimals != reg.wusd_decimals {
            return Err(PolyleverageError::StablecoinDecimalsMismatch.into());
        }
    }

    let mut data = registry_ai.try_borrow_mut_data()?;
    let reg = StablecoinRegistry::load_mut(&mut data)?;
    reg.add_mint(*new_mint_ai.key)?;
    Ok(())
}

/// Accounts:
///   0. `[]` program config
///   1. `[writable]` stablecoin_registry
///   2. `[]` stablecoin mint to remove
///   (plus admin signer / multisig)
///
/// Note: removal only stops new `WrapStablecoin` calls. Existing wUSD
/// holders can still `UnwrapStablecoin` against whatever reserves remain
/// for any mint still whitelisted.
pub fn process_remove_stablecoin_mint(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let config_ai = next_account_info(iter)?;
    let registry_ai = next_account_info(iter)?;
    let mint_ai = next_account_info(iter)?;
    assert_writable(registry_ai)?;
    if config_ai.owner != program_id || registry_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        authorize_admin_or_multisig(program_id, cfg, accounts)?;
    }
    let mut data = registry_ai.try_borrow_mut_data()?;
    let reg = StablecoinRegistry::load_mut(&mut data)?;
    reg.remove_mint(mint_ai.key)?;
    Ok(())
}
