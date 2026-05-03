//! Matching handlers. All matching is on-chain:
//!
//! - `MatchPair` — explicit `(long_id, short_id)`. Keeper-driven when the caller
//!   has already discovered the pair off-chain.
//! - `MatchBestAvailable` — on-chain scan of the intent book; finds the oldest
//!   long with an overlapping short and matches them. Permissionless; anyone can
//!   run it.
//!
//! Both call into the same `match_pair_core` helper so the economic logic is
//! identical either way. V1 matches **one contract per ix** — callers loop for
//! batch matching.
//!
//! At match time, both sides lock `n × collateral_bucket` (bilateral symmetry
//! preserved). Taker (intent with higher `post_seq`) pays fee from their
//! `fee_buffer`; maker's fee_buffer releases to free. See spec §4.6 and §8.

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
    instruction::{MatchBestArgs, MatchPairArgs},
    math,
    seeds::{SEED_PMLC, SEED_USER_VOLUME},
    state::{
        intent_tree, BookMut, InstrumentConfig, IntentNode, MarginAccount, Pmlc, UserVolume,
        DISC_PMLC, INTENT_FLAG_REENTRY, NODE_TAG_INTENT, PMLC_FLAG_LONG_REENTRY,
        PMLC_FLAG_SHORT_REENTRY, PMLC_LEN, PMLC_STATUS_LIVE, SIDE_LONG, SIDE_SHORT,
        USER_VOLUME_LEN,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Lazy-init a UserVolume PDA if absent. Matches the pattern used by
/// `ensure_fee_treasury`.
fn ensure_user_volume<'info>(
    program_id: &Pubkey,
    payer: &AccountInfo<'info>,
    volume_ai: &AccountInfo<'info>,
    owner: &Pubkey,
    mint: &Pubkey,
    system_program: &AccountInfo<'info>,
    now_slot: u64,
) -> ProgramResult {
    if volume_ai.data_len() != 0 {
        return Ok(());
    }
    let bump = assert_pda(
        &[SEED_USER_VOLUME, owner.as_ref(), mint.as_ref()],
        program_id,
        volume_ai.key,
    )?;
    let rent = Rent::get()?;
    invoke_signed(
        &solana_program::system_instruction::create_account(
            payer.key,
            volume_ai.key,
            rent.minimum_balance(USER_VOLUME_LEN),
            USER_VOLUME_LEN as u64,
            program_id,
        ),
        &[payer.clone(), volume_ai.clone(), system_program.clone()],
        &[&[SEED_USER_VOLUME, owner.as_ref(), mint.as_ref(), &[bump]]],
    )?;
    let mut data = volume_ai.try_borrow_mut_data()?;
    UserVolume::init(&mut data, *owner, *mint, now_slot, bump)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public handlers
// ---------------------------------------------------------------------------

/// Accounts:
///   0. `[signer, writable]` rent payer
///   1. `[]` instrument config
///   2. `[writable]` intent book
///   3. `[writable]` long margin account
///   4. `[writable]` short margin account
///   5. `[writable]` PMLC PDA (new)
///   6. `[]` system program
pub fn process_match_pair(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: MatchPairArgs,
) -> ProgramResult {
    let ctx = unpack_accounts(program_id, accounts)?;
    match_pair_core(
        program_id,
        &ctx,
        args.long_intent_id,
        args.short_intent_id,
        args.max_contracts,
    )
}

/// Same accounts as `MatchPair`; finds the first valid pair on-chain.
///
/// Scan order: iterate node slots linearly (post_seq order approximates), pick
/// the earliest long with `contracts_remaining > 0 && !expired`, then look up
/// a short whose range contains `long.min_price_fp` via the augmented interval
/// tree (`O(log n + k)`). If none, fall back to shorts containing `long.max_price_fp`.
///
/// Returns `MatchNotFound` if no pair exists.
pub fn process_match_best_available(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    _args: MatchBestArgs,
) -> ProgramResult {
    let ctx = unpack_accounts(program_id, accounts)?;

    // Scan pass. BookMut required only because the interval-tree traversal API
    // takes it; we don't mutate anything in the scan itself.
    let (long_id, short_id) = {
        let mut data = ctx.book_ai.try_borrow_mut_data()?;
        let book = BookMut::load(&mut data)?;
        scan_for_first_valid_pair(&book, Clock::get()?.slot)?
            .ok_or(PolyleverageError::IntentNotFound)?
    };
    msg!("match_best picked long={} short={}", long_id, short_id);

    // V1: match one contract per ix. Caller loops.
    match_pair_core(program_id, &ctx, long_id, short_id, 1)
}

// ---------------------------------------------------------------------------
// Account unpacking — shared by both entry points
// ---------------------------------------------------------------------------

pub struct MatchCtx<'a, 'info> {
    pub payer: &'a AccountInfo<'info>,
    pub instrument_ai: &'a AccountInfo<'info>,
    pub book_ai: &'a AccountInfo<'info>,
    pub long_margin_ai: &'a AccountInfo<'info>,
    pub short_margin_ai: &'a AccountInfo<'info>,
    pub pmlc_ai: &'a AccountInfo<'info>,
    pub system_program: &'a AccountInfo<'info>,
    /// Optional fee context. When `Some`, the taker is charged the
    /// tier-derived fee; when `None`, no fee applies (0 bps).
    pub fee_ctx: Option<FeeCtx<'a, 'info>>,
}

/// Accounts needed to apply tier-based taker fees.
pub struct FeeCtx<'a, 'info> {
    /// Fee schedule PDA (read-only).
    pub fee_schedule_ai: &'a AccountInfo<'info>,
    /// Taker's UserVolume PDA (created lazily if not yet initialized).
    pub taker_volume_ai: &'a AccountInfo<'info>,
    /// Maker's UserVolume PDA (created lazily).
    pub maker_volume_ai: &'a AccountInfo<'info>,
    /// FeeTreasury PDA for the instrument's collateral mint (created lazily).
    pub fee_treasury_ai: &'a AccountInfo<'info>,
}

fn unpack_accounts<'a, 'info>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'info>],
) -> Result<MatchCtx<'a, 'info>, ProgramError> {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let long_margin_ai = next_account_info(iter)?;
    let short_margin_ai = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_signer(payer)?;
    assert_writable(book_ai)?;
    assert_writable(long_margin_ai)?;
    assert_writable(short_margin_ai)?;
    assert_writable(pmlc_ai)?;
    if instrument_ai.owner != program_id
        || book_ai.owner != program_id
        || long_margin_ai.owner != program_id
        || short_margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if system_program.key != &solana_program::system_program::ID {
        return Err(ProgramError::InvalidAccountData);
    }

    // Optional fee accounts: fee_schedule, taker_volume, maker_volume, fee_treasury.
    // Presence is detected by accounts length ≥ 11 (7 base + 4 fee).
    let fee_ctx = if accounts.len() >= 11 {
        let fee_schedule_ai = next_account_info(iter)?;
        let taker_volume_ai = next_account_info(iter)?;
        let maker_volume_ai = next_account_info(iter)?;
        let fee_treasury_ai = next_account_info(iter)?;
        Some(FeeCtx {
            fee_schedule_ai,
            taker_volume_ai,
            maker_volume_ai,
            fee_treasury_ai,
        })
    } else {
        None
    };

    Ok(MatchCtx {
        payer,
        instrument_ai,
        book_ai,
        long_margin_ai,
        short_margin_ai,
        pmlc_ai,
        system_program,
        fee_ctx,
    })
}

