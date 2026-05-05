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
    processor::match_ix::{find_overlap_on_side, match_pair_core, FeeCtx, MatchCtx},
    seeds::{SEED_MARGIN, SEED_SESSION},
    state::{
        intent_tree, seat_tree, BookMut, InstrumentConfig, IntentNode, MarginAccount,
        ProgramConfig, Session, INTENT_FLAG_REENTRY, NODE_TAG_INTENT, NULL_IDX, RB_RED, SIDE_LONG,
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
///   5..11. (optional) inline-match extras: counterparty_margin, pmlc, system_program,
///          fee_schedule, taker_volume, maker_volume, fee_treasury
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

    do_post_intent_inner(
        program_id,
        owner.key,
        owner,
        program_config_ai,
        instrument_ai,
        book_ai,
        margin_ai,
        None,
        accounts,
        5, // inline-match extras start at index 5
        args,
    )
}

/// Delegated PostIntent — TEE-derived session delegate signs on
/// behalf of the owner. The polyleverage program verifies the session
/// is live, the instrument is allowlisted, and atomically charges the
/// session's cumulative cap before processing the post.
///
/// Accounts:
///   0. `[signer, writable]` fee_payer (typically SolanaRelayer; pays gas only)
///   1. `[signer]` delegate (TEE-derived per-user Solana key)
///   2. `[]` program config
///   3. `[]` instrument config
///   4. `[writable]` intent book
///   5. `[writable]` margin account PDA (session.owner, collateral_mint)
///   6. `[writable]` session PDA (seeds: [SEED_SESSION, session.owner])
///   7..13. (optional) inline-match extras: counterparty_margin, pmlc, system_program,
///          fee_schedule, taker_volume, maker_volume, fee_treasury
pub fn process_post_intent_delegated(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: PostIntentArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let fee_payer = next_account_info(iter)?;
    let delegate = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;
    let session_ai = next_account_info(iter)?;

    assert_signer(fee_payer)?;
    assert_writable(fee_payer)?;
    assert_signer(delegate)?;
    assert_writable(session_ai)?;
    if session_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Validate session: delegate match, active, instrument allowlisted.
    let now_slot = Clock::get()?.slot;
    let effective_owner = {
        let data = session_ai.try_borrow_data()?;
        let s = Session::load(&data)?;
        if s.delegate != *delegate.key {
            return Err(PolyleverageError::SessionDelegateMismatch.into());
        }
        if !s.is_active(now_slot) {
            return Err(PolyleverageError::SessionNotActive.into());
        }
        if !s.allows_instrument(instrument_ai.key) {
            return Err(PolyleverageError::SessionInstrumentNotAllowed.into());
        }
        // PDA validation happens here too: a forged session_ai with a
        // matching delegate would fail this check unless its seed-derived
        // address actually matches s.owner.
        let _ = assert_pda(
            &[SEED_SESSION, s.owner.as_ref()],
            program_id,
            session_ai.key,
        )?;
        s.owner
    };

    do_post_intent_inner(
        program_id,
        &effective_owner,
        fee_payer,
        program_config_ai,
        instrument_ai,
        book_ai,
        margin_ai,
        Some(session_ai),
        accounts,
        7, // inline-match extras start at index 7 (after the 7 fixed accounts)
        args,
    )
}

