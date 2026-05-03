//! Intent handlers: post, cancel.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::{CancelIntentArgs, PostIntentArgs},
    math,
    processor::match_ix::{find_overlap_on_side, match_pair_core, MatchCtx},
    seeds::SEED_MARGIN,
    state::{
        intent_tree, seat_tree, BookMut, InstrumentConfig, IntentNode, MarginAccount,
        ProgramConfig, INTENT_FLAG_REENTRY, NODE_TAG_INTENT, NULL_IDX, RB_RED, SIDE_LONG,
        SIDE_SHORT,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Compute the fee_buffer for a freshly posted intent at current `max_tier_fee_bps`.
#[inline]
pub(crate) fn fee_buffer_for(
    contracts: u16,
    collateral_bucket: u64,
    max_tier_fee_bps: u16,
) -> Result<u64, ProgramError> {
    (contracts as u128)
        .checked_mul(collateral_bucket as u128)
        .and_then(|v| v.checked_mul(max_tier_fee_bps as u128))
        .and_then(|v| v.checked_div(math::BPS_DENOM as u128))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(|| PolyleverageError::ArithmeticOverflow.into())
}

/// Accounts:
///   0. `[signer]` owner
///   1. `[]` program config
///   2. `[]` instrument config
///   3. `[writable]` intent book
///   4. `[writable]` margin account PDA (owner, instrument.collateral_mint)
pub fn process_post_intent(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: PostIntentArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(book_ai)?;
    assert_writable(margin_ai)?;
    if program_config_ai.owner != program_id
        || instrument_ai.owner != program_id
        || book_ai.owner != program_id
        || margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Check global pause.
    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.global_pause != 0 {
            return Err(PolyleverageError::GlobalPaused.into());
        }
    }

    // Snapshot instrument config.
    let (market_id, collateral_mint, _leverage_bps, collateral_bucket, tick_fp, instrument_book) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        inst.require_active()?;
        (
            inst.market_id,
            inst.collateral_mint,
            inst.leverage_bps,
            inst.collateral_bucket,
            inst.tick_fp,
            inst.intent_book,
        )
    };

    // Validate book PDA matches the instrument's book.
    if instrument_book != *book_ai.key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    // Validate margin PDA.
    let margin_bump = assert_pda(
        &[SEED_MARGIN, owner.key.as_ref(), collateral_mint.as_ref()],
        program_id,
        margin_ai.key,
    )?;

    // Validate args.
    if args.side != SIDE_LONG && args.side != SIDE_SHORT {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }
    if args.contracts == 0 {
        return Err(PolyleverageError::InvalidContractCount.into());
    }
    math::validate_range(args.min_price_fp, args.max_price_fp, tick_fp)?;
    let now = solana_program::clock::Clock::get()?.slot;
    if args.expiration_slot <= now {
        return Err(PolyleverageError::IntentExpired.into());
    }

    // Compute reservations.
    let collateral_reserve = (args.contracts as u64)
        .checked_mul(collateral_bucket)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;

    // Load current max_tier_fee_bps (cached in book header).
    let max_fee_bps = {
        let data = book_ai.try_borrow_data()?;
        let book = crate::state::BookRef::load(&data)?;
        book.header.cached_max_fee_bps
    };
    let fee_buffer = fee_buffer_for(args.contracts, collateral_bucket, max_fee_bps)?;
    let total_reserve = collateral_reserve
        .checked_add(fee_buffer)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;

    // Update margin account: move free → reserved.
    {
        let mut data = margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        if m.owner != *owner.key || m.collateral_mint != collateral_mint || m.bump != margin_bump {
            return Err(PolyleverageError::InvalidPda.into());
        }
        m.move_free_to_reserved(total_reserve)?;
    }

    // Allocate the intent node and insert into tree.
    let mut book_data = book_ai.try_borrow_mut_data()?;
    let mut book = BookMut::load(&mut book_data)?;

    // Find or create seat for owner.
    let seat_idx = seat_tree::find_or_create(&mut book, *owner.key)?;
    book.seat_mut(seat_idx)?.active_intent_count = book
        .seat(seat_idx)?
        .active_intent_count
        .checked_add(1)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;

    let intent_id = book.next_intent_id()?;
    let post_seq = book.next_seq()?;
    let node_idx = book.alloc_node()?;
    let intent = IntentNode {
        tag: NODE_TAG_INTENT,
        side: args.side,
        color: RB_RED,
        flags: if args.reentry_enabled != 0 {
            INTENT_FLAG_REENTRY
        } else {
            0
        },
        left: NULL_IDX,
        right: NULL_IDX,
        parent: NULL_IDX,
        _pad0: [0; 2],
        min_price_fp: args.min_price_fp,
        max_price_fp: args.max_price_fp,
        subtree_max_fp: args.max_price_fp,
        id: intent_id,
        owner_seat: seat_idx,
        contracts_total: args.contracts,
        contracts_remaining: args.contracts,
        expiration_slot: args.expiration_slot,
        post_seq,
        reserved_collateral: collateral_reserve,
        fee_buffer,
    };
    book.write_intent(node_idx, intent)?;
    intent_tree::insert(&mut book, args.side, node_idx)?;

    msg!("intent posted id={} seq={}", intent_id, post_seq);
    drop(book_data); // release so inline match can re-borrow

    // ---- Event-driven inline match ----
    //
    // If caller set try_match=true AND provided the extra accounts, scan the
    // opposite side of the book for the first overlap with the freshly-posted
    // intent. If found, delegate to match_pair_core — which will match the pair,
    // settle collateral, and create the PMLC all within this same tx.
    //
    // Required extra accounts (at indices 5, 6, 7 in `accounts`):
    //   5. [writable] counterparty's margin account
    //   6. [writable] new PMLC PDA
    //   7. []         system program
    //
    // Client pre-scans the book to predict the counterparty, derives the correct
    // counterparty margin PDA and the next PMLC PDA, and includes them here.
    // On mismatch (book moved between client scan and this ix), the inline match
    // fails silently — the post still succeeds. Client can then call
    // `MatchBestAvailable` in a follow-up tx.
    if args.try_match != 0 && accounts.len() >= 8 {
        let counterparty_margin_ai = &accounts[5];
        let pmlc_ai = &accounts[6];
        let system_program_ai = &accounts[7];

        let now_slot = Clock::get()?.slot;
        let opposite_side = if args.side == SIDE_LONG {
            SIDE_SHORT
        } else {
            SIDE_LONG
        };

        // Scan for an overlapping counterparty intent.
        let counterparty_id = {
            let mut data = book_ai.try_borrow_mut_data()?;
            let book = BookMut::load(&mut data)?;
            find_overlap_on_side(
                &book,
                opposite_side,
                args.min_price_fp,
                args.max_price_fp,
                now_slot,
                intent_id,
            )?
        };

        if let Some(cp_id) = counterparty_id {
            // Build MatchCtx. Poster becomes either the long or short participant
            // based on their side.
            let (long_id, short_id, long_margin_ai, short_margin_ai) = if args.side == SIDE_LONG
            {
                (intent_id, cp_id, margin_ai, counterparty_margin_ai)
            } else {
                (cp_id, intent_id, counterparty_margin_ai, margin_ai)
            };

            let ctx = MatchCtx {
                payer: owner,
                instrument_ai,
                book_ai,
                long_margin_ai,
                short_margin_ai,
                pmlc_ai,
                system_program: system_program_ai,
                fee_ctx: None, // inline match is fee-free (post directly to avoid fees).
            };

            // V1: one contract per ix; caller can re-post for multi-contract fills.
            match match_pair_core(program_id, &ctx, long_id, short_id, 1) {
                Ok(()) => msg!("inline match ok: long={} short={}", long_id, short_id),
                Err(err) => {
                    // Post succeeded; match is best-effort. Log + swallow so the
                    // post itself is preserved.
                    msg!("inline match skipped: {:?}", err);
                }
            }
        } else {
            msg!("inline match skipped: no overlapping counterparty");
        }
    }

    let _ = market_id;

    Ok(())
}

