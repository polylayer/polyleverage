//! Instrument handlers: create, pause, resume.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::{CreateInstrumentArgs, ExpandIntentBookArgs},
    seeds::{SEED_BOOK, SEED_BUCKET_REGISTRY, SEED_INSTRUMENT},
    state::{
        init_intent_book, intent_book_byte_size, BookMut, BucketRegistry, FreeNode,
        InstrumentConfig, ProgramConfig, DISC_BUCKET_REGISTRY, INSTRUMENT_CONFIG_LEN,
        NODE_SIZE, NODE_TAG_FREE, STATUS_ACTIVE, STATUS_PAUSED,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Accounts:
///   0. `[writable, signer]` admin (pays rent)
///   1. `[]` program config
///   2. `[writable]` instrument config PDA (seeds [SEED_INSTRUMENT, market_id, leverage, bucket, window])
///   3. `[writable]` intent book PDA (seeds [SEED_BOOK, instrument_config])
///   4. `[]` collateral mint
///   5. `[]` system program
pub fn process_create_instrument(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CreateInstrumentArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let config_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(admin)?;
    assert_writable(instrument_ai)?;
    assert_writable(book_ai)?;
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }
    if mint.owner != &spl_token::ID {
        return Err(PolyleverageError::InvalidMint.into());
    }
    if config_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.admin != *admin.key {
            return Err(PolyleverageError::InvalidAdminSigner.into());
        }
    }

    // Validate input config.
    //
    // Prior to v2.2 this was gated by a hard-coded `ALLOWED_LEVERAGE_BPS`
    // constant. It is now gated by the admin-managed `BucketRegistry` —
    // clients pass the registry PDA as an extra account after the system
    // program. If omitted (legacy callers), we fall back to the
    // per-instrument validity check (nonzero + tick aligned); admins who
    // want the extra safety-net initialize the registry and pass it here.
    if args.initial_book_capacity < 16 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    if args.twap_window_slots == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    if args.collateral_bucket == 0 || args.tick_fp == 0 {
        return Err(PolyleverageError::InvalidCollateralBucket.into());
    }

    if let Some(registry_ai) = iter.next() {
        // Trailing account: optional BucketRegistry. Accept only if it has
        // the right owner + discriminator + canonical PDA.
        let looks_like_registry = registry_ai.owner == program_id
            && registry_ai.data_len() > 0
            && registry_ai
                .try_borrow_data()
                .map(|d| !d.is_empty() && d[0] == DISC_BUCKET_REGISTRY)
                .unwrap_or(false);
        if looks_like_registry {
            let (expected_key, _bump) =
                Pubkey::find_program_address(&[SEED_BUCKET_REGISTRY], program_id);
            if *registry_ai.key != expected_key {
                return Err(PolyleverageError::InvalidPda.into());
            }
            let data = registry_ai.try_borrow_data()?;
            let reg = BucketRegistry::load(&data)?;
            if !reg.allows_leverage(args.leverage_bps) {
                return Err(PolyleverageError::BucketNotInRegistry.into());
            }
            if !reg.allows_bucket(args.collateral_bucket) {
                return Err(PolyleverageError::BucketNotInRegistry.into());
            }
        }
    }

    // Derive and allocate instrument PDA. Seed includes `source` so two
    // platforms can share the same 32-byte market_id without collision.
    let src_byte = [args.source];
    let lev_bytes = args.leverage_bps.to_le_bytes();
    let bucket_bytes = args.collateral_bucket.to_le_bytes();
    let window_bytes = args.twap_window_slots.to_le_bytes();
    let instrument_bump = assert_pda(
        &[
            SEED_INSTRUMENT,
            &src_byte,
            &args.market_id,
            &lev_bytes,
            &bucket_bytes,
            &window_bytes,
        ],
        program_id,
        instrument_ai.key,
    )?;
    if instrument_ai.data_len() != 0 {
        return Err(PolyleverageError::AlreadyInitialized.into());
    }
    let rent = Rent::get()?;
    let instr_lamports = rent.minimum_balance(INSTRUMENT_CONFIG_LEN);
    let create_instr_ix = system_instruction::create_account(
        admin.key,
        instrument_ai.key,
        instr_lamports,
        INSTRUMENT_CONFIG_LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_instr_ix,
        &[admin.clone(), instrument_ai.clone(), system_program.clone()],
        &[&[
            SEED_INSTRUMENT,
            &src_byte,
            &args.market_id,
            &lev_bytes,
            &bucket_bytes,
            &window_bytes,
            &[instrument_bump],
        ]],
    )?;

    // Derive and allocate intent book PDA.
    let book_bump = assert_pda(&[SEED_BOOK, instrument_ai.key.as_ref()], program_id, book_ai.key)?;
    if book_ai.data_len() != 0 {
        return Err(PolyleverageError::AlreadyInitialized.into());
    }
    let book_bytes = intent_book_byte_size(args.initial_book_capacity);
    let book_lamports = rent.minimum_balance(book_bytes);
    let create_book_ix = system_instruction::create_account(
        admin.key,
        book_ai.key,
        book_lamports,
        book_bytes as u64,
        program_id,
    );
    invoke_signed(
        &create_book_ix,
        &[admin.clone(), book_ai.clone(), system_program.clone()],
        &[&[SEED_BOOK, instrument_ai.key.as_ref(), &[book_bump]]],
    )?;

    // Initialize the instrument config.
    let mut instr_data = instrument_ai.try_borrow_mut_data()?;
    InstrumentConfig::init(
        &mut instr_data,
        args.source,
        args.market_id,
        args.source_token_id_a,
        args.source_token_id_b,
        *mint.key,
        *book_ai.key,
        args.leverage_bps,
        args.collateral_bucket,
        args.twap_window_slots,
        args.tick_fp,
        args.liquidation_bps,
        args.liquidation_bounty_bps,
        args.max_staleness_secs,
        instrument_bump,
    )?;
    drop(instr_data);

    // Initialize the intent book.
    let mut book_data = book_ai.try_borrow_mut_data()?;
    init_intent_book(&mut book_data, *instrument_ai.key, args.initial_book_capacity, book_bump, 0)?;

    Ok(())
}