// ---------------------------------------------------------------------------
// On-chain scan: find the first long/short pair with overlap + remaining size
// ---------------------------------------------------------------------------

/// Linear scan: find first intent on `opposite_side` whose `[min, max]` range
/// overlaps `[target_min, target_max]`, has `contracts_remaining > 0`, and isn't
/// expired. Returns the intent `id`. O(n) over node slots — fine for sub-1k books.
pub fn find_overlap_on_side(
    book: &BookMut,
    opposite_side: u8,
    target_min: u64,
    target_max: u64,
    now_slot: u64,
    skip_id: u64,
) -> Result<Option<u64>, ProgramError> {
    for slot in book.nodes.iter() {
        if slot.tag() != NODE_TAG_INTENT {
            continue;
        }
        let node: &IntentNode = bytemuck::from_bytes(&slot.bytes);
        if node.side != opposite_side {
            continue;
        }
        if node.contracts_remaining == 0 {
            continue;
        }
        if node.expiration_slot <= now_slot {
            continue;
        }
        if node.id == skip_id {
            continue;
        }
        // Range overlap: max(a,c) <= min(b,d).
        if node.max_price_fp >= target_min && node.min_price_fp <= target_max {
            return Ok(Some(node.id));
        }
    }
    Ok(None)
}

/// Linear walk through intent nodes. Returns the first `(long_id, short_id)` pair
/// where both have `contracts_remaining > 0`, neither is expired, and their
/// ranges overlap.
pub fn scan_for_first_valid_pair(
    book: &BookMut,
    now_slot: u64,
) -> Result<Option<(u64, u64)>, ProgramError> {
    // Collect active longs sorted by post_seq. For a well-sized book (≤ few
    // hundred intents), a linear pass is fine.
    let mut longs: Vec<&IntentNode> = Vec::new();
    for slot in book.nodes.iter() {
        if slot.tag() != NODE_TAG_INTENT {
            continue;
        }
        let node: &IntentNode = bytemuck::from_bytes(&slot.bytes);
        if node.side != SIDE_LONG {
            continue;
        }
        if node.contracts_remaining == 0 {
            continue;
        }
        if node.expiration_slot <= now_slot {
            continue;
        }
        longs.push(node);
    }
    longs.sort_by_key(|n| n.post_seq);

    for long in longs {
        let mut found_short: Option<u64> = None;
        // Fast path: shorts whose interval contains long.min_price_fp.
        intent_tree::for_each_containing(book, SIDE_SHORT, long.min_price_fp, |_idx, s| {
            if s.contracts_remaining == 0 || s.expiration_slot <= now_slot {
                return true; // keep searching
            }
            if s.max_price_fp >= long.min_price_fp && s.min_price_fp <= long.max_price_fp {
                found_short = Some(s.id);
                return false;
            }
            true
        })?;
        if found_short.is_none() {
            // Fallback sweep: shorts containing long.max_price_fp.
            intent_tree::for_each_containing(book, SIDE_SHORT, long.max_price_fp, |_idx, s| {
                if s.contracts_remaining == 0 || s.expiration_slot <= now_slot {
                    return true;
                }
                if s.max_price_fp >= long.min_price_fp && s.min_price_fp <= long.max_price_fp {
                    found_short = Some(s.id);
                    return false;
                }
                true
            })?;
        }
        if let Some(short_id) = found_short {
            return Ok(Some((long.id, short_id)));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Core match logic (spec §4.6) — shared by both entry points
// ---------------------------------------------------------------------------

pub fn match_pair_core(
    program_id: &Pubkey,
    ctx: &MatchCtx<'_, '_>,
    long_intent_id: u64,
    short_intent_id: u64,
    max_contracts: u16,
) -> ProgramResult {
    // Snapshot instrument config.
    let (instrument_key, collateral_mint, collateral_bucket, leverage_bps, market_id, book_key) = {
        let data = ctx.instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        inst.require_active()?;
        (
            *ctx.instrument_ai.key,
            inst.collateral_mint,
            inst.collateral_bucket,
            inst.leverage_bps,
            inst.market_id,
            inst.intent_book,
        )
    };
    if book_key != *ctx.book_ai.key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    // --- Resolve intents + update tree inside one book borrow ---
    let (match_result, pmlc_id, pmlc_bump) = {
        let mut book_data = ctx.book_ai.try_borrow_mut_data()?;
        let mut book = BookMut::load(&mut book_data)?;

        let (long_idx, long_side) = find_intent_by_id(&book, long_intent_id)?;
        let (short_idx, short_side) = find_intent_by_id(&book, short_intent_id)?;
        if long_side != SIDE_LONG || short_side != SIDE_SHORT {
            return Err(PolyleverageError::NotOppositeSides.into());
        }

        let long = *book.intent(long_idx)?;
        let short = *book.intent(short_idx)?;

        let now_slot = Clock::get()?.slot;
        if long.expiration_slot <= now_slot || short.expiration_slot <= now_slot {
            return Err(PolyleverageError::IntentExpired.into());
        }
        if long.contracts_remaining == 0 || short.contracts_remaining == 0 {
            return Err(PolyleverageError::IntentNotFound.into());
        }

        let (overlap_min, overlap_max) = math::range_overlap(
            long.min_price_fp,
            long.max_price_fp,
            short.min_price_fp,
            short.max_price_fp,
        )
        .ok_or(PolyleverageError::NoRangeOverlap)?;
        let entry_fp = math::overlap_midpoint(overlap_min, overlap_max);

        let _requested_n = long
            .contracts_remaining
            .min(short.contracts_remaining)
            .min(max_contracts.max(1));
        // V1: 1 contract per ix (one PMLC allocated).
        let n_in_this_ix: u16 = 1;

        // Taker: intent with higher post_seq.
        let taker_is_long = long.post_seq > short.post_seq;

        // Update intent counters + reservation ledger in the node itself.
        let per_contract_reserve = collateral_bucket;
        let mut long = long;
        let mut short = short;
        long.contracts_remaining -= n_in_this_ix;
        short.contracts_remaining -= n_in_this_ix;
        long.reserved_collateral = long
            .reserved_collateral
            .checked_sub(per_contract_reserve * n_in_this_ix as u64)
            .ok_or(PolyleverageError::InsufficientReservedCollateral)?;
        short.reserved_collateral = short
            .reserved_collateral
            .checked_sub(per_contract_reserve * n_in_this_ix as u64)
            .ok_or(PolyleverageError::InsufficientReservedCollateral)?;

        let per_contract_fee_buf_long = long.fee_buffer / long.contracts_total.max(1) as u64;
        let per_contract_fee_buf_short = short.fee_buffer / short.contracts_total.max(1) as u64;
        long.fee_buffer = long
            .fee_buffer
            .saturating_sub(per_contract_fee_buf_long * n_in_this_ix as u64);
        short.fee_buffer = short
            .fee_buffer
            .saturating_sub(per_contract_fee_buf_short * n_in_this_ix as u64);

        *book.intent_mut(long_idx)? = long;
        *book.intent_mut(short_idx)? = short;

        if long.contracts_remaining == 0 {
            intent_tree::remove(&mut book, SIDE_LONG, long_idx)?;
            book.free_node(long_idx)?;
        }
        if short.contracts_remaining == 0 {
            intent_tree::remove(&mut book, SIDE_SHORT, short_idx)?;
            book.free_node(short_idx)?;
        }

        let pmlc_id = book.next_intent_id()?;
        let (pda, bump) = Pmlc::find_pda(program_id, &instrument_key, pmlc_id);
        if pda != *ctx.pmlc_ai.key {
            return Err(PolyleverageError::InvalidPda.into());
        }

        let long_trader = book.seat(long.owner_seat)?.trader;
        let short_trader = book.seat(short.owner_seat)?.trader;

        (
            MatchResult {
                long_trader,
                short_trader,
                entry_fp,
                n_contracts: n_in_this_ix,
                per_contract_fee_buf_long,
                per_contract_fee_buf_short,
                taker_is_long,
                long_reentry: (long.flags & INTENT_FLAG_REENTRY) != 0,
                short_reentry: (short.flags & INTENT_FLAG_REENTRY) != 0,
                long_range: (long.min_price_fp, long.max_price_fp),
                short_range: (short.min_price_fp, short.max_price_fp),
            },
            pmlc_id,
            bump,
        )
    };

    // --- Validate margin accounts ---
    {
        let data = ctx.long_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != match_result.long_trader || m.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }
    {
        let data = ctx.short_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != match_result.short_trader || m.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    let per_side_collateral = (match_result.n_contracts as u64) * collateral_bucket;
    let now_slot = Clock::get()?.slot;

    // Compute the taker's fee by looking up their tier. Requires FeeCtx.
    // When no fee_ctx is passed (e.g. inline match in PostIntent), fee = 0.
    let taker_fee_atoms: u64 = if let Some(fc) = &ctx.fee_ctx {
        // Ensure fee treasury exists (lazy init by whoever is paying rent).
        crate::processor::fees::ensure_fee_treasury(
            program_id,
            ctx.payer,
            fc.fee_treasury_ai,
            &collateral_mint,
            ctx.system_program,
        )?;

        // Ensure both traders have UserVolume accounts.
        let taker_pubkey = if match_result.taker_is_long {
            &match_result.long_trader
        } else {
            &match_result.short_trader
        };
        let maker_pubkey = if match_result.taker_is_long {
            &match_result.short_trader
        } else {
            &match_result.long_trader
        };
        ensure_user_volume(
            program_id,
            ctx.payer,
            fc.taker_volume_ai,
            taker_pubkey,
            &collateral_mint,
            ctx.system_program,
            now_slot,
        )?;
        ensure_user_volume(
            program_id,
            ctx.payer,
            fc.maker_volume_ai,
            maker_pubkey,
            &collateral_mint,
            ctx.system_program,
            now_slot,
        )?;

        // Look up taker's tier fee bps from schedule + their 30d volume.
        let fee_bps: u16 = {
            let sdata = fc.fee_schedule_ai.try_borrow_data()?;
            let sched = crate::state::FeeSchedule::load(&sdata)?;
            let vdata = fc.taker_volume_ai.try_borrow_data()?;
            let vol = crate::state::UserVolume::load(&vdata)?;
            let v30 = vol.effective_30d_volume(now_slot);
            sched.fee_for_volume(v30)
        };
        let fee_atoms = (per_side_collateral as u128 * fee_bps as u128
            / math::BPS_DENOM as u128) as u64;

        // Update volumes (both sides get credited for the matched notional).
        {
            let mut tdata = fc.taker_volume_ai.try_borrow_mut_data()?;
            let tvol = crate::state::UserVolume::load_mut(&mut tdata)?;
            tvol.add_volume(per_side_collateral, now_slot)?;
        }
        {
            let mut mdata = fc.maker_volume_ai.try_borrow_mut_data()?;
            let mvol = crate::state::UserVolume::load_mut(&mut mdata)?;
            mvol.add_volume(per_side_collateral, now_slot)?;
        }
        // Accrue fee into the treasury ledger.
        if fee_atoms > 0 {
            let mut tdata = fc.fee_treasury_ai.try_borrow_mut_data()?;
            let treasury = crate::state::FeeTreasury::load_mut(&mut tdata)?;
            treasury.accrue(fee_atoms)?;
        }
        fee_atoms
    } else {
        0
    };

    let fee_buf_long_total =
        match_result.per_contract_fee_buf_long * match_result.n_contracts as u64;
    let fee_buf_short_total =
        match_result.per_contract_fee_buf_short * match_result.n_contracts as u64;

    {
        let mut data = ctx.long_margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        m.move_reserved_to_locked(per_side_collateral)?;
        if fee_buf_long_total > 0 {
            m.move_reserved_to_free(fee_buf_long_total)?;
        }
        if match_result.taker_is_long && taker_fee_atoms > 0 {
            m.debit_free(taker_fee_atoms)?;
        }
    }
    {
        let mut data = ctx.short_margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        m.move_reserved_to_locked(per_side_collateral)?;
        if fee_buf_short_total > 0 {
            m.move_reserved_to_free(fee_buf_short_total)?;
        }
        if !match_result.taker_is_long && taker_fee_atoms > 0 {
            m.debit_free(taker_fee_atoms)?;
        }
    }

    // Create PMLC PDA.
    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(PMLC_LEN);
    let ix = solana_program::system_instruction::create_account(
        ctx.payer.key,
        ctx.pmlc_ai.key,
        lamports,
        PMLC_LEN as u64,
        program_id,
    );
    invoke_signed(
        &ix,
        &[
            ctx.payer.clone(),
            ctx.pmlc_ai.clone(),
            ctx.system_program.clone(),
        ],
        &[&[
            SEED_PMLC,
            instrument_key.as_ref(),
            &pmlc_id.to_le_bytes(),
            &[pmlc_bump],
        ]],
    )?;

    let notional = math::notional_atoms(collateral_bucket, leverage_bps)?;
    let size_fp = math::position_size_fp(notional, match_result.entry_fp)?;

    let mut flags = 0u8;
    if match_result.long_reentry {
        flags |= PMLC_FLAG_LONG_REENTRY;
    }
    if match_result.short_reentry {
        flags |= PMLC_FLAG_SHORT_REENTRY;
    }
    let now_slot = Clock::get()?.slot;
    let mut pmlc_data = ctx.pmlc_ai.try_borrow_mut_data()?;
    let p: &mut Pmlc = bytemuck::from_bytes_mut(&mut pmlc_data[..PMLC_LEN]);
    *p = Pmlc {
        discriminator: DISC_PMLC,
        status: PMLC_STATUS_LIVE,
        close_reason: 0,
        flags,
        bump: pmlc_bump,
        _pad0: [0; 3],
        id: pmlc_id,
        instrument: instrument_key,
        collateral_mint,
        market_id,
        long_owner: match_result.long_trader,
        short_owner: match_result.short_trader,
        collateral_per_side: collateral_bucket,
        leverage_bps,
        _pad1: [0; 4],
        entry_price_fp: match_result.entry_fp,
        notional_atoms: u64::try_from(notional).unwrap_or(u64::MAX),
        size_fp_lo: size_fp as u64,
        size_fp_hi: (size_fp >> 64) as u64,
        opened_slot: now_slot,
        last_mark_slot: now_slot,
        long_reentry_min_fp: match_result.long_range.0,
        long_reentry_max_fp: match_result.long_range.1,
        short_reentry_min_fp: match_result.short_range.0,
        short_reentry_max_fp: match_result.short_range.1,
        _reserved: [0; 32],
    };

    msg!(
        "pmlc {} matched at {} (taker_is_long={})",
        pmlc_id,
        match_result.entry_fp,
        match_result.taker_is_long
    );
    let _ = assert_pda; // silence if unused
    Ok(())
}

struct MatchResult {
    long_trader: Pubkey,
    short_trader: Pubkey,
    entry_fp: u64,
    n_contracts: u16,
    per_contract_fee_buf_long: u64,
    per_contract_fee_buf_short: u64,
    taker_is_long: bool,
    long_reentry: bool,
    short_reentry: bool,
    long_range: (u64, u64),
    short_range: (u64, u64),
}

/// Linear scan: find intent by id. Works for both sides; returns the side tag
/// alongside the node index.
fn find_intent_by_id(book: &BookMut, id: u64) -> Result<(u32, u8), ProgramError> {
    for (i, slot) in book.nodes.iter().enumerate() {
        if slot.tag() != NODE_TAG_INTENT {
            continue;
        }
        let node: &IntentNode = bytemuck::from_bytes(&slot.bytes);
        if node.id == id {
            return Ok((i as u32, node.side));
        }
    }
    Err(PolyleverageError::IntentNotFound.into())
}
