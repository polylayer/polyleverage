//! Settlement handlers: Liquidate, Resolve, CloseMutual.
//!
//! All settlement paths require an attestation signed by
//! `ProgramConfig.attestation_signer`, verified via Ed25519 sysvar
//! introspection. Bounded-win math applies: loser loses full collateral,
//! winner gets 2× collateral (minus optional keeper bounty).
//!
//! `Liquidate` consumes a HISTORICAL_LIQUIDATION attestation that binds
//! a specific PMLC pubkey + the historical TWAP price + unix ts at which
//! the position first became liquidatable. Settlement uses the historical
//! mark — submission timing is irrelevant. (See spec §6 / §22.)

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::{instructions as sysvar_instructions, Sysvar},
};

use crate::{
    attestation::{
        self, Attestation, ATT_TYPE_HISTORICAL_LIQUIDATION, ATT_TYPE_RESOLUTION,
    },
    error::PolyleverageError,
    instruction::{LiquidateArgs, ResolveArgs},
    math,
    seeds::{SEED_MARGIN, SEED_MARKET_NONCE},
    state::{
        InstrumentConfig, MarginAccount, MarketNonceAccount, Pmlc, ProgramConfig,
        CLOSE_REASON_LIQUIDATED, CLOSE_REASON_RESOLVED,
        PMLC_STATUS_LIQUIDATED, PMLC_STATUS_RESOLVED,
    },
    utils::{assert_pda, assert_signer, assert_writable},
};