/// Accounts:
///   0. `[signer]` admin
///   1. `[]` program config
///   2. `[writable]` instrument config
pub fn process_set_instrument_status(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    pause: bool,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let config_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;

    assert_signer(admin)?;
    assert_writable(instrument_ai)?;
    if config_ai.owner != program_id || instrument_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    {
        let data = config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.admin != *admin.key {
            return Err(PolyleverageError::InvalidAdminSigner.into());
        }
    }

    let mut data = instrument_ai.try_borrow_mut_data()?;
    let cfg = InstrumentConfig::load_mut(&mut data)?;
    cfg.status = if pause { STATUS_PAUSED } else { STATUS_ACTIVE };
    Ok(())
}

/// Expand an intent book by `additional_nodes` fresh slots (pushed onto the
/// freelist). Caller pays the incremental rent.
///
/// Accounts:
///   0. `[signer, writable]` payer (pays incremental rent)
///   1. `[]` program config
///   2. `[]` instrument config
///   3. `[writable]` intent book
///   4. `[]` system program
pub fn process_expand_intent_book(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: ExpandIntentBookArgs,
) -> ProgramResult {
    if args.additional_nodes == 0 {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(payer)?;
    assert_writable(payer)?;
    assert_writable(book_ai)?;
    if program_config_ai.owner != program_id
        || instrument_ai.owner != program_id
        || book_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    // Validate book ↔ instrument binding.
    {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        if inst.intent_book != *book_ai.key {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    let additional_bytes = (args.additional_nodes as usize) * NODE_SIZE;
    let new_len = book_ai
        .data_len()
        .checked_add(additional_bytes)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;

    // Top up lamports so the account stays rent-exempt at the new size.
    let rent = Rent::get()?;
    let required_lamports = rent.minimum_balance(new_len);
    let current_lamports = book_ai.lamports();
    if required_lamports > current_lamports {
        let diff = required_lamports - current_lamports;
        solana_program::program::invoke(
            &system_instruction::transfer(payer.key, book_ai.key, diff),
            &[payer.clone(), book_ai.clone(), system_program.clone()],
        )?;
    }

    // Grow the account. `realloc` zeros new bytes when the `false` flag is NOT
    // set (we pass `false` meaning "no auto-zeroing" and then zero ourselves).
    book_ai.realloc(new_len, true)?;

    // Extend capacity + push the new nodes onto the freelist.
    {
        let mut data = book_ai.try_borrow_mut_data()?;
        let old_capacity;
        {
            let book = BookMut::load(&mut data)?;
            old_capacity = book.header.capacity;
        }
        // Recompute the book view now that capacity changed (our BookMut pool was
        // sized against the old_capacity). Re-load with the new data length.
        let book = BookMut::load(&mut data)?;
        book.header.capacity = old_capacity
            .checked_add(args.additional_nodes)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;

        // Link every new node into the freelist.
        for idx in old_capacity..book.header.capacity {
            let next = book.header.freelist_head;
            let slot = &mut book.nodes[idx as usize];
            slot.bytes = [0u8; NODE_SIZE];
            let free: &mut FreeNode = bytemuck::from_bytes_mut(&mut slot.bytes);
            free.tag = NODE_TAG_FREE;
            free.next_free = next;
            book.header.freelist_head = idx;
        }
    }

    Ok(())
}
