//! Novate: transfer one side of a live PMLC from its current owner to a new owner.
//!
//! Semantics (spec §4.11):
//!   - Current `{side}_owner` signs + new owner signs.
//!   - New owner must have a MarginAccount on the instrument's collateral mint
//!     with `free_collateral ≥ collateral_per_side`.
//!   - Collateral movement:
//!       old_owner.locked -= c;    old_owner.free += c;   (exits with their original collateral)
//!       new_owner.free   -= c;    new_owner.locked += c; (takes over the position)
//!   - PMLC entry price + notional + size_fp are **preserved** — new owner inherits the
//!     existing position exactly. Any PnL realized between old + new owner is a side
//!     transaction between them, off-chain.
//!
//! This is plain-vanilla novation. The "squash on opposite-side match" pattern you
//! may want later (auto-detect offsetting positions during a match and net them
//! out) is a separate v2 feature — see spec §16.

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
    instruction::NovateArgs,
    seeds::SEED_MARGIN,
    state::{InstrumentConfig, MarginAccount, Pmlc, SIDE_LONG, SIDE_SHORT},
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Accounts:
///   0. `[signer]` current owner (must own `pmlc.{side}_owner`)
///   1. `[signer]` new owner
///   2. `[]` program config (for global pause)
///   3. `[]` instrument config
///   4. `[writable]` pmlc
///   5. `[writable]` current owner's margin account
///   6. `[writable]` new owner's margin account
pub fn process_novate(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: NovateArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let current_owner = next_account_info(iter)?;
    let new_owner = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;
    let current_margin_ai = next_account_info(iter)?;
    let new_margin_ai = next_account_info(iter)?;

    assert_signer(current_owner)?;
    assert_signer(new_owner)?;
    assert_writable(pmlc_ai)?;
    assert_writable(current_margin_ai)?;
    assert_writable(new_margin_ai)?;

    if program_config_ai.owner != program_id
        || instrument_ai.owner != program_id
        || pmlc_ai.owner != program_id
        || current_margin_ai.owner != program_id
        || new_margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }

    {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = crate::state::ProgramConfig::load(&data)?;
        if cfg.global_pause != 0 {
            return Err(PolyleverageError::GlobalPaused.into());
        }
    }

    if args.side != SIDE_LONG && args.side != SIDE_SHORT {
        return Err(PolyleverageError::InvalidPmlcSide.into());
    }

    // --- Load + validate PMLC ---
    let (expected_current_owner, collateral_per_side, collateral_mint, instrument_key) = {
        let data = pmlc_ai.try_borrow_data()?;
        let p = Pmlc::load(&data)?;
        p.require_live()?;
        let current = match args.side {
            SIDE_LONG => p.long_owner,
            SIDE_SHORT => p.short_owner,
            _ => return Err(PolyleverageError::InvalidPmlcSide.into()),
        };
        (
            current,
            p.collateral_per_side,
            p.collateral_mint,
            p.instrument,
        )
    };
    if expected_current_owner != *current_owner.key {
        return Err(PolyleverageError::InvalidPmlcSide.into());
    }

    // Validate instrument + mint consistency.
    {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        if inst.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }
    if *instrument_ai.key != instrument_key {
        return Err(PolyleverageError::InvalidPda.into());
    }

    // Validate margin accounts.
    let _current_bump = assert_pda(
        &[
            SEED_MARGIN,
            current_owner.key.as_ref(),
            collateral_mint.as_ref(),
        ],
        program_id,
        current_margin_ai.key,
    )?;
    let _new_bump = assert_pda(
        &[
            SEED_MARGIN,
            new_owner.key.as_ref(),
            collateral_mint.as_ref(),
        ],
        program_id,
        new_margin_ai.key,
    )?;

    // --- Apply the transfer ---
    // Note: must borrow each account mutably in a fresh scope since they're distinct.
    {
        let mut data = current_margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        if m.owner != *current_owner.key || m.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
        m.move_locked_to_free(collateral_per_side)?;
    }
    {
        let mut data = new_margin_ai.try_borrow_mut_data()?;
        let m = MarginAccount::load_mut(&mut data)?;
        if m.owner != *new_owner.key || m.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
        // free → locked for new owner (requires sufficient free).
        m.debit_free(collateral_per_side)?;
        m.credit_locked(collateral_per_side)?;
    }

    // Swap ownership on PMLC. Preserve entry + size exactly.
    {
        let mut data = pmlc_ai.try_borrow_mut_data()?;
        let p = Pmlc::load_mut(&mut data)?;
        match args.side {
            SIDE_LONG => p.long_owner = *new_owner.key,
            SIDE_SHORT => p.short_owner = *new_owner.key,
            _ => return Err(PolyleverageError::InvalidPmlcSide.into()),
        }
        p.last_mark_slot = Clock::get()?.slot;
    }

    msg!(
        "novate ok pmlc side={} from={} to={}",
        args.side,
        current_owner.key,
        new_owner.key
    );
    Ok(())
}
