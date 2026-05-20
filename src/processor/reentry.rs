//! Re-entry: after a PMLC closes (Liquidate / Resolve), the winning side may
//! re-post an intent using their credited free_collateral.
//!
//! This is split out of settlement to keep those hot paths lean. The user (or
//! their keeper) calls `ClaimReentry` once per closed PMLC side. The PMLC's
//! reentry flag for that side is cleared on success so the same close can't be
//! harvested twice.
//!
//! Accounts:
//!   0. `[signer]` owner (must match the side-owner on the PMLC)
//!   1. `[]` program config
//!   2. `[writable]` pmlc (closed)
//!   3. `[]` instrument config
//!   4. `[writable]` intent book
//!   5. `[writable]` margin account (owner, instrument.collateral_mint)

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    pubkey::Pubkey,
    sysvar::Sysvar,
};

use crate::{
    error::PolyleverageError,
    instruction::ClaimReentryArgs,
    math,
    processor::intent::fee_buffer_for,
    seeds::SEED_MARGIN,
    state::{
        intent_tree, seat_tree, BookMut, InstrumentConfig, IntentNode, MarginAccount, Pmlc,
        ProgramConfig, INTENT_FLAG_REENTRY, NODE_TAG_INTENT, NULL_IDX, PMLC_FLAG_LONG_REENTRY,
        PMLC_FLAG_SHORT_REENTRY, RB_RED, SIDE_LONG, SIDE_SHORT,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

pub fn process_claim_reentry(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: ClaimReentryArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let book_ai = next_account_info(iter)?;
    let margin_ai = next_account_info(iter)?;

    assert_signer(owner)?;
    assert_writable(pmlc_ai)?;
    assert_writable(book_ai)?;
    assert_writable(margin_ai)?;
    if program_config_ai.owner != program_id
        || pmlc_ai.owner != program_id
        || instrument_ai.owner != program_id
        || book_ai.owner != program_id
        || margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    // Global pause blocks reentry (we'd be creating new intents).
    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.global_pause != 0 {
            return Err(PolyleverageError::GlobalPaused.into());
        }
    }

    // Validate side.
    if args.side != SIDE_LONG && args.side != SIDE_SHORT {
        return Err(PolyleverageError::InvalidInstructionData.into());
    }

    // --- Load + validate PMLC ---
    let (required_owner, reentry_price_fp, reentry_flag_mask, instrument_key, collateral_mint) = {
        let data = pmlc_ai.try_borrow_data()?;
        let p = Pmlc::load(&data)?;
        // Must be closed; live PMLCs can't reenter.
        if p.status == crate::state::PMLC_STATUS_LIVE {
            return Err(PolyleverageError::PmlcNotLive.into()); // misnomer; reusing existing code
        }
        let (expected_owner, price, mask) = match args.side {
            SIDE_LONG => (
                p.long_owner,
                p.long_reentry_price_fp,
                PMLC_FLAG_LONG_REENTRY,
            ),
            SIDE_SHORT => (
                p.short_owner,
                p.short_reentry_price_fp,
                PMLC_FLAG_SHORT_REENTRY,
            ),
            _ => return Err(PolyleverageError::InvalidPmlcSide.into()),
        };
        if p.flags & mask == 0 {
            // Reentry flag was never set for this side, or already consumed.
            return Err(PolyleverageError::InvalidPmlcSide.into());
        }
        (expected_owner, price, mask, p.instrument, p.collateral_mint)
    };
    if required_owner != *owner.key {
        return Err(PolyleverageError::MissingSigner.into());
    }
    if instrument_ai.key != &instrument_key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    // --- Load instrument config ---
    let (book_key, leverage_bps, collateral_bucket, tick_fp) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        inst.require_active()?;
        if inst.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
        (
            inst.intent_book,
            inst.leverage_bps,
            inst.collateral_bucket,
            inst.tick_fp,
        )
    };
    let _ = leverage_bps;
    if book_key != *book_ai.key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    math::validate_price_ticked(reentry_price_fp, tick_fp)?;
    if args.expiration_slot <= Clock::get()?.slot {
        return Err(PolyleverageError::IntentExpired.into());
    }

    // --- Compute how many contracts we can afford ---
    let margin_bump = assert_pda(
        &[SEED_MARGIN, owner.key.as_ref(), collateral_mint.as_ref()],
        program_id,
        margin_ai.key,
    )?;
    let (free_collateral, margin_owner, margin_mint) = {
        let data = margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        (m.free_collateral, m.owner, m.collateral_mint)
    };
    if margin_owner != *owner.key || margin_mint != collateral_mint {
        return Err(PolyleverageError::InvalidPda.into());
    }
    let _ = margin_bump;

    let new_contracts_u64 = free_collateral / collateral_bucket;
    if new_contracts_u64 == 0 {
        // Clear the flag anyway so we don't leave a permanent claim surface.
        let mut pdata = pmlc_ai.try_borrow_mut_data()?;
        let p = Pmlc::load_mut(&mut pdata)?;
        p.flags &= !reentry_flag_mask;
        msg!("claim_reentry: insufficient free for any contract; flag cleared");
        return Ok(());
    }
    let mut new_contracts: u16 = new_contracts_u64.min(u16::MAX as u64) as u16;

    let mut collateral_reserve = (new_contracts as u64)
        .checked_mul(collateral_bucket)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;

    // Fee buffer at current schedule's max tier.
    let max_fee_bps = {
        let data = book_ai.try_borrow_data()?;
        let book = crate::state::BookRef::load(&data)?;
        book.header.cached_max_fee_bps
    };
    let mut fee_buffer = fee_buffer_for(new_contracts, collateral_bucket, max_fee_bps)?;
    let mut total_reserve = collateral_reserve
        .checked_add(fee_buffer)
        .ok_or(PolyleverageError::ArithmeticOverflow)?;

    if free_collateral < total_reserve {
        // Cap to what we can afford (shrink contract count one at a time).
        // This covers the case where fee_buffer pushes us over.
        // Trim until we fit.
        let mut n = new_contracts;
        while n > 0 {
            let c = n as u64 * collateral_bucket;
            let f = fee_buffer_for(n, collateral_bucket, max_fee_bps)?;
            if free_collateral >= c + f {
                break;
            }
            n -= 1;
        }
        if n == 0 {
            let mut pdata = pmlc_ai.try_borrow_mut_data()?;
            let p = Pmlc::load_mut(&mut pdata)?;
            p.flags &= !reentry_flag_mask;
            msg!("claim_reentry: fee buffer exceeded free; flag cleared");
            return Ok(());
        }
        new_contracts = n;
        collateral_reserve = (new_contracts as u64)
            .checked_mul(collateral_bucket)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        fee_buffer = fee_buffer_for(new_contracts, collateral_bucket, max_fee_bps)?;
        total_reserve = collateral_reserve
            .checked_add(fee_buffer)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
    }

    // --- Reserve + allocate intent node + insert into tree ---
    {
        let mut data = margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        m.move_free_to_reserved(total_reserve)?;
    }

    let mut book_data = book_ai.try_borrow_mut_data()?;
    let mut book = BookMut::load(&mut book_data)?;

    let seat_idx = seat_tree::find_or_create(&mut book, *owner.key)?;
    {
        let seat = book.seat_mut(seat_idx)?;
        seat.active_intent_count = seat
            .active_intent_count
            .checked_add(1)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
    }

    let intent_id = book.next_intent_id()?;
    let post_seq = book.next_seq()?;
    let node_idx = book.alloc_node()?;
    book.write_intent(
        node_idx,
        IntentNode {
            tag: NODE_TAG_INTENT,
            side: args.side,
            color: RB_RED,
            flags: INTENT_FLAG_REENTRY, // stay enrolled in rolling reentry
            left: NULL_IDX,
            right: NULL_IDX,
            parent: NULL_IDX,
            _pad0: [0; 2],
            price_fp: reentry_price_fp,
            _pad1: [0; 2],
            id: intent_id,
            owner_seat: seat_idx,
            contracts_total: new_contracts,
            contracts_remaining: new_contracts,
            expiration_slot: args.expiration_slot,
            post_seq,
            reserved_collateral: collateral_reserve,
            fee_buffer,
        },
    )?;
    intent_tree::insert(&mut book, args.side, node_idx)?;
    drop(book_data);

    // --- Clear the flag so it can't be harvested again ---
    {
        let mut pdata = pmlc_ai.try_borrow_mut_data()?;
        let p = Pmlc::load_mut(&mut pdata)?;
        p.flags &= !reentry_flag_mask;
    }

    msg!(
        "claim_reentry ok contracts={} intent_id={}",
        new_contracts,
        intent_id
    );
    Ok(())
}