/// Accounts:
///   0. `[signer, writable]` keeper (receives bounty via keeper_margin)
///   1. `[]` program config
///   2. `[]` instructions sysvar
///   3. `[]` instrument config
///   4. `[writable]` pmlc
///   5. `[writable]` market nonce
///   6. `[writable]` long margin account
///   7. `[writable]` short margin account
///   8. `[writable]` keeper margin account (seeds `[SEED_MARGIN, keeper, collateral_mint]`)
pub fn process_liquidate(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: LiquidateArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let keeper = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let sysvar_ix_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;
    let market_nonce_ai = next_account_info(iter)?;
    let long_margin_ai = next_account_info(iter)?;
    let short_margin_ai = next_account_info(iter)?;
    let keeper_margin_ai = next_account_info(iter)?;

    assert_signer(keeper)?;
    assert_writable(pmlc_ai)?;
    assert_writable(market_nonce_ai)?;
    assert_writable(long_margin_ai)?;
    assert_writable(short_margin_ai)?;
    assert_writable(keeper_margin_ai)?;

    if program_config_ai.owner != program_id
        || instrument_ai.owner != program_id
        || pmlc_ai.owner != program_id
        || market_nonce_ai.owner != program_id
        || long_margin_ai.owner != program_id
        || short_margin_ai.owner != program_id
        || keeper_margin_ai.owner != program_id
    {
        return Err(PolyleverageError::InvalidAccountOwner.into());
    }
    if sysvar_ix_ai.key != &sysvar_instructions::id() {
        return Err(ProgramError::InvalidAccountData);
    }

    // Snapshot config.
    let (attestation_signer, _default_staleness) = {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.global_pause != 0 {
            return Err(PolyleverageError::GlobalPaused.into());
        }
        (cfg.attestation_signer, cfg.default_max_staleness_secs)
    };

    // Snapshot instrument config (twap_window_slots no longer needed on-chain
    // for liquidation — the TEE attestor uses it off-chain to query Polymarket
    // history at the right granularity, and we trust the result).
    let (
        market_id,
        liquidation_bps,
        liquidation_bounty_bps,
        max_staleness_secs,
        collateral_mint,
    ) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        (
            inst.market_id,
            inst.liquidation_bps,
            inst.liquidation_bounty_bps,
            inst.max_staleness_secs,
            inst.collateral_mint,
        )
    };

    // Keeper margin must be the PDA for (keeper, collateral_mint). This is where
    // the liquidation bounty is credited.
    let _keeper_margin_bump = assert_pda(
        &[SEED_MARGIN, keeper.key.as_ref(), collateral_mint.as_ref()],
        program_id,
        keeper_margin_ai.key,
    )?;
    {
        let data = keeper_margin_ai.try_borrow_data()?;
        let km = MarginAccount::load(&data)?;
        if km.owner != *keeper.key || km.collateral_mint != collateral_mint {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    // Verify the wrapping signature came from the registered attestor.
    let attestation_bytes = attestation::verify_via_ed25519_sysvar(
        sysvar_ix_ai,
        &attestation_signer,
        args.attestation_ix_offset as usize,
    )?;
    let att = Attestation::parse(&attestation_bytes)?;

    // Liquidate consumes ATT_TYPE_HISTORICAL_LIQUIDATION specifically. The
    // attestation must bind to *this* PMLC by pubkey, so the same mark price
    // cannot be replayed against a different position.
    if att.attestation_type() != ATT_TYPE_HISTORICAL_LIQUIDATION {
        return Err(PolyleverageError::AttestationWrongType.into());
    }
    if att.breach_pmlc_pubkey()? != pmlc_ai.key.to_bytes() {
        return Err(PolyleverageError::AttestationWrongPmlc.into());
    }
    let now_unix = Clock::get()?.unix_timestamp.max(0) as u64;
    // Sanity: breach must have happened in the past (cannot be future-dated).
    let breach_ts = att.breach_unix_ts()?;
    if breach_ts < 0 || (breach_ts as u64) > now_unix {
        return Err(PolyleverageError::AttestationStale.into());
    }
    // The wrapper signature timestamp + nonce still apply (attestor must have
    // SIGNED recently even if the breach itself is historical). This is what
    // bounds replay of an old signed attestation.
    {
        let expected_bump =
            assert_pda(&[SEED_MARKET_NONCE, &market_id], program_id, market_nonce_ai.key)?;
        let _ = expected_bump;
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
        n.last_accepted_ts = now_unix;
        n.last_accepted_twap_fp = att.breach_mark_fp()?;
    }

    // Settle at the historical breach price — not the current mark.
    let mark_fp = att.breach_mark_fp()?;

    // Load PMLC.
    let (entry_fp, size_fp, collateral_per_side, long_owner, short_owner) = {
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

    // PnL and equity (bounded).
    let pnl_long = math::pnl_long_atoms(entry_fp, mark_fp, size_fp)?;
    let eq_long = math::bounded_equity(collateral_per_side, pnl_long);
    // Keep bilateral sum == 2c exactly:
    let two_c = collateral_per_side.saturating_mul(2);
    let eq_short = two_c.saturating_sub(eq_long);

    // Liquidation threshold (bounded-win: threshold == 0 → only fires on exact 0 equity).
    let threshold = (collateral_per_side as u128 * liquidation_bps as u128 / math::BPS_DENOM as u128) as u64;
    let min_equity = eq_long.min(eq_short);
    if min_equity > threshold {
        return Err(PolyleverageError::NotLiquidatable.into());
    }

    // Determine winner/loser. Bounded-win: winner gets 2c - bounty - fee; loser gets 0.
    let long_wins = eq_long > eq_short;
    let bounty = (collateral_per_side as u128 * liquidation_bounty_bps as u128 / math::BPS_DENOM as u128) as u64;

    // Validate margin PDAs match PMLC owners.
    {
        let data = long_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != long_owner {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }
    {
        let data = short_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != short_owner {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    // Apply collateral movement.
    {
        let mut long_data = long_margin_ai.try_borrow_mut_data()?;
        let long_margin = MarginAccount::load_mut(&mut long_data)?;
        let mut short_data = short_margin_ai.try_borrow_mut_data()?;
        let short_margin = MarginAccount::load_mut(&mut short_data)?;

        if long_wins {
            // Long: locked → free, credit 2c - bounty - short's locked.
            // Actually: long.locked = -c; short.locked = -c;
            //            long.free += (2c - bounty)
            //            short gets 0 (loser)
            long_margin.debit_locked(collateral_per_side)?;
            short_margin.debit_locked(collateral_per_side)?;
            long_margin.credit_free(two_c.saturating_sub(bounty))?;
            // bounty is paid into the keeper's MarginAccount below — see the
            // `if bounty > 0` block after this match arm.
            // Loser's realized PnL for display:
            short_margin.realized_pnl = short_margin
                .realized_pnl
                .saturating_sub(collateral_per_side as i64);
            long_margin.realized_pnl = long_margin
                .realized_pnl
                .saturating_add((collateral_per_side.saturating_sub(bounty)) as i64);
        } else {
            long_margin.debit_locked(collateral_per_side)?;
            short_margin.debit_locked(collateral_per_side)?;
            short_margin.credit_free(two_c.saturating_sub(bounty))?;
            long_margin.realized_pnl = long_margin
                .realized_pnl
                .saturating_sub(collateral_per_side as i64);
            short_margin.realized_pnl = short_margin
                .realized_pnl
                .saturating_add((collateral_per_side.saturating_sub(bounty)) as i64);
        }
    }

    // Mark PMLC closed.
    {
        let mut data = pmlc_ai.try_borrow_mut_data()?;
        let p = Pmlc::load_mut(&mut data)?;
        p.status = PMLC_STATUS_LIQUIDATED;
        p.close_reason = CLOSE_REASON_LIQUIDATED;
        p.last_mark_slot = Clock::get()?.slot;
    }

    // Pay keeper bounty into their MarginAccount.free_collateral.
    // Sum across winner, loser, keeper remains exactly 2c (winner credited 2c-bounty,
    // loser credited 0, keeper credited bounty).
    if bounty > 0 {
        let mut data = keeper_margin_ai.try_borrow_mut_data()?;
        let km = MarginAccount::load_mut(&mut data)?;
        km.credit_free(bounty)?;
    }

    msg!(
        "liquidate pmlc entry={} mark={} eq_long={} eq_short={} long_wins={} bounty={}",
        entry_fp,
        mark_fp,
        eq_long,
        eq_short,
        long_wins,
        bounty
    );

    Ok(())
}

/// Accounts: same as Liquidate. Attestation must be RESOLUTION type.
pub fn process_resolve(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: ResolveArgs,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let caller = next_account_info(iter)?;
    let program_config_ai = next_account_info(iter)?;
    let sysvar_ix_ai = next_account_info(iter)?;
    let instrument_ai = next_account_info(iter)?;
    let pmlc_ai = next_account_info(iter)?;
    let market_nonce_ai = next_account_info(iter)?;
    let long_margin_ai = next_account_info(iter)?;
    let short_margin_ai = next_account_info(iter)?;

    assert_signer(caller)?;
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

    let (attestation_signer, _default_staleness) = {
        let data = program_config_ai.try_borrow_data()?;
        let cfg = ProgramConfig::load(&data)?;
        if cfg.global_pause != 0 {
            return Err(PolyleverageError::GlobalPaused.into());
        }
        (cfg.attestation_signer, cfg.default_max_staleness_secs)
    };

    let (market_id, max_staleness_secs) = {
        let data = instrument_ai.try_borrow_data()?;
        let inst = InstrumentConfig::load(&data)?;
        (inst.market_id, inst.max_staleness_secs)
    };

    // Verify attestation.
    let attestation_bytes = attestation::verify_via_ed25519_sysvar(
        sysvar_ix_ai,
        &attestation_signer,
        args.attestation_ix_offset as usize,
    )?;
    let att = Attestation::parse(&attestation_bytes)?;
    if att.attestation_type() != ATT_TYPE_RESOLUTION {
        return Err(PolyleverageError::AttestationWrongType.into());
    }

    let now_unix = Clock::get()?.unix_timestamp.max(0) as u64;
    {
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
        n.last_accepted_ts = now_unix;
        n.final_outcome_bps = att.final_outcome_bps()?;
        n.is_resolved = 1;
    }

    // Compute final mark from outcome bps. 10000 → 1.0; 0 → 0; 5000 → 0.5.
    let final_fp: u64 = match att.final_outcome_bps()? {
        0 => 0,
        5000 => math::PRICE_ONE / 2,
        10000 => math::PRICE_ONE.saturating_sub(1), // PRICE_ONE itself invalid; cap slightly below
        _ => return Err(PolyleverageError::InvalidInstructionData.into()),
    };

    // Resolution's mark can be 0 or PRICE_ONE - 1; we allow those as settlement terminal values.
    let (entry_fp, size_fp, collateral_per_side, long_owner, short_owner) = {
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

    // Compute PnL using bounded math. Special case: final_fp == 0 or near-1 yields extreme PnL.
    // Bounded equity caps long/short equity at [0, 2c] regardless.
    // For exactness at bilateral sum = 2c, compute eq_long, derive eq_short.
    let pnl_long: i128 = if att.final_outcome_bps()? == 0 {
        -(collateral_per_side as i128).saturating_mul(2) // loser, clamped
    } else if att.final_outcome_bps()? == 10_000 {
        (collateral_per_side as i128).saturating_mul(2) // winner
    } else {
        // 5000: approximate neutral — compute normally
        math::pnl_long_atoms(entry_fp, final_fp, size_fp)?
    };
    let eq_long = math::bounded_equity(collateral_per_side, pnl_long);
    let two_c = collateral_per_side.saturating_mul(2);
    let eq_short = two_c.saturating_sub(eq_long);

    // Validate margin accounts.
    {
        let data = long_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != long_owner {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }
    {
        let data = short_margin_ai.try_borrow_data()?;
        let m = MarginAccount::load(&data)?;
        if m.owner != short_owner {
            return Err(PolyleverageError::InvalidPda.into());
        }
    }

    // Apply.
    {
        let mut long_data = long_margin_ai.try_borrow_mut_data()?;
        let lm = MarginAccount::load_mut(&mut long_data)?;
        lm.debit_locked(collateral_per_side)?;
        lm.credit_free(eq_long)?;
        lm.realized_pnl = lm
            .realized_pnl
            .saturating_add(eq_long as i64 - collateral_per_side as i64);

        let mut short_data = short_margin_ai.try_borrow_mut_data()?;
        let sm = MarginAccount::load_mut(&mut short_data)?;
        sm.debit_locked(collateral_per_side)?;
        sm.credit_free(eq_short)?;
        sm.realized_pnl = sm
            .realized_pnl
            .saturating_add(eq_short as i64 - collateral_per_side as i64);
    }

    {
        let mut data = pmlc_ai.try_borrow_mut_data()?;
        let p = Pmlc::load_mut(&mut data)?;
        p.status = PMLC_STATUS_RESOLVED;
        p.close_reason = CLOSE_REASON_RESOLVED;
        p.last_mark_slot = Clock::get()?.slot;
    }

    msg!(
        "resolve pmlc mark={} eq_long={} eq_short={}",
        final_fp,
        eq_long,
        eq_short
    );
    Ok(())
}
