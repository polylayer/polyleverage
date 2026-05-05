//! Match-with-substitution.
//!
//! Two variants:
//!
//! - `MatchSubstitute` — plain substitution. Alice exits with her original
//!   `c` collateral. No on-chain PnL transfer; any compensation between Alice
//!   and Carol happens off-chain.
//!
//! - `MatchSubstituteWithSettle` — **on-chain PnL settlement**. Carol pays
//!   Alice a premium equal to Alice's mark-to-market PnL at the match midpoint.
//!   Bob (the untouched counterparty) is not affected. Carol's on-chain
//!   position inherits PMLC's entry `e1`; the premium makes her *economic*
//!   entry equal to the match midpoint `e2`. Bilateral invariant `eq_long +
//!   eq_short = 2c` is preserved because only owners change on-chain; no
//!   collateral movement affects Bob's side.
//!
//! Consent model (both variants):
//! - Substitutor signs by calling the ix.
//! - Counterparty's consent is captured by requiring `PMLC.entry ∈ overlap` of
//!   both intents' ranges.

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
    instruction::MatchPairArgs,
    math,
    processor::match_ix::release_fee_buffer_for_fill,
    seeds::SEED_MARGIN,
    state::{
        intent_tree, BookMut, InstrumentConfig, IntentNode, MarginAccount, Pmlc, NODE_TAG_INTENT,
        SIDE_LONG, SIDE_SHORT,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Plain substitution: Alice exits with `c`, no on-chain PnL.
pub fn process_match_substitute(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: MatchPairArgs,
) -> ProgramResult {
    do_substitute(program_id, accounts, args, /* with_settle */ false)
}

/// Substitution with on-chain PnL settlement.
pub fn process_match_substitute_with_settle(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: MatchPairArgs,
) -> ProgramResult {
    do_substitute(program_id, accounts, args, /* with_settle */ true)
}

// ---------------------------------------------------------------------------
// Shared body
// ---------------------------------------------------------------------------

/// Accounts (both variants):
///   0. `[signer]` substitutor
///   1. `[]` instrument config
///   2. `[writable]` intent book
///   3. `[writable]` existing PMLC
///   4. `[writable]` substitutor's margin
///   5. `[writable]` counterparty's margin
fn do_substitute(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: MatchPairArgs,
    with_settle: bool,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let substitutor = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;
    let substitutor_margin_ai = next_account_info(iter)?;
    let counterparty_margin_ai = next_account_info(iter)?;

    assert_signer(substitutor)?;
    assert_writable(book_ai)?;
    assert_writable(pmlc_ai)?;
    assert_writable(substitutor_margin_ai)?;
    assert_writable(counterparty_margin_ai)?;
    if instrument_ai.owner != program_id
        || book_ai.owner != program_id
        || pmlc_ai.owner != program_id
        || substitutor_margin_ai.owner != program_id
        || counterparty_margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Instrument snapshot.
    let (instrument_key, collateral_mint, collateral_bucket, book_key) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        inst.require_active()?;
        (
            *instrument_ai.key,
            inst.collateral_mint,
            inst.collateral_bucket,
            inst.intent_book,
        )
    };
    if book_key != *book_ai.key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    // PMLC snapshot.
    let (
        pmlc_entry_fp,
        pmlc_size_fp,
        pmlc_long_owner,
        pmlc_short_owner,
        pmlc_collateral_per_side,
        pmlc_instrument,
    ) = {
        let data = pmlc_ai.try_borrow_data()?;
        let p = Pmlc::load(&data)?;
        p.require_live()?;
        (
            p.entry_price_fp,
            p.size_fp(),
            p.long_owner,
            p.short_owner,
            p.collateral_per_side,
            p.instrument,
        )
    };
    if pmlc_instrument != instrument_key {
        return Err(PolyleverageError::InvalidPda.into());
    }
    if pmlc_collateral_per_side != collateral_bucket {
        return Err(PolyleverageError::InvalidPda.into());
    }

    // --- Resolve intents + validate substitution ---
    let ctx = {
        let mut data = book_ai.try_borrow_mut_data()?;
        let mut book = BookMut::load(&mut data)?;

        let (long_idx, long_side) = find_intent_by_id(&book, args.long_intent_id)?;
        let (short_idx, short_side) = find_intent_by_id(&book, args.short_intent_id)?;
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

        // Require overlap to contain the PMLC entry (Carol's implicit consent).
        let (overlap_min, overlap_max) = math::range_overlap(
            long.min_price_fp,
            long.max_price_fp,
            short.min_price_fp,
            short.max_price_fp,
        )
        .ok_or(PolyleverageError::NoRangeOverlap)?;
        if pmlc_entry_fp < overlap_min || pmlc_entry_fp > overlap_max {
            return Err(PolyleverageError::NoRangeOverlap.into());
        }

        let match_midpoint = math::overlap_midpoint(overlap_min, overlap_max);

        let long_trader = book.seat(long.owner_seat)?.trader;
        let short_trader = book.seat(short.owner_seat)?.trader;

        let substitutor_is_long = long_trader == *substitutor.key;
        let substitutor_is_short = short_trader == *substitutor.key;
        if !substitutor_is_long && !substitutor_is_short {
            return Err(PolyleverageError::MissingSigner.into());
        }

        // Substitutor's NEW intent opposite side → they own the SAME-named side on PMLC.
        // I.e., if their new intent is long, they already own the short of PMLC (they're
        // exiting a short by posting a long buyback)... wait, that's wrong.
        //
        // Correction: the substitutor is EXITING a position. If they own LONG on PMLC
        // and want to exit, they post a SHORT intent to net out. So substitutor_is_short
        // (their NEW intent is short) ⇒ they own LONG of PMLC.
        let substitutor_pmlc_side = if substitutor_is_short {
            SIDE_LONG
        } else {
            SIDE_SHORT
        };
        let expected_pmlc_owner = match substitutor_pmlc_side {
            SIDE_LONG => pmlc_long_owner,
            SIDE_SHORT => pmlc_short_owner,
            _ => return Err(PolyleverageError::InvalidPmlcSide.into()),
        };
        if expected_pmlc_owner != *substitutor.key {
            return Err(PolyleverageError::InvalidPmlcSide.into());
        }

        // New PMLC owner is the counterparty.
        let new_pmlc_owner = if substitutor_is_long {
            short_trader
        } else {
            long_trader
        };

        // Consume one contract from each intent.
        let per_contract_reserve = collateral_bucket;
        let mut long = long;
        let mut short = short;
        let long_remaining_before = long.contracts_remaining;
        let short_remaining_before = short.contracts_remaining;
        long.contracts_remaining = long.contracts_remaining.saturating_sub(1);
        short.contracts_remaining = short.contracts_remaining.saturating_sub(1);
        long.reserved_collateral = long
            .reserved_collateral
            .checked_sub(per_contract_reserve)
            .ok_or(PolyleverageError::InsufficientReservedCollateral)?;
        short.reserved_collateral = short
            .reserved_collateral
            .checked_sub(per_contract_reserve)
            .ok_or(PolyleverageError::InsufficientReservedCollateral)?;

        let fee_buf_release_long = if long_remaining_before == 1 {
            long.fee_buffer
        } else {
            release_fee_buffer_for_fill(long.fee_buffer, long_remaining_before, 1)?
        };
        let fee_buf_release_short = if short_remaining_before == 1 {
            short.fee_buffer
        } else {
            release_fee_buffer_for_fill(short.fee_buffer, short_remaining_before, 1)?
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

        SubstitutionCtx {
            substitutor_pmlc_side,
            new_pmlc_owner,
            per_contract_collateral: per_contract_reserve,
            per_contract_fee_buf_long: fee_buf_release_long,
            per_contract_fee_buf_short: fee_buf_release_short,
            substitutor_is_long,
            match_midpoint,
        }
    };

    // --- Compute PnL to settle (signed, from substitutor's perspective) ---
    //
    // Substitutor exits at `match_midpoint`. PnL = (midpoint - entry) × size for long,
    // negated for short.
    let delta_atoms: i128 = if with_settle {
        let pnl_long = math::pnl_long_atoms(pmlc_entry_fp, ctx.match_midpoint, pmlc_size_fp)?;
        match ctx.substitutor_pmlc_side {
            SIDE_LONG => pnl_long,
            SIDE_SHORT => -pnl_long,
            _ => return Err(PolyleverageError::InvalidPmlcSide.into()),
        }
    } else {
        0
    };

    // Bounded-win: |delta| must be ≤ c (otherwise opponent is already liquidatable).
    if with_settle {
        let abs_delta = delta_atoms.unsigned_abs();
        if abs_delta > collateral_bucket as u128 {
            return Err(PolyleverageError::NotLiquidatable.into());
        }
    }

    // --- Validate margin PDAs ---
    let _ = assert_pda(
        &[
            SEED_MARGIN,
            substitutor.key.as_ref(),
            collateral_mint.as_ref(),
        ],
        program_id,
        substitutor_margin_ai.key,
    )?;
    let _ = assert_pda(
        &[
            SEED_MARGIN,
            ctx.new_pmlc_owner.as_ref(),
            collateral_mint.as_ref(),
        ],
        program_id,
        counterparty_margin_ai.key,
    )?;

    // --- Apply balance movements ---
    //
    // Substitutor:
    //   - Release their intent's per-contract reservation (collateral + fee buffer).
    //   - Unlock their PMLC-side c back to free.
    //   - If with_settle and delta > 0: receive delta from counterparty.
    //   - If with_settle and delta < 0: pay |delta| to counterparty.
    //
    // Counterparty:
    //   - Release their intent's fee buffer (no fee charged on substitution).
    //   - Move reserved c → locked (takes over the PMLC side).
    //   - If with_settle and delta > 0: pay delta to substitutor.
    //   - If with_settle and delta < 0: receive |delta| from substitutor.
    //
    // We use i128 for delta and apply it symmetrically.
    {
        let mut data = substitutor_margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        if m.owner != *substitutor.key || m.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
        m.move_reserved_to_free(ctx.per_contract_collateral)?;
        let sub_fee_buf = if ctx.substitutor_is_long {
            ctx.per_contract_fee_buf_long
        } else {
            ctx.per_contract_fee_buf_short
        };
        if sub_fee_buf > 0 {
            m.move_reserved_to_free(sub_fee_buf)?;
        }
        m.move_locked_to_free(ctx.per_contract_collateral)?;

        // Apply signed delta to substitutor's free.
        if delta_atoms > 0 {
            m.credit_free(delta_atoms as u64)?;
            m.realized_pnl = m.realized_pnl.saturating_add(delta_atoms as i64);
        } else if delta_atoms < 0 {
            let amount = (-delta_atoms) as u64;
            m.debit_free(amount)?;
            m.realized_pnl = m.realized_pnl.saturating_sub(amount as i64);
        }
    }
    {
        let mut data = counterparty_margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        if m.owner != ctx.new_pmlc_owner || m.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
        m.move_reserved_to_locked(ctx.per_contract_collateral)?;
        let cp_fee_buf = if ctx.substitutor_is_long {
            ctx.per_contract_fee_buf_short
        } else {
            ctx.per_contract_fee_buf_long
        };
        if cp_fee_buf > 0 {
            m.move_reserved_to_free(cp_fee_buf)?;
        }

        // Counterparty applies -delta (opposite of substitutor's).
        if delta_atoms > 0 {
            m.debit_free(delta_atoms as u64)?;
            // Carol's "realized PnL" isn't really realized — she paid for
            // Alice's in-the-money position. We don't touch her realized_pnl
            // counter because her future mark-to-market will absorb this.
        } else if delta_atoms < 0 {
            let amount = (-delta_atoms) as u64;
            m.credit_free(amount)?;
        }
    }

    // --- Transfer PMLC ownership ---
    {
        let mut data = pmlc_ai.try_borrow_mut_data()?;
        let p = Pmlc::load_mut(&mut data)?;
        match ctx.substitutor_pmlc_side {
            SIDE_LONG => p.long_owner = ctx.new_pmlc_owner,
            SIDE_SHORT => p.short_owner = ctx.new_pmlc_owner,
            _ => return Err(PolyleverageError::InvalidPmlcSide.into()),
        }
        p.last_mark_slot = Clock::get()?.slot;
    }

    msg!(
        "substitute{} side={} entry_fp={} midpoint={} delta={} new_owner={}",
        if with_settle { "_with_settle" } else { "" },
        ctx.substitutor_pmlc_side,
        pmlc_entry_fp,
        ctx.match_midpoint,
        delta_atoms,
        ctx.new_pmlc_owner,
    );
    Ok(())
}

struct SubstitutionCtx {
    substitutor_pmlc_side: u8,
    new_pmlc_owner: Pubkey,
    per_contract_collateral: u64,
    per_contract_fee_buf_long: u64,
    per_contract_fee_buf_short: u64,
    substitutor_is_long: bool,
    match_midpoint: u64,
}

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