/// Accounts:
///   0. `[signer]` owner
///   1. `[]` instrument config
///   2. `[writable]` intent book
///   3. `[writable]` margin account
pub fn process_cancel_intent(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CancelIntentArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(book_ai)?;
    assert_writable(margin_ai)?;
    if instrument_ai.owner != program_id
        || book_ai.owner != program_id
        || margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Load + validate via instrument.
    let (collateral_mint, instrument_book) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        (inst.collateral_mint, inst.intent_book)
    };
    if instrument_book != *book_ai.key {
        return Err(PolyleverageError::InvalidPda.into());
    }
    let margin_bump = assert_pda(
        &[SEED_MARGIN, owner.key.as_ref(), collateral_mint.as_ref()],
        program_id,
        margin_ai.key,
    )?;

    // Find intent by id in both trees (cancel by id lookup is O(n); for Phase 1 we
    // accept that since cancels are less frequent than posts). Phase 2 can add a
    // per-seat index or require caller to pass the node_idx directly.
    let mut book_data = book_ai.try_borrow_mut_data()?;
    let mut book = BookMut::load(&mut book_data)?;

    let (node_idx, side) = find_intent_by_id(&book, args.intent_id)?;

    let (collateral_release, fee_release, owner_seat) = {
        let node = book.intent(node_idx)?;
        // Must be cancellable only by owner.
        let seat = book.seat(node.owner_seat)?;
        if seat.trader != *owner.key {
            return Err(PolyleverageError::MissingSigner.into());
        }
        (node.reserved_collateral, node.fee_buffer, node.owner_seat)
    };

    // Remove from tree, free node, decrement seat counter.
    intent_tree::remove(&mut book, side, node_idx)?;
    book.free_node(node_idx)?;
    let seat = book.seat_mut(owner_seat)?;
    seat.active_intent_count = seat.active_intent_count.saturating_sub(1);

    // Release reservations.
    let total = collateral_release
        .checked_add(fee_release)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;
    drop(book_data);
    let mut data = margin_ai.try_borrow_mut_data()?;
    let m = MarginAccount::load_mut(&mut data)?;
    if m.owner != *owner.key || m.collateral_mint != collateral_mint || m.bump != margin_bump {
        return Err(PolyleverageError::InvalidPda.into());
    }
    m.move_reserved_to_free(total)?;
    Ok(())
}

/// Linear scan — finds the node idx and side for the given `intent_id`. Returns
/// `IntentNotFound` if absent.
fn find_intent_by_id(book: &BookMut, id: u64) -> Result<(u32, u8), ProgramError> {
    for (i, slot) in book.nodes.iter().enumerate() {
        if slot.tag() != crate::state::NODE_TAG_INTENT {
            continue;
        }
        let node: &IntentNode = bytemuck::from_bytes(&slot.bytes);
        if node.id == id {
            return Ok((i as u32, node.side));
        }
    }
    Err(PolyleverageError::IntentNotFound.into())
}
