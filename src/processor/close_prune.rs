//! Mutual close + expired intent pruning.
//!
//! - `CloseMutual`: both counterparties sign; PMLC settles at a CRE-attested mark.
//! - `PruneExpired`: anyone can call; frees nodes whose `expiration_slot` has passed
//!   and releases reservations back to owners' free_collateral. Earns a small bounty
//!   per pruned intent (debited from the expired owner's fee_buffer release).

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    pubkey::Pubkey,
    sysvar::{instructions as sysvar_instructions, Sysvar},
};

use crate::{
    attestation::{self, Attestation, ATT_TYPE_PRICE_TWAP},
    error::PolyleverageError,
    instruction::CloseMutualArgs,
    math,
    seeds::SEED_MARKET_NONCE,
    state::{
        intent_tree, BookMut, InstrumentConfig, IntentNode, MarginAccount, MarketNonceAccount,
        Pmlc, ProgramConfig, CLOSE_REASON_MUTUAL, NODE_TAG_INTENT, PMLC_STATUS_CLOSED, SIDE_LONG,
        SIDE_SHORT,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Accounts:
///   0. `[signer]` long_owner
///   1. `[signer]` short_owner
///   2. `[]` program config
///   3. `[]` instructions sysvar
///   4. `[]` instrument config
///   5. `[writable]` pmlc
///   6. `[writable]` market nonce
///   7. `[writable]` long margin account
///   8. `[writable]` short margin account
pub fn process_close_mutual(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CloseMutualArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let long_owner = next_account_info(iter)?;
    let short_owner = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let sysvar_ix_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;
    let market_nonce_ai = next_account_info(iter)?;
    let long_margin_ai = next_account_info(iter)?;
    let short_margin_ai = next_account_info(iter)?;

    assert_signer(long_owner)?;
    assert_signer(short_owner)?;
    assert_writable(pmlc_ai)?;
    assert_writable(market_nonce_ai)?;
    assert_writable(long_margin_ai)?;
    assert_writable(short_margin_ai)?;

    if program_config_ai.owner != program_id
        || instrument_ai.owner != program_id
        || pmlc_ai.owner != program_id
        || market_nonce_ai.owner != program_id
        || long_margin_ai.owner != program_id
        || short_margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if sysvar_ix_ai.key != &sysvar_instructions::id() {
        return Err(solana_program::program_error::ProgramError::InvalidAccountData);
    }

    // Config snapshot.
    let attestation_signer = {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.global_pause != 0 {
            return Err(PolyleverageError::GlobalPaused.into());
        }
        cfg.attestation_signer
    };
    let (market_id, twap_window_slots, max_staleness_secs) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        (inst.market_id, inst.twap_window_slots, inst.max_staleness_secs)
    };

    // Verify CRE price attestation.
    let att_bytes = attestation::verify_via_ed25519_sysvar(
        sysvar_ix_ai,
        &attestation_signer,
        args.attestation_ix_offset as usize,
    )?;
    let att = Attestation::parse(&att_bytes)?;
    if att.attestation_type() != ATT_TYPE_PRICE_TWAP {
        return Err(PolyleverageError::AttestationWrongType.into());
    }
    if att.twap_window_slots()? != twap_window_slots {
        return Err(PolyleverageError::AttestationWrongType.into());
    }
    let now_unix = Clock::get()?.unix_timestamp.max(0) as u64;

    {
        let _ = assert_pda(&[SEED_MARKET_NONCE, &market_id], program_id, market_nonce_ai.key)?;
        let mut data = market_nonce_ai.try_borrow_mut_data()?;
        let n = MarketNonceAccount::load_mut(&mut data)?;
        attestation::validate_freshness_and_nonce(
            &att,
            &market_id,
            n.last_accepted_cre_nonce,
            now_unix,
            max_staleness_secs,
        )?;
        n.last_accepted_cre_nonce = att.nonce();
        n.last_accepted_twap_fp = att.price_fp()?;
        n.last_accepted_ts = now_unix;
    }

    let mark_fp = att.price_fp()?;

    let (entry_fp, size_fp, collateral_per_side, expected_long_owner, expected_short_owner) = {
        let data = pmlc_ai.try_borrow_data()?;
        let p = Pmlc::load(&data)?;
        p.require_live()?;
        if p.market_id != market_id {
            return Err(PolyleverageError::InvalidPda.into());
        }
        (
            p.entry_price_fp,
            p.size_fp(),
            p.collateral_per_side,
            p.long_owner,
            p.short_owner,
        )
    };
    if expected_long_owner != *long_owner.key || expected_short_owner != *short_owner.key {
        return Err(PolyleverageError::InvalidPmlcSide.into());
    }

    // PnL (symmetric). Compute once, derive symmetric equities.
    let pnl_long = math::pnl_long_atoms(entry_fp, mark_fp, size_fp)?;
    let eq_long = math::bounded_equity(collateral_per_side, pnl_long);
    let two_c = collateral_per_side.saturating_mul(2);
    let eq_short = two_c.saturating_sub(eq_long);

    // Validate margin accounts.
    {
        let data = long_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != *long_owner.key {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }
    {
        let data = short_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != *short_owner.key {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    // Apply settlement. No fees, no bounty — mutual.
    {
        let mut ld = long_margin_ai.try_borrow_mut_data()?;
        let lm = MarginAccount::load_mut(&mut ld)?;
        lm.debit_locked(collateral_per_side)?;
        lm.credit_free(eq_long)?;
        lm.realized_pnl = lm.realized_pnl.saturating_add(eq_long as i64 - collateral_per_side as i64);

        let mut sd = short_margin_ai.try_borrow_mut_data()?;
        let sm = MarginAccount::load_mut(&mut sd)?;
        sm.debit_locked(collateral_per_side)?;
        sm.credit_free(eq_short)?;
        sm.realized_pnl = sm.realized_pnl.saturating_add(eq_short as i64 - collateral_per_side as i64);
    }

    {
        let mut data = pmlc_ai.try_borrow_mut_data()?;
        let p = Pmlc::load_mut(&mut data)?;
        p.status = PMLC_STATUS_CLOSED;
        p.close_reason = CLOSE_REASON_MUTUAL;
        p.last_mark_slot = Clock::get()?.slot;
    }

    msg!("close_mutual mark={} eq_long={} eq_short={}", mark_fp, eq_long, eq_short);
    Ok(())
}

// ============================================================================
// PruneExpired
// ============================================================================

/// Accounts:
///   0. `[signer, writable]` caller (receives bounty if we credit; v1 no bounty)
///   1. `[]` instrument config
///   2. `[writable]` intent book
///   3+. `[writable]` owner margin accounts — one per distinct intent owner pruned.
///       Caller must supply these in the same order as the intents they correspond to.
///       Intent-to-margin-account pairing is done by iterating the book and matching
///       each expired intent's seat.trader to the caller-supplied margin accounts by
///       position. For v1 simplicity we require one margin account per pruned intent
///       (duplicates allowed if same owner has multiple expired).
pub fn process_prune_expired(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let caller = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;

    assert_signer(caller)?;
    assert_writable(book_ai)?;
    if instrument_ai.owner != program_id || book_ai.owner != program_id {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    let (collateral_mint, instrument_book) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        (inst.collateral_mint, inst.intent_book)
    };
    if instrument_book != *book_ai.key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    // Collect remaining margin accounts.
    let mut remaining_margins: Vec<&AccountInfo> = Vec::new();
    for acc in iter.by_ref() {
        remaining_margins.push(acc);
    }

    let now = Clock::get()?.slot;

    // We need to: find expired intents, for each, match to provided margin account,
    // release reservation, remove from tree, free node. For v1 we do at most
    // `remaining_margins.len()` prunings per call.
    let mut margin_cursor = 0usize;
    let mut pruned_count: u32 = 0;

    // Collect expired intent (side, node_idx, owner, collateral + fee_buffer) while
    // holding a temporary mutable view of the book, then apply updates.
    let mut work: Vec<(u8, u32, Pubkey, u64)> = Vec::new();
    {
        let mut book_data = book_ai.try_borrow_mut_data()?;
        let book = BookMut::load(&mut book_data)?;
        for (i, slot) in book.nodes.iter().enumerate() {
            if slot.tag() != NODE_TAG_INTENT {
                continue;
            }
            if work.len() >= remaining_margins.len() {
                break;
            }
            let node: &IntentNode = bytemuck::from_bytes(&slot.bytes);
            if node.contracts_remaining == 0 {
                continue;
            }
            if node.expiration_slot >= now {
                continue;
            }
            // Resolve owner via seat.
            let seat_idx = node.owner_seat;
            let trader = book.seat(seat_idx)?.trader;
            let total_release = node
                .reserved_collateral
                .checked_add(node.fee_buffer)
                .ok_or(PolyleverageError::ArithmeticOverflow)?;
            work.push((node.side, i as u32, trader, total_release));
        }
    }

    // For each expired intent, find the matching margin account (by owner) among remaining,
    // release reservation, then remove from tree.
    for (side, node_idx, trader, total_release) in work.iter() {
        // Find margin account for this trader.
        let mut idx: Option<usize> = None;
        for (j, m_ai) in remaining_margins.iter().enumerate().skip(margin_cursor) {
            if m_ai.owner != program_id {
                return Err(PolyleverageError::InvalidAccountOwner.into());
            }
            let data = m_ai.try_borrow_data()?;
            let m = MarginAccount::load(&data)?;
            if m.owner == *trader && m.collateral_mint == collateral_mint {
                idx = Some(j);
                break;
            }
        }
        let midx = idx.ok_or(PolyleverageError::InvalidPda)?;
        let m_ai = remaining_margins[midx];
        margin_cursor = midx + 1;

        {
            let mut data = m_ai.try_borrow_mut_data()?;
            let m = MarginAccount::load_mut(&mut data)?;
            m.move_reserved_to_free(*total_release)?;
        }

        // Remove from tree + free node + decrement seat counter.
        {
            let mut book_data = book_ai.try_borrow_mut_data()?;
            let mut book = BookMut::load(&mut book_data)?;
            let seat_idx = book.intent(*node_idx)?.owner_seat;
            intent_tree::remove(&mut book, *side, *node_idx)?;
            book.free_node(*node_idx)?;
            let seat = book.seat_mut(seat_idx)?;
            seat.active_intent_count = seat.active_intent_count.saturating_sub(1);
        }

        pruned_count += 1;
    }

    msg!("prune_expired pruned={}", pruned_count);
    let _ = (SIDE_LONG, SIDE_SHORT); // silence if both constants unused
    Ok(())
}