/// Shared core for owner-signed and delegate-signed PostIntent.
///
/// `effective_owner` is whose seat / margin / intent is created.
/// `payer` covers rent for any new accounts (PMLC in inline-match).
/// `session_ai`, when supplied, is charged via `record_intent` after
/// the reservation is computed — bounds-checked on-chain so a TEE
/// compromise can't drain users beyond the cumulative cap.
#[allow(clippy::too_many_arguments)]
fn do_post_intent_inner<'a, 'b>(
    program_id: &Pubkey,
    effective_owner: &Pubkey,
    payer: &'a AccountInfo<'b>,
    program_config_ai: &'a AccountInfo<'b>,
    instrument_ai: &'a AccountInfo<'b>,
    book_ai: &'a AccountInfo<'b>,
    margin_ai: &'a AccountInfo<'b>,
    session_ai: Option<&'a AccountInfo<'b>>,
    full_accounts: &'a [AccountInfo<'b>],
    inline_match_offset: usize,
    args: PostIntentArgs,
) -> ProgramResult {
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

    // Validate margin PDA — derived from effective_owner (which is either
    // the direct signer or the session.owner in the delegated path).
    let margin_bump = assert_pda(
        &[
            SEED_MARGIN,
            effective_owner.as_ref(),
            collateral_mint.as_ref(),
        ],
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

    // ── Session bounds enforcement (delegated path) ─────────────────
    //
    // Charge the cumulative cap atomically with the rest of the post.
    // record_intent enforces both per_intent_max and cumulative_cap; if
    // the latter is exceeded, the entire tx reverts and no on-chain
    // state changes (including this counter).
    if let Some(session_ai) = session_ai {
        let mut data = session_ai.try_borrow_mut_data()?;
        let s = Session::load_mut(&mut data)?;
        s.record_intent(total_reserve)?;
    }

    // Opportunistic prune-on-post: refund up to MAX_PRUNES_PER_POST of the
    // poster's own expired intents before reserving for this new one. Bounded
    // CU cost; only the poster's intents are touched here (other users'
    // margin accounts are not in scope, so we intentionally skip them — third
    // parties can use CancelIntent to clear those).
    const MAX_PRUNES_PER_POST: usize = 4;
    let mut pruned_refund: u64 = 0;
    {
        let mut book_data = book_ai.try_borrow_mut_data()?;
        let mut book = BookMut::load(&mut book_data)?;
        let seat_idx = seat_tree::find(&book, effective_owner)?;
        if seat_idx != NULL_IDX {
            // Collect up to MAX_PRUNES_PER_POST expired intents on this seat —
            // stack array avoids heap allocation on the hot path.
            let mut prune_buf: [(u8, u32, u64); MAX_PRUNES_PER_POST] =
                [(0u8, 0u32, 0u64); MAX_PRUNES_PER_POST];
            let mut prune_len: usize = 0;
            for (i, slot) in book.nodes.iter().enumerate() {
                if prune_len >= MAX_PRUNES_PER_POST {
                    break;
                }
                if slot.tag() != NODE_TAG_INTENT {
                    continue;
                }
                let node: &IntentNode = bytemuck::from_bytes(&slot.bytes);
                if node.owner_seat != seat_idx {
                    continue;
                }
                // Expired iff `expiration_slot <= now` (matches the convention
                // used at post-time validation and at match-time skip).
                if node.expiration_slot > now {
                    continue;
                }
                if node.contracts_remaining == 0 {
                    continue;
                }
                let total = node
                    .reserved_collateral
                    .checked_add(node.fee_buffer)
                    .ok_or(PolyleverageError::ArithmeticOverflow)?;
                prune_buf[prune_len] = (node.side, i as u32, total);
                prune_len += 1;
            }
            // Apply removals.
            for (side, idx, total) in prune_buf.iter().take(prune_len) {
                intent_tree::remove(&mut book, *side, *idx)?;
                book.free_node(*idx)?;
                pruned_refund = pruned_refund
                    .checked_add(*total)
                    .ok_or(PolyleverageError::ArithmeticOverflow)?;
            }
            if prune_len > 0 {
                let seat = book.seat_mut(seat_idx)?;
                seat.active_intent_count =
                    seat.active_intent_count.saturating_sub(prune_len as u32);
                msg!(
                    "prune_on_post pruned={} refund={}",
                    prune_len,
                    pruned_refund
                );
            }
        }
    }

    // Update margin account: refund any pruned reservations, then move free → reserved
    // for the new intent. Net effect: poster's stuck collateral is recycled into
    // their new post in the same tx.
    {
        let mut data = margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        if m.owner != *effective_owner
            || m.collateral_mint != collateral_mint
            || m.bump != margin_bump
        {
            return Err(PolyleverageError::InvalidPda.into());
        }
        if pruned_refund > 0 {
            m.move_reserved_to_free(pruned_refund)?;
        }
        m.move_free_to_reserved(total_reserve)?;
    }

    // Allocate the intent node and insert into tree.
    let mut book_data = book_ai.try_borrow_mut_data()?;
    let mut book = BookMut::load(&mut book_data)?;

    // Find or create seat for the effective owner.
    let seat_idx = seat_tree::find_or_create(&mut book, *effective_owner)?;
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
    // Required extra accounts for matching:
    //   0. [writable] counterparty's margin account
    //   1. [writable] new PMLC PDA
    //   2. []         system program
    //   3. []         fee schedule
    //   4. [writable] taker UserVolume PDA
    //   5. [writable] maker UserVolume PDA
    //   6. [writable] fee treasury PDA
    //
    // Client pre-scans the book to predict the counterparty, derives the correct
    // counterparty margin PDA and the next PMLC PDA, and includes them here.
    // On mismatch (book moved between client scan and this ix), the inline match
    // fails silently — the post still succeeds. Client can then call
    // `MatchBestAvailable` in a follow-up tx.
    if args.try_match != 0 {
        if full_accounts.len() < inline_match_offset + 7 {
            msg!("inline match skipped: fee accounts required");
        } else {
            let counterparty_margin_ai = &full_accounts[inline_match_offset];
            let pmlc_ai = &full_accounts[inline_match_offset + 1];
            let system_program_ai = &full_accounts[inline_match_offset + 2];
            let fee_schedule_ai = &full_accounts[inline_match_offset + 3];
            let taker_volume_ai = &full_accounts[inline_match_offset + 4];
            let maker_volume_ai = &full_accounts[inline_match_offset + 5];
            let fee_treasury_ai = &full_accounts[inline_match_offset + 6];

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
                    payer,
                    instrument_ai,
                    book_ai,
                    long_margin_ai,
                    short_margin_ai,
                    pmlc_ai,
                    system_program: system_program_ai,
                    fee_ctx: Some(FeeCtx {
                        fee_schedule_ai,
                        taker_volume_ai,
                        maker_volume_ai,
                        fee_treasury_ai,
                    }),
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
    }

    let _ = market_id;

    Ok(())
}

/// Accounts:
///   0. `[signer]` caller (the original intent owner, OR anyone if intent is expired)
///   1. `[]` instrument config
///   2. `[writable]` intent book
///   3. `[writable]` margin account of the intent's original owner — refunds always
///      flow back to the original owner regardless of who calls cancel.
pub fn process_cancel_intent(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CancelIntentArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let caller = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;

    assert_signer(caller)?;
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

    let now_slot = solana_program::clock::Clock::get()?.slot;

    // Find intent by id in both trees (cancel by id lookup is O(n); for Phase 1 we
    // accept that since cancels are less frequent than posts). Phase 2 can add a
    // per-seat index or require caller to pass the node_idx directly.
    let mut book_data = book_ai.try_borrow_mut_data()?;
    let mut book = BookMut::load(&mut book_data)?;

    let (node_idx, side) = find_intent_by_id(&book, args.intent_id)?;

    // Resolve original owner via the intent's seat. The caller may differ from
    // the original owner, but only when the intent has expired (self-service
    // cleanup so anyone can unstick a stale order).
    //
    // Expiration convention: an intent is expired iff `expiration_slot <= now`
    // — same threshold the matching engine uses to skip stale intents.
    let (collateral_release, fee_release, owner_seat, original_owner, is_expired) = {
        let node = book.intent(node_idx)?;
        let seat = book.seat(node.owner_seat)?;
        let is_expired = node.expiration_slot <= now_slot;
        (
            node.reserved_collateral,
            node.fee_buffer,
            node.owner_seat,
            seat.trader,
            is_expired,
        )
    };

    // Authorize: original owner can always cancel; third parties only after expiry.
    if *caller.key != original_owner && !is_expired {
        return Err(PolyleverageError::MissingSigner.into());
    }

    // Margin PDA always derives from the original owner — refund flows to them.
    let margin_bump = assert_pda(
        &[
            SEED_MARGIN,
            original_owner.as_ref(),
            collateral_mint.as_ref(),
        ],
        program_id,
        margin_ai.key,
    )?;

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
    if m.owner != original_owner || m.collateral_mint != collateral_mint || m.bump != margin_bump {
        return Err(PolyleverageError::InvalidPda.into());
    }
    m.move_reserved_to_free(total)?;
    Ok(())
}

/// Delegated CancelIntent — TEE-derived session delegate cancels on
/// behalf of the owner. Refund still flows to the original owner's
/// margin (regardless of who the caller is — same property as
/// `process_cancel_intent` for third-party-cancel-of-expired).
///
/// Cancel doesn't charge against the cumulative cap (it RELEASES
/// reserved collateral; doesn't commit new). The session's role here
/// is purely auth: "this delegate can cancel intents owned by
/// session.owner."
///
/// Accounts:
///   0. `[signer, writable]` fee_payer (pays gas)
///   1. `[signer]` delegate
///   2. `[]` instrument config
///   3. `[writable]` intent book
///   4. `[writable]` margin account of session.owner
///   5. `[]` session PDA (read-only — auth check only, no state change)
pub fn process_cancel_intent_delegated(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CancelIntentArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let fee_payer = next_account_info(iter)?;
    let delegate = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;
    let session_ai = next_account_info(iter)?;

    assert_signer(fee_payer)?;
    assert_writable(fee_payer)?;
    assert_signer(delegate)?;
    assert_writable(book_ai)?;
    assert_writable(margin_ai)?;
    if instrument_ai.owner != program_id
        || book_ai.owner != program_id
        || margin_ai.owner != program_id
        || session_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    let now_slot = Clock::get()?.slot;
    // Auth via session.
    let session_owner = {
        let data = session_ai.try_borrow_data()?;
        let s = Session::load(&data)?;
        if s.delegate != *delegate.key {
            return Err(PolyleverageError::SessionDelegateMismatch.into());
        }
        if !s.is_active(now_slot) {
            return Err(PolyleverageError::SessionNotActive.into());
        }
        if !s.allows_instrument(instrument_ai.key) {
            return Err(PolyleverageError::SessionInstrumentNotAllowed.into());
        }
        let _ = assert_pda(
            &[SEED_SESSION, s.owner.as_ref()],
            program_id,
            session_ai.key,
        )?;
        s.owner
    };

    // Load instrument + validate book PDA.
    let (collateral_mint, instrument_book) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        (inst.collateral_mint, inst.intent_book)
    };
    if instrument_book != *book_ai.key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    let mut book_data = book_ai.try_borrow_mut_data()?;
    let mut book = BookMut::load(&mut book_data)?;
    let (node_idx, side) = find_intent_by_id(&book, args.intent_id)?;

    // The intent must belong to session.owner — otherwise the delegate
    // would be cancelling someone else's intent.
    let (collateral_release, fee_release, owner_seat, original_owner) = {
        let node = book.intent(node_idx)?;
        let seat = book.seat(node.owner_seat)?;
        (
            node.reserved_collateral,
            node.fee_buffer,
            node.owner_seat,
            seat.trader,
        )
    };
    if original_owner != session_owner {
        return Err(PolyleverageError::SessionOwnerMismatch.into());
    }

    // Margin PDA is for the original owner (= session.owner).
    let margin_bump = assert_pda(
        &[
            SEED_MARGIN,
            original_owner.as_ref(),
            collateral_mint.as_ref(),
        ],
        program_id,
        margin_ai.key,
    )?;

    intent_tree::remove(&mut book, side, node_idx)?;
    book.free_node(node_idx)?;
    let seat = book.seat_mut(owner_seat)?;
    seat.active_intent_count = seat.active_intent_count.saturating_sub(1);

    let total = collateral_release
        .checked_add(fee_release)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;
    drop(book_data);
    let mut data = margin_ai.try_borrow_mut_data()?;
    let m = MarginAccount::load_mut(&mut data)?;
    if m.owner != original_owner || m.collateral_mint != collateral_mint || m.bump != margin_bump {
        return Err(PolyleverageError::InvalidPda.into());
    }
    m.move_reserved_to_free(total)?;
    let _ = fee_payer; // silence unused — needed only as fee_payer at the tx level
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
