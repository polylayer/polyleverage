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
    seeds::{SEED_FEE_SCHEDULE, SEED_MARGIN, SEED_PMLC, SEED_USER_VOLUME},
    state::{
        intent_tree, BookMut, InstrumentConfig, IntentNode, MarginAccount, Pmlc, UserVolume,
        DISC_PMLC, INTENT_FLAG_REENTRY, NODE_TAG_INTENT, PMLC_FLAG_LONG_REENTRY,
        PMLC_FLAG_SHORT_REENTRY, PMLC_LEN, PMLC_STATUS_LIVE, SIDE_LONG, SIDE_SHORT,
        USER_VOLUME_LEN,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Validate or lazy-init a UserVolume PDA.
fn ensure_user_volume<'info>(
    program_id: &Pubkey,
    payer: &AccountInfo<'info>,
    volume_ai: &AccountInfo<'info>,
    owner: &Pubkey,
    mint: &Pubkey,
    system_program: &AccountInfo<'info>,
    now_slot: u64,
) -> ProgramResult {
    let bump = assert_pda(
        &[SEED_USER_VOLUME, owner.as_ref(), mint.as_ref()],
        program_id,
        volume_ai.key,
    )?;
    if volume_ai.data_len() != 0 {
        if volume_ai.owner != program_id {
            return Err(PolyleverageError::InvalidAccountOwner.into());
        }
        let data = volume_ai.try_borrow_data()?;
        let volume = UserVolume::load(&data)?;
        if volume.owner != *owner || volume.collateral_mint != *mint || volume.bump != bump {
            return Err(PolyleverageError::InvalidPda.into());
        }
        return Ok(());
    }
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

pub(crate) fn release_fee_buffer_for_fill(
    fee_buffer: u64,
    contracts_remaining_before: u16,
    fill_contracts: u16,
) -> Result<u64, ProgramError> {
    if fill_contracts == 0
        || contracts_remaining_before == 0
        || fill_contracts > contracts_remaining_before
    {
        return Err(PolyleverageError::InvalidContractCount.into());
    }
    if fill_contracts == contracts_remaining_before {
        return Ok(fee_buffer);
    }
    u64::try_from(fee_buffer as u128 * fill_contracts as u128 / contracts_remaining_before as u128)
        .map_err(|_| PolyleverageError::ArithmeticOverflow.into())
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
/// The book is two price-sorted trees. The best (most aggressive) live long
/// and short are each reached in `O(log n)`; they cross iff
/// `long.price >= short.price`.
///
/// Returns `IntentNotFound` if no crossing pair exists.
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
    /// Fee context used to charge the taker and update both users' volumes.
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

    if accounts.len() < 11 {
        return Err(PolyleverageError::FeeAccountsRequired.into());
    }
    let fee_schedule_ai = next_account_info(iter)?;
    let taker_volume_ai = next_account_info(iter)?;
    let maker_volume_ai = next_account_info(iter)?;
    let fee_treasury_ai = next_account_info(iter)?;
    assert_writable(taker_volume_ai)?;
    assert_writable(maker_volume_ai)?;
    assert_writable(fee_treasury_ai)?;
    if fee_schedule_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    assert_pda(&[SEED_FEE_SCHEDULE], program_id, fee_schedule_ai.key)?;
    let fee_ctx = Some(FeeCtx {
        fee_schedule_ai,
        taker_volume_ai,
        maker_volume_ai,
        fee_treasury_ai,
    });

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
// On-chain matching discovery
// ---------------------------------------------------------------------------

/// Find the best live intent on `opposite_side` that crosses a taker resting
/// at `taker_price`. A long crosses a short iff `long.price >= short.price`.
/// `O(log n)` — the side's best-priced live order; if it doesn't cross,
/// nothing worse-priced will. `skip_id` excludes one intent (the taker's own).
pub fn find_overlap_on_side(
    book: &BookMut,
    opposite_side: u8,
    taker_price: u64,
    now_slot: u64,
    skip_id: u64,
) -> Result<Option<u64>, ProgramError> {
    let mut found: Option<u64> = None;
    intent_tree::for_each_best_first(book, opposite_side, |_idx, node| {
        if node.contracts_remaining == 0
            || node.expiration_slot <= now_slot
            || node.id == skip_id
        {
            return true; // skip dead / self, keep walking
        }
        // First live candidate is this side's best price. It crosses the
        // taker, or nothing does.
        let crosses = if opposite_side == SIDE_SHORT {
            taker_price >= node.price_fp // taker is long
        } else {
            node.price_fp >= taker_price // taker is short
        };
        if crosses {
            found = Some(node.id);
        }
        false // stop at the first live candidate either way
    })?;
    Ok(found)
}

/// Returns the best crossing `(long_id, short_id)` pair, or `None`.
///
/// The highest live bid and the lowest live ask are each reached in
/// `O(log n)`; they cross iff `long.price >= short.price`, and if those two
/// don't cross no pair does. Price-time priority falls out of the trees'
/// best-first ordering.
///
/// This is the permissionless backstop matcher. The hot path is keeper-driven
/// `MatchPair`, which is handed the pair directly.
pub fn scan_for_first_valid_pair(
    book: &BookMut,
    now_slot: u64,
) -> Result<Option<(u64, u64)>, ProgramError> {
    let best_long = intent_tree::first_live(book, SIDE_LONG, now_slot)?;
    let best_short = intent_tree::first_live(book, SIDE_SHORT, now_slot)?;
    match (best_long, best_short) {
        (Some((_, long)), Some((_, short))) if long.price_fp >= short.price_fp => {
            Ok(Some((long.id, short.id)))
        }
        _ => Ok(None),
    }
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

        // A long crosses a short iff it will pay at least the short's ask.
        if long.price_fp < short.price_fp {
            return Err(PolyleverageError::NoRangeOverlap.into());
        }
        let entry_fp = math::price_midpoint(long.price_fp, short.price_fp);

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
        let long_remaining_before = long.contracts_remaining;
        let short_remaining_before = short.contracts_remaining;
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

        let fee_buf_release_long = if long_remaining_before == n_in_this_ix {
            long.fee_buffer
        } else {
            release_fee_buffer_for_fill(long.fee_buffer, long_remaining_before, n_in_this_ix)?
        };
        let fee_buf_release_short = if short_remaining_before == n_in_this_ix {
            short.fee_buffer
        } else {
            release_fee_buffer_for_fill(short.fee_buffer, short_remaining_before, n_in_this_ix)?
        };
        long.fee_buffer = long
            .fee_buffer
            .checked_sub(fee_buf_release_long)
            .ok_or(PolyleverageError::ArithmeticUnderflow)?;
        short.fee_buffer = short
            .fee_buffer
            .checked_sub(fee_buf_release_short)
            .ok_or(PolyleverageError::ArithmeticUnderflow)?;

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
                fee_buf_release_long,
                fee_buf_release_short,
                taker_is_long,
                long_reentry: (long.flags & INTENT_FLAG_REENTRY) != 0,
                short_reentry: (short.flags & INTENT_FLAG_REENTRY) != 0,
                long_price: long.price_fp,
                short_price: short.price_fp,
            },
            pmlc_id,
            bump,
        )
    };

    // --- Validate margin accounts ---
    {
        let bump = assert_pda(
            &[
                SEED_MARGIN,
                match_result.long_trader.as_ref(),
                collateral_mint.as_ref(),
            ],
            program_id,
            ctx.long_margin_ai.key,
        )?;
        let data = ctx.long_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != match_result.long_trader
            || m.collateral_mint != collateral_mint
            || m.bump != bump
        {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }
    {
        let bump = assert_pda(
            &[
                SEED_MARGIN,
                match_result.short_trader.as_ref(),
                collateral_mint.as_ref(),
            ],
            program_id,
            ctx.short_margin_ai.key,
        )?;
        let data = ctx.short_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != match_result.short_trader
            || m.collateral_mint != collateral_mint
            || m.bump != bump
        {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    let per_side_collateral = (match_result.n_contracts as u64) * collateral_bucket;
    let now_slot = Clock::get()?.slot;

    // Compute the taker's fee by looking up their tier. Requires FeeCtx.
    let fc = ctx
        .fee_ctx
        .as_ref()
        .ok_or(PolyleverageError::FeeAccountsRequired)?;
    let taker_fee_atoms: u64 = {
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
        let fee_atoms =
            (per_side_collateral as u128 * fee_bps as u128 / math::BPS_DENOM as u128) as u64;

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
    };

    let fee_buf_long_total = match_result.fee_buf_release_long;
    let fee_buf_short_total = match_result.fee_buf_release_short;

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
        long_reentry_price_fp: match_result.long_price,
        short_reentry_price_fp: match_result.short_price,
        _reserved: [0; 48],
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
    fee_buf_release_long: u64,
    fee_buf_release_short: u64,
    taker_is_long: bool,
    long_reentry: bool,
    short_reentry: bool,
    long_price: u64,
    short_price: u64,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{init_intent_book, intent_book_byte_size, NULL_IDX, RB_RED};
    use solana_program::pubkey::Pubkey;

    fn mk_book(capacity: u32) -> Vec<u8> {
        let mut buf = vec![0u8; intent_book_byte_size(capacity)];
        init_intent_book(&mut buf, Pubkey::new_unique(), capacity, 0, 0).unwrap();
        buf
    }

    fn mk_intent(side: u8, price: u64, seq: u64, id: u64) -> IntentNode {
        IntentNode {
            tag: NODE_TAG_INTENT,
            side,
            color: RB_RED,
            flags: 0,
            left: NULL_IDX,
            right: NULL_IDX,
            parent: NULL_IDX,
            _pad0: [0; 2],
            price_fp: price,
            _pad1: [0; 2],
            id,
            owner_seat: 1,
            contracts_total: 1,
            contracts_remaining: 1,
            expiration_slot: u64::MAX,
            post_seq: seq,
            reserved_collateral: 0,
            fee_buffer: 0,
        }
    }

    fn put(book: &mut BookMut, side: u8, price: u64, seq: u64, id: u64) {
        let idx = book.alloc_node().unwrap();
        book.write_intent(idx, mk_intent(side, price, seq, id)).unwrap();
        intent_tree::insert(book, side, idx).unwrap();
    }

    #[test]
    fn scan_finds_crossing_pair() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        // Long bids 600, short asks 300 → long.price >= short.price → cross.
        put(&mut book, SIDE_LONG, 600, 1, 1);
        put(&mut book, SIDE_SHORT, 300, 2, 2);
        assert_eq!(scan_for_first_valid_pair(&book, 0).unwrap(), Some((1, 2)));
    }

    #[test]
    fn scan_rejects_non_crossing_book() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        // Long bids only 200, short asks 500 → spread, no cross.
        put(&mut book, SIDE_LONG, 200, 1, 1);
        put(&mut book, SIDE_SHORT, 500, 2, 2);
        assert_eq!(scan_for_first_valid_pair(&book, 0).unwrap(), None);
    }

    #[test]
    fn scan_picks_best_priced_pair() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        // Two longs, two shorts. Best bid = 700, best ask = 300.
        put(&mut book, SIDE_LONG, 500, 1, 1);
        put(&mut book, SIDE_LONG, 700, 2, 2);
        put(&mut book, SIDE_SHORT, 400, 3, 3);
        put(&mut book, SIDE_SHORT, 300, 4, 4);
        assert_eq!(scan_for_first_valid_pair(&book, 0).unwrap(), Some((2, 4)));
    }

    #[test]
    fn find_overlap_matches_crossing_and_rejects_spread() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        put(&mut book, SIDE_SHORT, 300, 1, 7);

        // A long taker bidding 600 crosses the 300 ask.
        assert_eq!(
            find_overlap_on_side(&book, SIDE_SHORT, 600, 0, u64::MAX).unwrap(),
            Some(7)
        );
        // A long taker bidding only 200 does not.
        assert_eq!(
            find_overlap_on_side(&book, SIDE_SHORT, 200, 0, u64::MAX).unwrap(),
            None
        );
    }

    #[test]
    fn fee_buffer_release_spreads_remaining_buffer() {
        assert_eq!(release_fee_buffer_for_fill(101, 3, 1).unwrap(), 33);
        assert_eq!(release_fee_buffer_for_fill(68, 2, 1).unwrap(), 34);
    }

    #[test]
    fn fee_buffer_release_final_fill_releases_dust() {
        assert_eq!(release_fee_buffer_for_fill(34, 1, 1).unwrap(), 34);
        assert_eq!(release_fee_buffer_for_fill(2, 1, 1).unwrap(), 2);
    }

    #[test]
    fn fee_buffer_release_rejects_impossible_fill() {
        assert!(matches!(
            release_fee_buffer_for_fill(10, 0, 1),
            Err(ProgramError::Custom(code)) if code == PolyleverageError::InvalidContractCount as u32
        ));
        assert!(matches!(
            release_fee_buffer_for_fill(10, 1, 2),
            Err(ProgramError::Custom(code)) if code == PolyleverageError::InvalidContractCount as u32
        ));
    }
}
